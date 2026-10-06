//! `POST /rpc` envelope, auth, validation and dispatch; `/livez`, `/metrics` and `/_/dashboard`.

use crate::App;
use crate::db::{RequestRow, WriteOp, now_ms};
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;
use subtle::ConstantTimeEq;

#[derive(Debug, Clone, Serialize)]
pub struct ErrorBody {
    pub code: &'static str,
    pub message: String,
    pub stage: Option<&'static str>,
}

/// An RPC failure: HTTP status, stable code, safe message, optional failed result in `data`.
#[derive(Debug, Clone)]
pub struct AppError {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
    pub stage: Option<&'static str>,
    pub retry_after: Option<u64>,
    pub data: Option<Vec<u8>>,
}

impl AppError {
    pub fn new(status: u16, code: &'static str, message: impl Into<String>) -> AppError {
        AppError { status, code, message: message.into(), stage: None, retry_after: None, data: None }
    }
    pub fn bad_request(code: &'static str, message: impl Into<String>) -> AppError {
        AppError::new(400, code, message)
    }
    pub fn internal(message: impl Into<String>) -> AppError {
        AppError::new(500, "internal_error", message)
    }
    pub fn feature_not_compiled(feature: &str) -> AppError {
        AppError::new(400, "feature_not_compiled", format!("this build does not include the `{feature}` feature"))
    }
    pub fn stage(mut self, s: &'static str) -> AppError {
        self.stage = Some(s);
        self
    }
    pub fn retry_after(mut self, secs: u64) -> AppError {
        self.retry_after = Some(secs.clamp(1, 3600));
        self
    }
    pub fn body(&self) -> ErrorBody {
        ErrorBody { code: self.code, message: self.message.clone(), stage: self.stage }
    }
}

pub struct ReqCtx {
    pub request_id: String,
    pub started: Instant,
    pub created_at: i64,
    pub operation: String,
}

/// Build `{"ok":..,"operation":..,"request_id":..,"data":<data>,"error":..}` without re-parsing `data`.
pub fn envelope(op: &str, request_id: &str, data: Option<&[u8]>, error: Option<&ErrorBody>) -> Vec<u8> {
    let data_len = data.map_or(4, |d| d.len());
    let mut out = Vec::with_capacity(data_len + 160);
    out.extend_from_slice(b"{\"ok\":");
    out.extend_from_slice(if error.is_none() { b"true" } else { b"false" });
    out.extend_from_slice(b",\"operation\":");
    serde_json::to_writer(&mut out, op).ok();
    out.extend_from_slice(b",\"request_id\":");
    serde_json::to_writer(&mut out, request_id).ok();
    out.extend_from_slice(b",\"data\":");
    out.extend_from_slice(data.unwrap_or(b"null"));
    out.extend_from_slice(b",\"error\":");
    match error {
        Some(e) => serde_json::to_writer(&mut out, e).ok(),
        None => {
            out.extend_from_slice(b"null");
            None
        }
    };
    out.push(b'}');
    out
}

pub fn json_response(status: u16, body: Vec<u8>, retry_after: Option<u64>) -> Response {
    let mut r = Response::new(Body::from(body));
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    if let Some(s) = retry_after {
        r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from(s));
    }
    r
}

pub fn ok(op: &str, request_id: &str, status: u16, data: &impl Serialize) -> Response {
    let bytes = serde_json::to_vec(data).unwrap_or_else(|_| b"null".to_vec());
    json_response(status, envelope(op, request_id, Some(&bytes), None), None)
}

pub fn err_response(op: &str, request_id: &str, e: &AppError) -> Response {
    json_response(e.status, envelope(op, request_id, e.data.as_deref(), Some(&e.body())), e.retry_after)
}

/// Rate cap for logging rejected requests, so floods cannot fill the DB.
#[derive(Default)]
pub struct RateCap {
    window: AtomicU64,
    count: AtomicU64,
}

impl RateCap {
    pub fn allow(&self, per_sec: u32) -> bool {
        let sec = now_ms() as u64 / 1000;
        if self.window.swap(sec, Relaxed) != sec {
            self.count.store(0, Relaxed);
        }
        self.count.fetch_add(1, Relaxed) < per_sec as u64
    }
}

pub fn hash_token(t: &str) -> [u8; 32] {
    *blake3::hash(t.as_bytes()).as_bytes()
}

pub fn authorized(app: &App, headers: &HeaderMap) -> bool {
    let Some(v) = headers.get("x-access-token") else { return false };
    hash_token(v.to_str().unwrap_or("")).ct_eq(&app.token_hash).into()
}

