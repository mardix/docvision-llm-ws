//! Mock OpenAI-compatible provider: configurable latency, 429/5xx injection, a concurrency
//! limit that returns 429 when exceeded, optional usage reporting, and call counters.
//! Used by integration tests and by `examples/mock_provider.rs` for load tests.

#![allow(dead_code)]

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::SeqCst};
use std::time::Duration;

#[derive(Default)]
pub struct MockState {
    pub latency_ms: Mutex<(u64, u64)>,
    pub p429: Mutex<f64>,
    pub p5xx: Mutex<f64>,
    /// Return 429 when more than this many requests are in flight (0 = unlimited).
    pub max_inflight: AtomicU64,
    pub omit_usage: AtomicBool,
    /// Report finish_reason=length for transcription requests spanning more than one page.
    pub truncate_multi_page: AtomicBool,
    /// Wrap transcriptions like some models do: preamble + ```markdown fence.
    pub wrap_markdown: AtomicBool,
    /// Answer this many structured-extraction requests with JSON that misses required fields.
    pub bad_extractions: AtomicU64,
    /// Page count of each inline PDF received (shows whether the service split the document).
    pub pdf_pages_seen: Mutex<Vec<usize>>,
    pub extractions: AtomicU64,
    pub inflight: AtomicU64,
    pub peak_inflight: AtomicU64,
    pub requests: AtomicU64,
    pub ok: AtomicU64,
    pub rejected_429: AtomicU64,
    pub errors_5xx: AtomicU64,
    pub transcriptions: AtomicU64,
    pub enrich_calls: AtomicU64,
    pub translations: AtomicU64,
    pub uploads: AtomicU64,
    pub bytes_received: AtomicU64,
}

impl MockState {
    pub fn set_latency(&self, min: u64, max: u64) {
        *self.latency_ms.lock().unwrap() = (min, max.max(min));
    }
    pub fn snapshot(&self) -> Value {
        json!({
            "requests": self.requests.load(SeqCst), "ok": self.ok.load(SeqCst),
            "rejected_429": self.rejected_429.load(SeqCst), "errors_5xx": self.errors_5xx.load(SeqCst),
            "transcriptions": self.transcriptions.load(SeqCst), "enrich_calls": self.enrich_calls.load(SeqCst),
            "translations": self.translations.load(SeqCst), "uploads": self.uploads.load(SeqCst),
            "peak_inflight": self.peak_inflight.load(SeqCst), "bytes_received": self.bytes_received.load(SeqCst),
        })
    }
}

struct Inflight<'a>(&'a MockState);
impl Drop for Inflight<'_> {
    fn drop(&mut self) {
        self.0.inflight.fetch_sub(1, SeqCst);
    }
}

fn pages_requested(text: &str) -> Vec<u32> {
    // "Transcribe pages A through B" or "Transcribe page N"
    let nums: Vec<u32> = text.split(|c: char| !c.is_ascii_digit()).filter_map(|t| t.parse().ok()).collect();
    if text.contains("pages") && nums.len() >= 2 {
        (nums[0]..=nums[1]).collect()
    } else if text.contains("page ") && !nums.is_empty() {
        vec![nums[0]]
    } else {
        vec![1]
    }
}

