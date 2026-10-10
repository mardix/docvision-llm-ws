//! Completion webhooks (`webhooks` feature): HMAC-SHA256 signatures, stable event IDs,
//! bounded retries (~1 h), at-least-once delivery, resumed after restart.

use crate::App;
use crate::rpc::{AppError, WebhookSpec};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;

/// Wait before each attempt, in units of `DOCVISION_WEBHOOK_RETRY_BASE` (default 30 s):
/// 0, 30 s, 2 min, 10 min, 20 min, 30 min => 6 attempts over ~1 hour.
pub const SCHEDULE: [u32; 6] = [0, 1, 4, 20, 40, 60];

pub fn validate(w: &WebhookSpec) -> Result<(), AppError> {
    if !cfg!(feature = "webhooks") {
        return Err(AppError::feature_not_compiled("webhooks"));
    }
    let u = url::Url::parse(&w.url).map_err(|_| AppError::bad_request("invalid_webhook", "webhook.url is not a valid URL"))?;
    if !crate::source::web_url_allowed(&u) {
        return Err(AppError::bad_request("invalid_webhook", "webhook.url must be https:// (http:// only for localhost)"));
    }
    if w.body.as_ref().is_some_and(|b| b.to_string().len() > 16 * 1024) {
        return Err(AppError::bad_request("invalid_webhook", "webhook.body must be at most 16 KiB"));
    }
    if w.headers.len() > 32 {
        return Err(AppError::bad_request("invalid_webhook", "at most 32 webhook headers"));
    }
    for (k, v) in &w.headers {
        let bad = reqwest::header::HeaderName::from_bytes(k.as_bytes()).is_err() || reqwest::header::HeaderValue::from_str(v).is_err();
        if bad
            || k.to_ascii_lowercase().starts_with("x-docvision-")
            || k.eq_ignore_ascii_case("content-length")
            || k.eq_ignore_ascii_case("host")
        {
            return Err(AppError::bad_request("invalid_webhook", format!("webhook header `{k}` is not allowed")));
        }
    }
    Ok(())
}

/// Most webhooks one job can have.
pub const MAX_WEBHOOKS: usize = 10;

/// Stable per job and endpoint (`index` in the request's list), so receivers can deduplicate retries.
pub fn event_id(job_id: &str, index: usize) -> String {
    format!("evt_{}_{index}", &blake3::hash(job_id.as_bytes()).to_hex()[..32])
}

/// The endpoint index encoded in an event ID.
#[cfg(feature = "webhooks")]
fn event_index(event_id: &str) -> Option<usize> {
    event_id.rsplit_once('_')?.1.parse().ok()
}

#[cfg(feature = "webhooks")]
type Mac = hmac::Hmac<sha2::Sha256>;

#[cfg(feature = "webhooks")]
fn mac_hex(m: Mac) -> String {
    use hmac::Mac as _;
    let mut hex = String::with_capacity(71);
    hex.push_str("sha256=");
    for b in m.finalize().into_bytes() {
        hex.push_str(&format!("{b:02x}"));
    }
    hex
}

#[cfg(feature = "webhooks")]
fn mac_start(secret: &str, timestamp: &str) -> Mac {
    use hmac::Mac as _;
    let mut m = Mac::new_from_slice(secret.as_bytes()).expect("hmac key");
    m.update(timestamp.as_bytes());
    m.update(b".");
    m
}

/// `sha256=<hex HMAC-SHA256(secret, timestamp + "." + body)>`
#[cfg(feature = "webhooks")]
pub fn sign(secret: &str, timestamp: &str, body: &[u8]) -> String {
    use hmac::Mac as _;
    let mut m = mac_start(secret, timestamp);
    m.update(body);
    mac_hex(m)
}

/// Same signature computed by streaming a file (full payloads are never held in memory).
#[cfg(feature = "webhooks")]
fn sign_file(secret: &str, timestamp: &str, path: &std::path::Path) -> std::io::Result<String> {
    use hmac::Mac as _;
    use std::io::Read;
    let mut m = mac_start(secret, timestamp);
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        m.update(&buf[..n]);
    }
    Ok(mac_hex(m))
}

#[cfg(feature = "webhooks")]
enum Payload {
    Bytes(bytes::Bytes),
    File(PathBuf),
}

