//! Google Gemini `streamGenerateContent` (SSE) + resumable Files API upload.

use super::{
    B64_PLACEHOLDER, CallError, DocRef, Endpoint, LlmRequest, LlmResponse, Usage, http_error, map_reqwest, read_sse, streamed_json_body,
};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;

pub async fn call(client: &reqwest::Client, ep: &Endpoint, req: &LlmRequest) -> Result<LlmResponse, CallError> {
    let mut parts = Vec::new();
    let mut stream_path = None;
    match &req.doc {
        Some(DocRef::Inline { path, mime, .. }) => {
            parts.push(json!({"inline_data": {"mime_type": mime, "data": B64_PLACEHOLDER}}));
            stream_path = Some(path.clone());
        }
        Some(DocRef::Uploaded { id, mime }) => parts.push(json!({"file_data": {"mime_type": mime, "file_uri": id}})),
        None => {}
    }
    parts.push(json!({"text": req.user}));
    let mut gen_cfg = json!({"maxOutputTokens": req.max_output_tokens});
    if req.json || req.schema.is_some() {
        gen_cfg["responseMimeType"] = json!("application/json");
    }
    let body = json!({
        "systemInstruction": {"parts": [{"text": req.system}]},
        "contents": [{"role": "user", "parts": parts}],
        "generationConfig": gen_cfg,
    });
    let resp = client
        .post(format!("{}/models/{}:streamGenerateContent?alt=sse", ep.base_url, ep.model))
        .header("x-goog-api-key", ep.api_key.expose())
        .header("content-type", "application/json")
        .timeout(Duration::from_millis(ep.cfg.timeout_ms))
        .body(streamed_json_body(&body, stream_path))
        .send()
        .await
        .map_err(map_reqwest)?;
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        let headers = resp.headers().clone();
        let text = resp.text().await.unwrap_or_default();
        return Err(http_error(status, &headers, &text));
    }
    let mut out = LlmResponse::default();
    read_sse(resp.bytes_stream(), |data| {
        let v: Value = serde_json::from_str(data).map_err(|e| CallError::Protocol(e.to_string()))?;
        if out.provider_request_id.is_none() {
            out.provider_request_id = v.get("responseId").and_then(Value::as_str).map(str::to_string);
        }
        if let Some(c) = v.pointer("/candidates/0") {
            if let Some(ps) = c.pointer("/content/parts").and_then(Value::as_array) {
                for p in ps {
                    // Skip thought summaries; only answer text is output.
                    if p.get("thought").and_then(Value::as_bool) == Some(true) {
                        continue;
                    }
                    if let Some(t) = p.get("text").and_then(Value::as_str) {
                        out.text.push_str(t);
                    }
                }
            }
            if c.get("finishReason").and_then(Value::as_str) == Some("MAX_TOKENS") {
                out.truncated = true;
            }
        }
        if let Some(u) = v.get("usageMetadata") {
            out.usage = parse_usage(u);
        }
        Ok(true)
    })
    .await?;
    Ok(out)
}

/// Gemini reports thoughts separately from candidates; normalize so output includes reasoning
/// (and reasoning stays a subset), matching OpenAI semantics.
pub fn parse_usage(u: &Value) -> Usage {
    let g = |k: &str| u.get(k).and_then(Value::as_u64);
    let input = g("promptTokenCount");
    let cand = g("candidatesTokenCount");
    let thoughts = g("thoughtsTokenCount");
    let output = match (cand, thoughts) {
        (Some(c), t) => Some(c + t.unwrap_or(0)),
        (None, Some(t)) => Some(t),
        (None, None) => None,
    };
    Usage {
        input_tokens: input,
        output_tokens: output,
        total_tokens: g("totalTokenCount"),
        cached_tokens: g("cachedContentTokenCount"),
        reasoning_tokens: thoughts,
    }
}

fn upload_base(base: &str) -> String {
    match base.rfind("/v1") {
        Some(i) => format!("{}/upload{}", &base[..i], &base[i..]),
        None => format!("{base}/upload"),
    }
}

