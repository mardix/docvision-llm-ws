//! Provider abstraction, streamed request bodies, SSE parsing and per-attempt accounting.

pub mod compatible;
pub mod gemini;
pub mod openai;

use crate::config::{ProviderConfig, ProviderKind, Secret};
use crate::db::{WriteOp, now_ms};
use crate::writer::Writer;
use base64::Engine;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

/// A resolved provider endpoint (config merged with per-request overrides).
#[derive(Clone, Debug)]
pub struct Endpoint {
    pub provider: String,
    pub kind: ProviderKind,
    pub base_url: String,
    pub api_key: Secret<String>,
    pub model: String,
    pub cfg: ProviderConfig,
    /// Gateway lane key: provider + model + api key fingerprint.
    pub lane_key: String,
}

impl Endpoint {
    pub fn resolve(cfg: &ProviderConfig, model: Option<&str>, base_url: Option<&str>, api_key: Option<&Secret<String>>) -> Endpoint {
        let model = model.unwrap_or(&cfg.model).to_string();
        let base_url = base_url.map(|u| u.trim_end_matches('/').to_string()).unwrap_or_else(|| cfg.base_url.clone());
        // The configured key only ever goes to the configured endpoint: a request that points
        // `llm_base_url` elsewhere must bring its own `llm_api_key`, or none is sent.
        let api_key = match api_key {
            Some(k) => k.clone(),
            None if base_url == cfg.base_url => cfg.api_key.clone(),
            None => Secret(String::new()),
        };
        let fp = blake3::hash(api_key.expose().as_bytes()).to_hex();
        let lane_key = format!("{}/{}/{}/{}", cfg.name, model, &fp[..8], crate::source::sanitize(&base_url));
        Endpoint { provider: cfg.name.clone(), kind: cfg.kind, base_url, api_key, model, cfg: cfg.clone(), lane_key }
    }

    pub fn sanitized_endpoint(&self) -> String {
        crate::source::sanitize(&self.base_url)
    }
}

/// A document attached to a call: streamed from disk as base64, or a provider file reference.
#[derive(Clone, Debug)]
pub enum DocRef {
    Inline { path: PathBuf, mime: &'static str, file_name: String },
    Uploaded { id: String, mime: &'static str },
}

#[derive(Clone, Debug)]
pub struct LlmRequest {
    pub system: String,
    pub user: String,
    pub doc: Option<DocRef>,
    pub max_output_tokens: u32,
    pub json: bool,
    /// JSON Schema the answer must follow (structured extraction). OpenAI enforces it natively;
    /// other providers get JSON mode and the result is validated by the caller.
    pub schema: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
}

impl Usage {
    pub fn source(&self) -> &'static str {
        if self.input_tokens.is_some() || self.output_tokens.is_some() { "provider" } else { "missing" }
    }
}

#[derive(Debug, Clone, Default)]
pub struct LlmResponse {
    pub text: String,
    pub truncated: bool,
    pub usage: Usage,
    pub provider_request_id: Option<String>,
}

#[derive(Debug, Clone)]
pub enum CallError {
    Http { status: u16, retry_after: Option<Duration>, message: String },
    Network(String),
    Timeout,
    Protocol(String),
    BreakerOpen,
    Cancelled,
}

impl CallError {
    pub fn retryable(&self) -> bool {
        match self {
            CallError::Http { status, .. } => *status == 429 || *status >= 500,
            CallError::Network(_) | CallError::Timeout => true,
            _ => false,
        }
    }
    pub fn status_label(&self) -> &'static str {
        match self {
            CallError::Http { status: 429, .. } => "rate_limited",
            CallError::Http { .. } => "http_error",
            CallError::Network(_) => "network_error",
            CallError::Timeout => "timeout",
            CallError::Protocol(_) => "protocol_error",
            CallError::BreakerOpen => "breaker_open",
            CallError::Cancelled => "cancelled",
        }
    }
    pub fn message(&self) -> String {
        match self {
            CallError::Http { status, message, .. } => format!("provider returned HTTP {status}: {message}"),
            CallError::Network(m) => format!("network error: {m}"),
            CallError::Timeout => "provider call timed out".into(),
            CallError::Protocol(m) => format!("unexpected provider response: {m}"),
            CallError::BreakerOpen => "provider circuit breaker is open".into(),
            CallError::Cancelled => "call cancelled".into(),
        }
    }
    pub fn to_app(&self, stage: &'static str) -> crate::rpc::AppError {
        use crate::rpc::AppError;
        match self {
            CallError::Timeout => AppError::new(504, "provider_timeout", self.message()),
            CallError::BreakerOpen => AppError::new(503, "provider_unavailable", self.message()).retry_after(30),
            CallError::Http { status: 401 | 403, .. } => AppError::new(502, "provider_auth_failed", self.message()),
            _ => AppError::new(502, "provider_error", self.message()),
        }
        .stage(stage)
    }
}