pub fn schedule(
    app: Arc<App>,
    job_id: String,
    request_id: String,
    spec: WebhookSpec,
    index: usize,
    result_path: PathBuf,
    start_attempt: usize,
) {
    #[cfg(feature = "webhooks")]
    tokio::spawn(deliver(app, job_id, request_id, spec, index, result_path, start_attempt));
    #[cfg(not(feature = "webhooks"))]
    let _ = (app, job_id, request_id, spec, index, result_path, start_attempt);
}

#[cfg_attr(not(feature = "webhooks"), allow(dead_code))]
/// A placeholder is `"$"` (the whole result) or `"$.path"`; `"$$..."` escapes a literal `$`.
fn placeholder(s: &str) -> Option<&str> {
    match s.strip_prefix('$')? {
        "" => Some(""),
        rest => rest.strip_prefix('.'),
    }
}

#[cfg_attr(not(feature = "webhooks"), allow(dead_code))]
/// Top-level result fields a template needs (`None` = the whole result).
fn template_fields(t: &Value, out: &mut Vec<String>) -> Option<()> {
    match t {
        Value::String(s) => match placeholder(s) {
            Some("") => return None,
            Some(p) => {
                let top = p.split(['.', '[']).next().unwrap_or(p).to_string();
                if !out.contains(&top) {
                    out.push(top);
                }
            }
            None => {}
        },
        Value::Array(a) => a.iter().try_for_each(|v| template_fields(v, out))?,
        Value::Object(o) => o.values().try_for_each(|v| template_fields(v, out))?,
        _ => {}
    }
    Some(())
}

#[cfg_attr(not(feature = "webhooks"), allow(dead_code))]
/// Resolve `a.b`, `a.0` or `a[0]` against `root`.
fn lookup<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let path = path.replace('[', ".").replace(']', "");
    path.split('.').filter(|s| !s.is_empty()).try_fold(root, |v, seg| match v {
        Value::Object(o) => o.get(seg),
        Value::Array(a) => a.get(seg.parse::<usize>().ok()?),
        _ => None,
    })
}

#[cfg_attr(not(feature = "webhooks"), allow(dead_code))]
/// Fill a body template: placeholders become result values (missing ones become `null`).
pub fn render(t: &Value, root: &Value) -> Value {
    match t {
        Value::String(s) if s.starts_with("$$") => Value::String(s[1..].to_string()),
        Value::String(s) => match placeholder(s) {
            Some(p) => lookup(root, p).cloned().unwrap_or(Value::Null),
            None => t.clone(),
        },
        Value::Array(a) => Value::Array(a.iter().map(|v| render(v, root)).collect()),
        Value::Object(o) => Value::Object(o.iter().map(|(k, v)| (k.clone(), render(v, root))).collect()),
        _ => t.clone(),
    }
}