pub async fn upload(client: &reqwest::Client, ep: &Endpoint, path: PathBuf, mime: &'static str, size: u64) -> Result<DocRef, CallError> {
    let start = client
        .post(format!("{}/files", upload_base(&ep.base_url)))
        .header("x-goog-api-key", ep.api_key.expose())
        .header("X-Goog-Upload-Protocol", "resumable")
        .header("X-Goog-Upload-Command", "start")
        .header("X-Goog-Upload-Header-Content-Length", size.to_string())
        .header("X-Goog-Upload-Header-Content-Type", mime)
        .header("content-type", "application/json")
        .body(json!({"file": {"display_name": "docvision-upload"}}).to_string())
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .map_err(map_reqwest)?;
    let status = start.status().as_u16();
    if !(200..300).contains(&status) {
        let headers = start.headers().clone();
        return Err(http_error(status, &headers, &start.text().await.unwrap_or_default()));
    }
    let url = start
        .headers()
        .get("x-goog-upload-url")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| CallError::Protocol("missing upload URL".into()))?
        .to_string();
    let file = tokio::fs::File::open(&path).await.map_err(|e| CallError::Network(e.to_string()))?;
    let resp = client
        .post(url)
        .header("Content-Length", size.to_string())
        .header("X-Goog-Upload-Offset", "0")
        .header("X-Goog-Upload-Command", "upload, finalize")
        .body(reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::with_capacity(file, 64 * 1024)))
        .timeout(Duration::from_millis(ep.cfg.timeout_ms))
        .send()
        .await
        .map_err(map_reqwest)?;
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let text = resp.text().await.map_err(map_reqwest)?;
    if !(200..300).contains(&status) {
        return Err(http_error(status, &headers, &text));
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| CallError::Protocol(e.to_string()))?;
    let uri = v
        .pointer("/file/uri")
        .and_then(Value::as_str)
        .ok_or_else(|| CallError::Protocol("upload response missing uri".into()))?
        .to_string();
    let name = v.pointer("/file/name").and_then(Value::as_str).unwrap_or("").to_string();
    // Wait briefly for the file to become ACTIVE.
    let mut state = v.pointer("/file/state").and_then(Value::as_str).unwrap_or("ACTIVE").to_string();
    for _ in 0..30 {
        if state != "PROCESSING" || name.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        let r = client
            .get(format!("{}/{name}", ep.base_url))
            .header("x-goog-api-key", ep.api_key.expose())
            .send()
            .await
            .map_err(map_reqwest)?;
        let body = r.bytes().await.map_err(map_reqwest)?;
        let v: Value = serde_json::from_slice(&body).map_err(|e| CallError::Protocol(e.to_string()))?;
        state = v.get("state").and_then(Value::as_str).unwrap_or("ACTIVE").to_string();
    }
    if state == "FAILED" {
        return Err(CallError::Protocol("provider failed to process the uploaded file".into()));
    }
    Ok(DocRef::Uploaded { id: uri, mime })
}

pub async fn delete(client: &reqwest::Client, ep: &Endpoint, uri: &str) -> Result<(), CallError> {
    // The file URI ends with `files/<id>`.
    if let Some(i) = uri.find("files/") {
        let _ = client
            .delete(format!("{}/{}", ep.base_url, &uri[i..]))
            .header("x-goog-api-key", ep.api_key.expose())
            .timeout(Duration::from_secs(30))
            .send()
            .await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn usage_normalization() {
        let u =
            parse_usage(&json!({"promptTokenCount": 100, "candidatesTokenCount": 50, "thoughtsTokenCount": 20, "totalTokenCount": 170}));
        assert_eq!(u.output_tokens, Some(70));
        assert_eq!(u.reasoning_tokens, Some(20));
        assert_eq!(u.total_tokens, Some(170));
        let u = parse_usage(&json!({}));
        assert_eq!(u.input_tokens, None);
        assert_eq!(u.source(), "missing");
        assert_eq!(upload_base("https://g.com/v1beta"), "https://g.com/upload/v1beta");
    }
}