/// Providers echo (partly masked) API keys in auth errors; never pass any part of one on.
fn redact_keys(s: &str) -> String {
    s.split(' ')
        .map(|w| {
            let t = w.trim_matches(|c: char| !c.is_ascii_alphanumeric());
            let key_like = ["sk-", "AIza", "AQ.", "gsk_", "xai-"].iter().any(|p| t.starts_with(p)) && t.len() >= 8;
            if key_like { "***" } else { w }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn http_error(status: u16, headers: &reqwest::header::HeaderMap, body: &str) -> CallError {
    let retry_after = headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<f64>().ok())
        .or_else(|| {
            // x-ratelimit-reset-requests: "1s" / "250ms" / "6m0s"
            headers.get("x-ratelimit-reset-requests").and_then(|v| v.to_str().ok()).and_then(parse_reset)
        })
        .map(Duration::from_secs_f64);
    // Keep only a short, content-free error summary.
    let message = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.pointer("/error/message").and_then(|m| m.as_str()).map(|s| redact_keys(s).chars().take(200).collect()))
        .unwrap_or_else(|| "request failed".to_string());
    CallError::Http { status, retry_after, message }
}

fn parse_reset(s: &str) -> Option<f64> {
    let mut total = 0.0;
    let mut num = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
            continue;
        }
        let n: f64 = num.parse().ok()?;
        num.clear();
        match c {
            'h' => total += n * 3600.0,
            'm' if chars.peek() == Some(&'s') => {
                chars.next();
                total += n / 1000.0
            }
            'm' => total += n * 60.0,
            's' => total += n,
            _ => return None,
        }
    }
    Some(total)
}

pub fn map_reqwest(e: reqwest::Error) -> CallError {
    if e.is_timeout() { CallError::Timeout } else { CallError::Network(e.without_url().to_string()) }
}

/// Base64-encode a file as a byte stream (64 KiB-ish reads; never the whole file in memory).
pub fn base64_file_stream(path: PathBuf) -> impl Stream<Item = std::io::Result<Bytes>> + Send + 'static {
    use tokio::io::AsyncReadExt;
    const CHUNK: usize = 3 * 21_846; // multiple of 3 => no padding until the last chunk
    futures_util::stream::unfold(None::<tokio::fs::File>, move |state| {
        let path = path.clone();
        async move {
            let mut f = match state {
                Some(f) => f,
                None => match tokio::fs::File::open(&path).await {
                    Ok(f) => f,
                    Err(e) => return Some((Err(e), None)),
                },
            };
            let mut buf = vec![0u8; CHUNK];
            let mut n = 0;
            while n < CHUNK {
                match f.read(&mut buf[n..]).await {
                    Ok(0) => break,
                    Ok(k) => n += k,
                    Err(e) => return Some((Err(e), None)),
                }
            }
            if n == 0 {
                return None;
            }
            buf.truncate(n);
            let enc = base64::engine::general_purpose::STANDARD.encode(&buf);
            Some((Ok(Bytes::from(enc)), Some(f)))
        }
    })
}

pub const B64_PLACEHOLDER: &str = "__DOCVISION_BASE64__";