#[cfg(feature = "webhooks")]
async fn body_for(app: &App, eid: &str, job_id: &str, request_id: &str, spec: &WebhookSpec, path: &std::path::Path) -> Option<Payload> {
    let p = path.to_path_buf();
    let Some(template) = &spec.body else {
        let fields: Vec<String> = [
            "status",
            "src_file",
            "src_bytes",
            "dest_file",
            "md_file",
            "md_bytes",
            "docv_file",
            "docv_bytes",
            "metadata",
            "statistics",
            "llm",
            "timing",
            "error",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let mut m = app.cpu.run(move || crate::jobs::select_fields(&p, &fields)).await.ok()?.ok()?;
        let totals = m.remove("llm").and_then(|mut l| l.get_mut("totals").map(Value::take));
        let v = serde_json::json!({
            "event_id": eid,
            "event": "job.finished",
            "job_id": job_id,
            "request_id": request_id,
            "status": m.remove("status"),
            "src_file": m.remove("src_file"),
            "src_bytes": m.remove("src_bytes"),
            "dest_file": m.remove("dest_file"),
            "md_file": m.remove("md_file"),
            "md_bytes": m.remove("md_bytes"),
            "docv_file": m.remove("docv_file"),
            "docv_bytes": m.remove("docv_bytes"),
            "metadata": m.remove("metadata"),
            "statistics": m.remove("statistics"),
            "llm": totals,
            "timing": m.remove("timing"),
            "error": m.remove("error"),
        });
        return serde_json::to_vec(&v).ok().map(|b| Payload::Bytes(b.into()));
    };
    let len = tokio::fs::metadata(&p).await.ok()?.len();
    if len > app.cfg.max_result_bytes {
        return None;
    }
    // `"$"` alone streams the result file as-is.
    if template.as_str().and_then(placeholder) == Some("") {
        return Some(Payload::File(p));
    }
    let mut fields = Vec::new();
    let whole = template_fields(template, &mut fields).is_none();
    let (template, eid) = (template.clone(), eid.to_string());
    app.cpu
        .run(move || {
            let mut root = if whole {
                let f = std::io::BufReader::with_capacity(64 * 1024, std::fs::File::open(&p).ok()?);
                serde_json::from_reader(f).ok()?
            } else {
                crate::jobs::select_fields(&p, &fields).ok()?
            };
            root.insert("event_id".into(), Value::from(eid));
            root.insert("event".into(), Value::from("job.finished"));
            serde_json::to_vec(&render(&template, &Value::Object(root))).ok().map(|b| Payload::Bytes(b.into()))
        })
        .await
        .ok()?
}

#[cfg(feature = "webhooks")]
async fn deliver(app: Arc<App>, job_id: String, request_id: String, spec: WebhookSpec, index: usize, path: PathBuf, start_attempt: usize) {
    use crate::db::{WebhookRow, WriteOp, now_ms};
    let eid = event_id(&job_id, index);
    let created_at = now_ms();
    let mut row = WebhookRow {
        event_id: eid.clone(),
        job_id: job_id.clone(),
        status: "pending".into(),
        attempts: start_attempt as i64,
        last_http_status: None,
        last_error: None,
        next_attempt_at: Some(now_ms()),
        created_at,
        delivered_at: None,
    };
    app.writer.send(WriteOp::Webhook(row.clone()));
    let Some(payload) = body_for(&app, &eid, &job_id, &request_id, &spec, &path).await else {
        row.status = "failed".into();
        row.last_error = Some("result unavailable or larger than DOCVISION_MAX_RESULT_BYTES".into());
        app.writer.send(WriteOp::Webhook(row));
        return;
    };
    let client = crate::source::http_client();
    for (attempt, &wait) in SCHEDULE.iter().enumerate().skip(start_attempt) {
        if wait > 0 {
            let d = app.cfg.webhook_retry_base * wait;
            row.next_attempt_at = Some(now_ms() + d.as_millis() as i64);
            app.writer.send(WriteOp::Webhook(row.clone()));
            tokio::time::sleep(d).await;
        }
        let ts = (now_ms() / 1000).to_string();
        let mut rb = client.post(&spec.url).timeout(std::time::Duration::from_secs(10));
        for (k, v) in &spec.headers {
            rb = rb.header(k, v);
        }
        rb = rb.header("content-type", "application/json").header("x-docvision-event-id", &eid).header("x-docvision-timestamp", &ts);
        if let Some(secret) = &spec.secret {
            let sig = match &payload {
                Payload::Bytes(b) => Some(sign(secret, &ts, b)),
                Payload::File(p) => {
                    let (secret, ts, p) = (secret.clone(), ts.clone(), p.clone());
                    tokio::task::spawn_blocking(move || sign_file(&secret, &ts, &p).ok()).await.ok().flatten()
                }
            };
            if let Some(sig) = sig {
                rb = rb.header("x-docvision-signature", sig);
            }
        }
        let body = match &payload {
            Payload::Bytes(b) => reqwest::Body::from(b.clone()),
            Payload::File(p) => match tokio::fs::File::open(p).await {
                Ok(f) => reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::with_capacity(f, 64 * 1024)),
                Err(_) => {
                    row.status = "failed".into();
                    row.last_error = Some("result file no longer available".into());
                    break;
                }
            },
        };
        let started = std::time::Instant::now();
        let res = rb.body(body).send().await;
        row.attempts = attempt as i64 + 1;
        let retry = match res {
            Ok(r) if r.status().is_success() => {
                row.status = "delivered".into();
                row.last_http_status = Some(r.status().as_u16() as i64);
                row.last_error = None;
                row.delivered_at = Some(now_ms());
                row.next_attempt_at = None;
                false
            }
            Ok(r) => {
                let s = r.status().as_u16();
                row.last_http_status = Some(s as i64);
                row.last_error = Some(format!("HTTP {s}"));
                s == 429 || s >= 500
            }
            Err(e) => {
                row.last_error = Some(if e.is_timeout() { "timeout".into() } else { "network error".into() });
                true
            }
        };
        tracing::info!(target: "docvision_llm_ws::webhook", job_id = %job_id, attempt = row.attempts, status = %row.status, elapsed_ms = started.elapsed().as_millis() as u64, "webhook attempt");
        if spec.fire_and_forget {
            // Send and forget: one attempt; any HTTP response counts, its status is not checked.
            row.status = if row.last_http_status.is_some() { "sent" } else { "failed" }.into();
            if row.status == "sent" {
                row.last_error = None;
                row.delivered_at = Some(now_ms());
            }
            break;
        }
        if row.status == "delivered" {
            break;
        }
        if !retry {
            row.status = "failed".into();
            break;
        }
    }
    if row.status == "pending" {
        row.status = "failed".into();
    }
    row.next_attempt_at = None;
    app.writer.send(WriteOp::Webhook(row));
}