const UPLOAD_KEYS: &[&str] = &["file", "content_base64", "file_data", "file_content", "base64", "data_url", "upload"];

/// Find upload-like fields anywhere in the request.
pub fn find_upload(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if s.starts_with("data:") => Some("data: URL".into()),
        Value::Array(a) => a.iter().find_map(find_upload),
        Value::Object(m) => {
            m.iter().find_map(|(k, v)| if UPLOAD_KEYS.contains(&k.as_str()) { Some(format!("field `{k}`")) } else { find_upload(v) })
        }
        _ => None,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    operation: String,
    #[serde(default)]
    payload: Value,
    #[serde(default)]
    options: Value,
}

pub async fn livez() -> &'static str {
    "ok"
}

/// Self-contained activity dashboard. The page itself is public; all data is fetched from
/// `/rpc` with the token the user enters (kept in the tab's sessionStorage).
pub async fn dashboard() -> Response {
    const PAGE: &str = include_str!("dashboard.html");
    let mut r = Response::new(Body::from(PAGE));
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; form-action 'none'; frame-ancestors 'none'; base-uri 'none'"),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

pub async fn metrics(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    if !authorized(&app, &headers) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let mut r = Response::new(Body::from(app.metrics.render()));
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; version=0.0.4"));
    r
}

fn log_rejected(app: &App, ctx: &ReqCtx, status: u16, code: &str) {
    if app.reject_cap.allow(app.cfg.reject_log_per_sec) {
        app.writer.send(WriteOp::InsertRequest(RequestRow {
            request_id: ctx.request_id.clone(),
            operation: ctx.operation.clone(),
            request_status: "rejected".into(),
            execution_status: Some(code.to_string()),
            http_status: Some(status as i64),
            created_at: ctx.created_at,
            ..Default::default()
        }));
    }
}

pub async fn rpc(State(app): State<Arc<App>>, headers: HeaderMap, body: Bytes) -> Response {
    let mut ctx =
        ReqCtx { request_id: uuid::Uuid::now_v7().to_string(), started: Instant::now(), created_at: now_ms(), operation: "unknown".into() };
    let resp = match dispatch(&app, &mut ctx, &headers, &body).await {
        Ok(r) => r,
        Err(e) => {
            if matches!(e.status, 400 | 401 | 413 | 415) {
                log_rejected(&app, &ctx, e.status, e.code);
            }
            err_response(&ctx.operation, &ctx.request_id, &e)
        }
    };
    app.metrics.rpc(&ctx.operation, resp.status().as_u16());
    let mut resp = resp;
    if let Ok(v) = HeaderValue::from_str(&ctx.request_id) {
        resp.headers_mut().insert("x-request-id", v);
    }
    resp
}

async fn dispatch(app: &Arc<App>, ctx: &mut ReqCtx, headers: &HeaderMap, body: &Bytes) -> Result<Response, AppError> {
    if !authorized(app, headers) {
        return Err(AppError::new(401, "unauthorized", "missing or invalid X-ACCESS-TOKEN"));
    }
    let ct = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    if ct.starts_with("multipart/") {
        return Err(AppError::new(
            415,
            "upload_not_supported",
            "multipart uploads are not accepted; reference the document by path, HTTPS URL or S3 location",
        ));
    }
    if !ct.starts_with("application/json") {
        return Err(AppError::new(415, "unsupported_media_type", "Content-Type must be application/json"));
    }
    let value: Value = serde_json::from_slice(body).map_err(|e| AppError::bad_request("invalid_json", format!("invalid JSON: {e}")))?;
    if let Some(op) = value.get("operation").and_then(Value::as_str) {
        ctx.operation = op.chars().take(32).collect();
    }
    if let Some(what) = find_upload(&value) {
        return Err(AppError::new(
            415,
            "upload_not_supported",
            format!("direct uploads are not accepted ({what}); reference the document by path, HTTPS URL or S3 location"),
        ));
    }
    let env: Envelope = serde_json::from_value(value).map_err(|e| AppError::bad_request("invalid_envelope", e.to_string()))?;
    if body.len() > app.cfg.max_body_bytes && !matches!(env.operation.as_str(), "summarize" | "chunk" | "extract") {
        return Err(AppError::new(413, "body_too_large", format!("request body exceeds {} bytes", app.cfg.max_body_bytes)));
    }

    match env.operation.as_str() {
        "convert" => crate::jobs::convert(app, ctx, env.payload, env.options).await,
        "summarize" => crate::enrich::summarize_op(app, ctx, env.payload, env.options).await,
        "chunk" => crate::enrich::chunk_op(app, ctx, env.payload, env.options).await,
        "extract" => crate::enrich::extract_op(app, ctx, env.payload, env.options).await,
        "job.get" => crate::jobs::job_get(app, ctx, env.payload).await,
        "job.wait" => crate::jobs::job_wait(app, ctx, env.payload).await,
        "history.list" => crate::history::list(app, ctx, env.payload).await,
        "history.get" => crate::history::get(app, ctx, env.payload).await,
        "stats" => crate::history::stats(app, ctx, env.payload).await,
        "health" => Ok(crate::health(app, ctx).await),
        other => Err(AppError::bad_request("unknown_operation", format!("unknown operation `{other}`"))),
    }
}

/// Parse a typed payload, mapping serde errors to 400.
pub fn typed<T: serde::de::DeserializeOwned>(v: Value, what: &str) -> Result<T, AppError> {
    let v = if v.is_null() { Value::Object(Default::default()) } else { v };
    serde_json::from_value(v).map_err(|e| AppError::bad_request("invalid_payload", format!("invalid {what}: {e}")))
}

// ---------- typed request structs ----------

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Execution {
    #[default]
    Async,
    Sync,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Ocr {
    #[default]
    On,
    Off,
    Auto,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Method {
    #[default]
    Llm,
    Local,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheMode {
    #[default]
    Use,
    Bypass,
    Refresh,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    #[default]
    Normal,
    Low,
}

pub const MIN_CHUNK_SIZE: u32 = 128;
pub const MAX_CHUNK_SIZE: u32 = 200_000;

/// Request options. Flat by design: every option is a top-level key.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Options {
    // execution
    pub execution: Execution,
    pub priority: Priority,
    pub cache: CacheMode,
    pub allow_partial: bool,
    // PDF / OCR
    pub ocr: Ocr,
    pub pdf_skip_local_processing: Option<bool>,
    // enrichment
    pub gen_title: bool,
    pub gen_summary: bool,
    pub gen_chunks: bool,
    pub title_method: Method,
    pub summary_method: Method,
    pub chunk_size: u32,
    /// `None` = 15% of the effective chunk size.
    pub chunk_overlap: Option<u32>,
    pub detect_language: bool,
    pub language_method: Method,
    pub translate_to: Option<String>,
    pub words_per_page: u32,
    // output
    pub overwrite: bool,
    // LLM
    pub llm_provider: Option<String>,
    pub llm_model: Option<String>,
    pub llm_base_url: Option<String>,
    pub llm_api_key: Option<crate::config::Secret<String>>,
    // structured extraction
    /// JSON Schema (object at the top); the result's `extracted` field follows it.
    pub extract_schema: Option<Value>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            execution: Execution::Async,
            priority: Priority::Normal,
            cache: CacheMode::Use,
            allow_partial: false,
            ocr: Ocr::On,
            pdf_skip_local_processing: None,
            gen_title: true,
            gen_summary: true,
            gen_chunks: true,
            title_method: Method::Llm,
            summary_method: Method::Llm,
            chunk_size: 512,
            chunk_overlap: None,
            detect_language: false,
            language_method: Method::Local,
            translate_to: None,
            words_per_page: 500,
            overwrite: false,
            llm_provider: None,
            llm_model: None,
            llm_base_url: None,
            llm_api_key: None,
            extract_schema: None,
        }
    }
}

impl Options {
    /// Effective chunk size: values below `MIN_CHUNK_SIZE` are raised to it.
    pub fn effective_chunk_size(&self) -> u32 {
        self.chunk_size.max(MIN_CHUNK_SIZE)
    }

    /// Effective overlap: the explicit value, or 15% of the effective chunk size.
    pub fn effective_chunk_overlap(&self) -> u32 {
        self.chunk_overlap.unwrap_or_else(|| self.effective_chunk_size() * 15 / 100)
    }

    /// Effective `pdf_skip_local_processing`: defaults to `true` for `ocr=on`.
    pub fn skip_local(&self) -> bool {
        self.pdf_skip_local_processing.unwrap_or(self.ocr == Ocr::On)
    }

    pub fn validate(&self) -> Result<(), AppError> {
        let bad = |m: &str| Err(AppError::bad_request("invalid_options", m.to_string()));
        if self.pdf_skip_local_processing == Some(true) && self.ocr != Ocr::On {
            return bad("pdf_skip_local_processing=true requires ocr=on");
        }
        if self.chunk_size > MAX_CHUNK_SIZE {
            return bad("chunk_size must be at most 200000");
        }
        if self.chunk_overlap.is_some_and(|o| o >= self.effective_chunk_size()) {
            return bad("chunk_overlap must be smaller than chunk_size");
        }
        if self.words_per_page == 0 {
            return bad("words_per_page must be > 0");
        }
        if self.detect_language && self.language_method == Method::Local && !cfg!(feature = "lang-detect") {
            return Err(AppError::feature_not_compiled("lang-detect"));
        }
        if self.translate_to.is_some() && !cfg!(feature = "translate") {
            return Err(AppError::feature_not_compiled("translate"));
        }
        if let Some(t) = &self.translate_to {
            if t.is_empty() || t.len() > 35 || !t.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                return bad("translate_to must be a BCP-47 tag");
            }
        }
        if self.execution == Execution::Sync && self.priority == Priority::Low {
            return bad("priority is only valid with async execution");
        }
        if let Some(s) = &self.extract_schema {
            crate::schema::check_schema(s).map_err(|m| AppError::bad_request("invalid_options", m))?;
        }
        if let Some(u) = &self.llm_base_url {
            if !(u.starts_with("https://") || u.starts_with("http://")) {
                return bad("llm_base_url must be an http(s) URL");
            }
        }
        Ok(())
    }

    /// Canonical, secret-free form used for cache keys and history.
    pub fn sanitized(&self) -> Value {
        let mut v = serde_json::to_value(self).unwrap_or(Value::Null);
        if let Some(o) = v.as_object_mut() {
            o.insert("chunk_size".into(), Value::from(self.effective_chunk_size()));
            o.insert("chunk_overlap".into(), Value::from(self.effective_chunk_overlap()));
            o.remove("llm_api_key");
            if let Some(Value::String(u)) = o.get_mut("llm_base_url") {
                *u = crate::source::sanitize(u);
            }
        }
        v
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebhookSpec {
    pub url: String,
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub secret: Option<String>,
    /// JSON template: `"$.path"` strings are replaced with values from the result, `"$"` is the
    /// whole result. Omitted: a short `job.finished` event.
    #[serde(default)]
    pub body: Option<Value>,
    /// One attempt, no retries; any HTTP response marks the delivery `sent`.
    #[serde(default)]
    pub fire_and_forget: bool,
}

/// `payload.webhook`: one webhook object or an array of them.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(transparent)]
pub struct Webhooks(pub Vec<WebhookSpec>);

impl<'de> Deserialize<'de> for Webhooks {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        match Value::deserialize(d)? {
            v @ Value::Array(_) => serde_json::from_value(v).map(Webhooks).map_err(D::Error::custom),
            v => serde_json::from_value(v).map(|w| Webhooks(vec![w])).map_err(D::Error::custom),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConvertPayload {
    pub source: String,
    #[serde(default)]
    pub destination: Option<String>,
    #[serde(default)]
    pub metadata: Option<serde_json::Map<String, Value>>,
    #[serde(default)]
    pub webhook: Option<Webhooks>,
    /// Who sent the request (a name or ID), for filtering history. Default `unknown`.
    #[serde(default)]
    pub requester_id: Option<String>,
}

/// Normalized `requester_id`: trimmed, `unknown` when absent, at most 128 printable characters.
pub fn requester_id(v: Option<&str>) -> Result<String, AppError> {
    let v = v.map(str::trim).filter(|v| !v.is_empty()).unwrap_or("unknown");
    if v.chars().count() > 128 || v.chars().any(char::is_control) {
        return Err(AppError::bad_request("invalid_payload", "requester_id must be at most 128 printable characters"));
    }
    Ok(v.to_string())
}

impl ConvertPayload {
    pub fn webhooks(&self) -> &[WebhookSpec] {
        self.webhook.as_ref().map_or(&[], |w| &w.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn finds_uploads() {
        assert!(find_upload(&serde_json::json!({"payload": {"file": "x"}})).is_some());
        assert!(find_upload(&serde_json::json!({"payload": {"a": ["data:text/plain;base64,AA"]}})).is_some());
        assert!(find_upload(&serde_json::json!({"payload": {"source": "/a.pdf"}})).is_none());
    }
    #[test]
    fn envelope_shape() {
        let b = envelope("convert", "r1", Some(b"{\"a\":1}"), None);
        let v: Value = serde_json::from_slice(&b).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["data"]["a"], 1);
        let e = AppError::new(429, "queue_full", "full");
        let v: Value = serde_json::from_slice(&envelope("convert", "r1", None, Some(&e.body()))).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["data"], Value::Null);
        assert_eq!(v["error"]["code"], "queue_full");
    }
}