async fn chat(State(st): State<Arc<MockState>>, body: Bytes) -> Response {
    st.requests.fetch_add(1, SeqCst);
    st.bytes_received.fetch_add(body.len() as u64, SeqCst);
    let n = st.inflight.fetch_add(1, SeqCst) + 1;
    let _g = Inflight(&st);
    st.peak_inflight.fetch_max(n, SeqCst);
    let max = st.max_inflight.load(SeqCst);
    if max > 0 && n > max {
        st.rejected_429.fetch_add(1, SeqCst);
        return (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "0.05")], r#"{"error":{"message":"rate limited"}}"#).into_response();
    }
    let (p429, p5xx) = (*st.p429.lock().unwrap(), *st.p5xx.lock().unwrap());
    let r = fastrand::f64();
    if r < p429 {
        st.rejected_429.fetch_add(1, SeqCst);
        return (StatusCode::TOO_MANY_REQUESTS, r#"{"error":{"message":"rate limited"}}"#).into_response();
    }
    if r < p429 + p5xx {
        st.errors_5xx.fetch_add(1, SeqCst);
        return (StatusCode::INTERNAL_SERVER_ERROR, r#"{"error":{"message":"boom"}}"#).into_response();
    }
    let (lo, hi) = *st.latency_ms.lock().unwrap();
    let delay = if hi > lo { fastrand::u64(lo..=hi) } else { lo };
    let v: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return (StatusCode::BAD_REQUEST, r#"{"error":{"message":"bad json"}}"#).into_response(),
    };
    let system = v.pointer("/messages/0/content").and_then(Value::as_str).unwrap_or("").to_string();
    let user_text = v.pointer("/messages/1/content/0/text").and_then(Value::as_str).unwrap_or("").to_string();
    let has_file = v.pointer("/messages/1/content/1").is_some();
    if let Some(data) = v.pointer("/messages/1/content/1/file/file_data").and_then(Value::as_str) {
        use base64::Engine;
        if let Some(b) = data.split_once("base64,").and_then(|(_, b)| base64::engine::general_purpose::STANDARD.decode(b).ok()) {
            st.pdf_pages_seen.lock().unwrap().push(count_pdf_pages(&b));
        }
    }
    let json_mode = v.get("response_format").is_some();
    let mut finish = "stop";
    let text = if system.contains("extract structured data") {
        st.extractions.fetch_add(1, SeqCst);
        // The schema sits in the prompt (and in response_format for native OpenAI).
        let schema = user_text
            .split_once("<schema>\n")
            .and_then(|(_, r)| r.split_once("\n</schema>"))
            .and_then(|(s, _)| serde_json::from_str::<Value>(s).ok())
            .unwrap_or(Value::Null);
        let bad = st.bad_extractions.load(SeqCst) > 0;
        if bad {
            st.bad_extractions.fetch_sub(1, SeqCst);
            "{}".to_string()
        } else {
            sample_for(&schema).to_string()
        }
    } else if json_mode {
        st.enrich_calls.fetch_add(1, SeqCst);
        json!({"title": "Mock Title", "summary": "Mock summary of the document.", "language": "en", "language_confidence": 0.93})
            .to_string()
    } else if system.contains("translator") {
        st.translations.fetch_add(1, SeqCst);
        user_text.split_once("\n\n").map(|x| x.1.to_string()).unwrap_or_default()
    } else if has_file && user_text.contains("image") && !user_text.contains("PDF") {
        st.transcriptions.fetch_add(1, SeqCst);
        "# Image Heading\n\nMock image text.\n".to_string()
    } else if has_file {
        st.transcriptions.fetch_add(1, SeqCst);
        let pages = pages_requested(&user_text);
        if pages.len() > 1 && st.truncate_multi_page.load(SeqCst) {
            finish = "length";
        }
        let mut out = String::new();
        for p in pages {
            out.push_str(&format!("## Page {p}\n\nMock transcription of page {p}. It has several words of text.\n\n"));
        }
        out
    } else {
        st.enrich_calls.fetch_add(1, SeqCst);
        "Mock partial summary.".to_string()
    };
    let text =
        if st.wrap_markdown.load(SeqCst) && !json_mode { format!("Here is the transcription:\n```markdown\n{text}\n```\n") } else { text };
    if delay > 0 {
        tokio::time::sleep(Duration::from_millis(delay)).await;
    }
    st.ok.fetch_add(1, SeqCst);
    let prompt_tokens = (body.len() / 4) as u64;
    let completion_tokens = (text.len() / 4) as u64 + 1;
    let mut sse = String::new();
    // Stream the text in a few chunks.
    let chars: Vec<char> = text.chars().collect();
    for part in chars.chunks(64) {
        let s: String = part.iter().collect();
        sse.push_str(&format!(
            "data: {}\n\n",
            json!({"id": "chatcmpl-mock", "choices": [{"index": 0, "delta": {"content": s}, "finish_reason": null}]})
        ));
    }
    sse.push_str(&format!("data: {}\n\n", json!({"id": "chatcmpl-mock", "choices": [{"index": 0, "delta": {}, "finish_reason": finish}]})));
    if !st.omit_usage.load(SeqCst) {
        sse.push_str(&format!(
            "data: {}\n\n",
            json!({"id": "chatcmpl-mock", "choices": [], "usage": {"prompt_tokens": prompt_tokens, "completion_tokens": completion_tokens, "total_tokens": prompt_tokens + completion_tokens, "prompt_tokens_details": {"cached_tokens": 0}}})
        ));
    }
    sse.push_str("data: [DONE]\n\n");
    Response::builder().header("content-type", "text/event-stream").header("x-request-id", "req-mock").body(Body::from(sse)).unwrap()
}

async fn upload(State(st): State<Arc<MockState>>, body: Bytes) -> Response {
    st.uploads.fetch_add(1, SeqCst);
    st.bytes_received.fetch_add(body.len() as u64, SeqCst);
    json_resp(json!({"id": "file-mock", "object": "file"}))
}

async fn stats(State(st): State<Arc<MockState>>) -> Response {
    json_resp(st.snapshot())
}

fn json_resp(v: Value) -> Response {
    ([("content-type", "application/json")], v.to_string()).into_response()
}

pub fn router(st: Arc<MockState>) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(chat))
        .route("/v1/files", post(upload))
        .route("/v1/files/{id}", delete(|| async { "{}" }))
        .route("/stats", get(stats))
        .layer(axum::extract::DefaultBodyLimit::disable())
        .with_state(st)
}

/// Start on an ephemeral port; returns the base URL (`http://127.0.0.1:PORT/v1`).
pub async fn spawn(st: Arc<MockState>) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, router(st)).await.unwrap() });
    format!("http://{addr}/v1")
}

/// A value that satisfies `schema` (object, array, string, number, integer, boolean, enum).
fn sample_for(schema: &Value) -> Value {
    if let Some(first) = schema.get("enum").and_then(Value::as_array).and_then(|e| e.first()) {
        return first.clone();
    }
    let t = match schema.get("type") {
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).find(|t| *t != "null").unwrap_or("null").to_string(),
        Some(Value::String(t)) => t.clone(),
        _ => "string".into(),
    };
    match t.as_str() {
        "object" => Value::Object(
            schema
                .get("properties")
                .and_then(Value::as_object)
                .map(|p| p.iter().map(|(k, s)| (k.clone(), sample_for(s))).collect())
                .unwrap_or_default(),
        ),
        "array" => Value::Array(vec![sample_for(schema.get("items").unwrap_or(&Value::Null))]),
        "number" => json!(42.5),
        "integer" => json!(2),
        "boolean" => json!(true),
        "null" => Value::Null,
        _ => json!("mock"),
    }
}

/// Count `/Type /Page` dictionaries (not `/Pages`) in raw PDF bytes.
fn count_pdf_pages(b: &[u8]) -> usize {
    let mut n = 0;
    let mut i = 0;
    while let Some(p) = b[i..].windows(5).position(|w| w == b"/Type") {
        let mut j = i + p + 5;
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        if b[j..].starts_with(b"/Page") && b.get(j + 5).is_none_or(|c| !c.is_ascii_alphanumeric()) {
            n += 1;
        }
        i = j;
    }
    n
}