/// Resume pending deliveries after a restart.
#[cfg(feature = "webhooks")]
pub async fn resume(app: &Arc<App>) {
    let Ok(rows) = app.db.pending_webhooks().await else { return };
    for w in rows {
        let Ok(Some(job)) = app.db.get_job(&w.job_id).await else { continue };
        let (Some(stored), Some(path)) = (crate::jobs::stored_job(app, &job), job.result_path.clone()) else { continue };
        let Some(index) = event_index(&w.event_id) else { continue };
        if let Some(spec) = stored.payload.webhooks().get(index) {
            schedule(
                app.clone(),
                job.job_id.clone(),
                job.request_id.clone(),
                spec.clone(),
                index,
                PathBuf::from(path),
                (w.attempts as usize).min(SCHEDULE.len() - 1),
            );
        }
    }
}

#[cfg(all(test, feature = "webhooks"))]
mod tests {
    use super::*;
    #[test]
    fn signature_and_event_id() {
        // HMAC-SHA256("secret", "1700000000.{}")
        let s = sign("secret", "1700000000", b"{}");
        assert!(s.starts_with("sha256=") && s.len() == 71);
        assert_eq!(s, sign("secret", "1700000000", b"{}"));
        assert_ne!(s, sign("other", "1700000000", b"{}"));
        assert_eq!(event_id("job-1", 0), event_id("job-1", 0));
        assert_ne!(event_id("job-1", 0), event_id("job-2", 0));
        assert_ne!(event_id("job-1", 0), event_id("job-1", 1));
        assert_eq!(event_index(&event_id("job-1", 3)), Some(3));
    }

    #[test]
    fn body_template() {
        let root = serde_json::json!({"content": "# Hi", "title": "T", "chunks": [{"content": "a"}, {"content": "b"}]});
        let t = serde_json::json!({"md": "$.content", "t": "$.title", "c": "$.chunks[1].content", "c0": "$.chunks.0.content",
            "all": "$", "missing": "$.nope", "lit": "hello", "esc": "$$.content", "price": "$5", "n": [1, "$.title"]});
        let v = render(&t, &root);
        assert_eq!(v["md"], "# Hi");
        assert_eq!(v["t"], "T");
        assert_eq!(v["c"], "b");
        assert_eq!(v["c0"], "a");
        assert_eq!(v["all"], root);
        assert!(v["missing"].is_null());
        assert_eq!(v["lit"], "hello");
        assert_eq!(v["esc"], "$.content");
        assert_eq!(v["price"], "$5");
        assert_eq!(v["n"], serde_json::json!([1, "T"]));
        let mut f = Vec::new();
        assert!(template_fields(&serde_json::json!({"a": "$.content", "b": ["$.chunks[0]"]}), &mut f).is_some());
        assert_eq!(f, ["content", "chunks"]);
        assert!(template_fields(&serde_json::json!({"a": "$"}), &mut f).is_none());
    }
}