/// Build a request body: the serialized JSON with the placeholder replaced by a streamed
/// base64 encoding of `path`. No base64 copy of the document is ever held in memory.
pub fn streamed_json_body(json: &serde_json::Value, path: Option<PathBuf>) -> reqwest::Body {
    let s = serde_json::to_string(json).unwrap_or_default();
    match (path, s.find(B64_PLACEHOLDER)) {
        (Some(p), Some(i)) => {
            let prefix = Bytes::from(s[..i].to_string());
            let suffix = Bytes::from(s[i + B64_PLACEHOLDER.len()..].to_string());
            let stream = futures_util::stream::once(async move { Ok::<_, std::io::Error>(prefix) })
                .chain(base64_file_stream(p))
                .chain(futures_util::stream::once(async move { Ok(suffix) }));
            reqwest::Body::wrap_stream(stream)
        }
        _ => reqwest::Body::from(s),
    }
}

/// Parse a Server-Sent Events byte stream, calling `on_data` for each `data:` payload.
pub async fn read_sse<S, F>(mut stream: S, mut on_data: F) -> Result<(), CallError>
where
    S: Stream<Item = reqwest::Result<Bytes>> + Unpin,
    F: FnMut(&str) -> Result<bool, CallError>,
{
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(map_reqwest)?;
        buf.extend_from_slice(&chunk);
        while let Some(nl) = buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buf.drain(..=nl).collect();
            let line = std::str::from_utf8(&line).map_err(|_| CallError::Protocol("invalid UTF-8 in stream".into()))?;
            let line = line.trim_end_matches(['\r', '\n']);
            if let Some(data) = line.strip_prefix("data:") {
                let data = data.trim_start();
                if data == "[DONE]" {
                    return Ok(());
                }
                if !on_data(data)? {
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

/// Dispatch to the provider adapter.
pub async fn send(client: &reqwest::Client, ep: &Endpoint, req: &LlmRequest) -> Result<LlmResponse, CallError> {
    match ep.kind {
        ProviderKind::Openai => openai::call(client, ep, req, true).await,
        ProviderKind::Compatible => compatible::call(client, ep, req).await,
        ProviderKind::Gemini => gemini::call(client, ep, req).await,
    }
}

/// Upload a document once for reuse across batches; `None` if the provider has no upload API.
pub async fn upload(
    client: &reqwest::Client,
    ep: &Endpoint,
    path: PathBuf,
    mime: &'static str,
    size: u64,
) -> Result<Option<DocRef>, CallError> {
    match ep.kind {
        ProviderKind::Openai => openai::upload(client, ep, path, mime).await.map(Some),
        ProviderKind::Gemini => gemini::upload(client, ep, path, mime, size).await.map(Some),
        ProviderKind::Compatible => Ok(None),
    }
}

pub async fn delete_upload(client: &reqwest::Client, ep: &Endpoint, doc: &DocRef) {
    if let DocRef::Uploaded { id, .. } = doc {
        let _ = match ep.kind {
            ProviderKind::Openai => openai::delete(client, ep, id).await,
            ProviderKind::Gemini => gemini::delete(client, ep, id).await,
            ProviderKind::Compatible => Ok(()),
        };
    }
}

// ---------------------------------------------------------------- accounting

#[derive(Debug, Clone, Serialize)]
pub struct CallRecord {
    pub call_id: String,
    pub stage: &'static str,
    pub purpose: String,
    pub provider: String,
    pub endpoint: String,
    pub model: String,
    pub provider_request_id: Option<String>,
    pub status: String,
    pub attempt: u32,
    pub source_pages_or_chunks: Option<String>,
    pub started_at: String,
    pub duration_ms: u64,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub usage_source: &'static str,
    pub request_summary: String,
    pub response_summary: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct LlmTotals {
    /// Provider and model of the calls (`null` when no LLM was used).
    pub provider: Option<String>,
    pub model: Option<String>,
    pub calls: u64,
    pub failed_calls: u64,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub usage_complete: bool,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct LlmSection {
    pub calls: Vec<CallRecord>,
    pub totals: LlmTotals,
}

/// Collects call records for one request; persists each attempt via the writer.
pub struct Accounting {
    pub request_id: String,
    writer: Option<Writer>,
    calls: Mutex<Vec<CallRecord>>,
    intervals: Mutex<Vec<(Instant, Instant)>>,
    pub retry_wait_ms: AtomicU64,
}

impl Accounting {
    pub fn new(request_id: String, writer: Option<Writer>) -> Accounting {
        Accounting {
            request_id,
            writer,
            calls: Mutex::new(Vec::new()),
            intervals: Mutex::new(Vec::new()),
            retry_wait_ms: AtomicU64::new(0),
        }
    }

    pub fn record(&self, rec: CallRecord, started: Instant, ended: Instant) {
        if let Some(w) = &self.writer {
            if let Ok(record) = serde_json::to_string(&rec) {
                w.send(WriteOp::Call { call_id: rec.call_id.clone(), request_id: self.request_id.clone(), record, at: now_ms() });
            }
        }
        self.intervals.lock().unwrap().push((started, ended));
        self.calls.lock().unwrap().push(rec);
    }

    pub fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    /// Union of active attempt intervals, and the plain sum.
    pub fn wall_and_sum_ms(&self) -> (u64, u64) {
        let mut iv = self.intervals.lock().unwrap().clone();
        let sum: u64 = iv.iter().map(|(a, b)| b.duration_since(*a).as_millis() as u64).sum();
        iv.sort_by_key(|x| x.0);
        let mut wall = Duration::ZERO;
        let mut cur: Option<(Instant, Instant)> = None;
        for (a, b) in iv {
            cur = match cur {
                Some((s, e)) if a <= e => Some((s, e.max(b))),
                Some((s, e)) => {
                    wall += e - s;
                    Some((a, b))
                }
                None => Some((a, b)),
            };
        }
        if let Some((s, e)) = cur {
            wall += e - s;
        }
        (wall.as_millis() as u64, sum)
    }

    pub fn section(&self) -> LlmSection {
        let calls = self.calls.lock().unwrap().clone();
        let totals = totals(&calls);
        LlmSection { calls, totals }
    }
}

pub fn totals(calls: &[CallRecord]) -> LlmTotals {
    let mut t = LlmTotals { calls: calls.len() as u64, usage_complete: true, ..Default::default() };
    let add = |acc: &mut Option<u64>, v: Option<u64>| {
        if let Some(v) = v {
            *acc = Some(acc.unwrap_or(0) + v);
        }
    };
    if let Some(c) = calls.first() {
        t.provider = Some(c.provider.clone());
        t.model = Some(c.model.clone());
    }
    for c in calls {
        if c.status != "ok" {
            t.failed_calls += 1;
        }
        // Attempts that reached the provider should report usage; missing usage stays null.
        if c.usage_source == "missing" && !matches!(c.status.as_str(), "cancelled" | "breaker_open" | "network_error" | "timeout") {
            t.usage_complete = false;
        }
        add(&mut t.input_tokens, c.input_tokens);
        add(&mut t.output_tokens, c.output_tokens);
        add(&mut t.total_tokens, c.total_tokens);
        add(&mut t.cached_tokens, c.cached_tokens);
        add(&mut t.reasoning_tokens, c.reasoning_tokens);
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_errors_never_carry_keys() {
        let e = http_error(
            401,
            &Default::default(),
            r#"{"error":{"message":"Incorrect API key provided: sk-abc123*****wxyz. You can find your API key at https://x."}}"#,
        );
        assert!(!e.message().contains("sk-abc"), "{}", e.message());
        assert!(e.message().contains("Incorrect API key provided: ***"));
    }

    #[test]
    fn default_key_never_goes_to_another_endpoint() {
        let cfg = crate::config::ProviderConfig {
            name: "openai".into(),
            kind: ProviderKind::Openai,
            api_key: Secret("sk-server-key".into()),
            base_url: "https://api.openai.com/v1".into(),
            model: "m".into(),
            max_concurrency: 1,
            timeout_ms: 1000,
            max_retries: 0,
            max_output_tokens: 100,
            max_input_tokens: 1000,
            tokens_per_page: 800,
        };
        assert_eq!(Endpoint::resolve(&cfg, None, None, None).api_key.expose(), "sk-server-key");
        assert_eq!(Endpoint::resolve(&cfg, None, Some("https://api.openai.com/v1/"), None).api_key.expose(), "sk-server-key");
        assert_eq!(Endpoint::resolve(&cfg, None, Some("https://attacker.example/v1"), None).api_key.expose(), "");
        let own = Secret("sk-request".to_string());
        assert_eq!(Endpoint::resolve(&cfg, None, Some("https://other.example/v1"), Some(&own)).api_key.expose(), "sk-request");
    }
}
