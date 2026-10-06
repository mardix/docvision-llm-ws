//! OpenAI Chat Completions (streaming) + Files API. Also used by OpenAI-compatible endpoints.

use super::{
    B64_PLACEHOLDER, CallError, DocRef, Endpoint, LlmRequest, LlmResponse, Usage, http_error, map_reqwest, read_sse, streamed_json_body,
};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;

fn auth(rb: reqwest::RequestBuilder, ep: &Endpoint) -> reqwest::RequestBuilder {
    if ep.api_key.expose().is_empty() { rb } else { rb.bearer_auth(ep.api_key.expose()) }
}

/// `native_params`: OpenAI proper uses `max_completion_tokens`; compatible APIs use `max_tokens`.
pub async fn call(client: &reqwest::Client, ep: &Endpoint, req: &LlmRequest, native_params: bool) -> Result<LlmResponse, CallError> {
    let mut content = vec![json!({"type": "text", "text": req.user})];
    let mut stream_path = None;
    match &req.doc {
        Some(DocRef::Inline { path, mime, file_name }) => {
            if mime.starts_with("image/") {
                content.push(json!({"type": "image_url", "image_url": {"url": format!("data:{mime};base64,{B64_PLACEHOLDER}")}}));
            } else {
                content.push(
                    json!({"type": "file", "file": {"filename": file_name, "file_data": format!("data:{mime};base64,{B64_PLACEHOLDER}")}}),
                );
            }
            stream_path = Some(path.clone());
        }
        Some(DocRef::Uploaded { id, .. }) => content.push(json!({"type": "file", "file": {"file_id": id}})),
        None => {}
    }
    let mut body = json!({
        "model": ep.model,
        "stream": true,
        "stream_options": {"include_usage": true},
        // System prompt first and fixed, so provider prompt caching can reuse the prefix.
        "messages": [
            {"role": "system", "content": req.system},
            {"role": "user", "content": content},
        ],
    });
    let key = if native_params { "max_completion_tokens" } else { "max_tokens" };
    body[key] = json!(req.max_output_tokens);
    match (&req.schema, native_params) {
        (Some(schema), true) => {
            body["response_format"] =
                json!({"type": "json_schema", "json_schema": {"name": "extraction", "schema": schema, "strict": false}});
        }
        _ if req.json || req.schema.is_some() => body["response_format"] = json!({"type": "json_object"}),
        _ => {}
    }
    let rb = client
        .post(format!("{}/chat/completions", ep.base_url))
        .header("content-type", "application/json")
        .timeout(Duration::from_millis(ep.cfg.timeout_ms))
        .body(streamed_json_body(&body, stream_path));
    let resp = auth(rb, ep).send().await.map_err(map_reqwest)?;
    let status = resp.status().as_u16();
    let request_id = resp.headers().get("x-request-id").and_then(|v| v.to_str().ok()).map(str::to_string);
    if !(200..300).contains(&status) {
        let headers = resp.headers().clone();
        let text = resp.text().await.unwrap_or_default();
        return Err(http_error(status, &headers, &text));
    }
    let mut out = LlmResponse { provider_request_id: request_id, ..Default::default() };
    read_sse(resp.bytes_stream(), |data| {
        let v: Value = serde_json::from_str(data).map_err(|e| CallError::Protocol(e.to_string()))?;
        if let Some(err) = v.get("error") {
            return Err(CallError::Protocol(
                err.get("message").and_then(Value::as_str).unwrap_or("stream error").chars().take(200).collect(),
            ));
        }
        if out.provider_request_id.is_none() {
            out.provider_request_id = v.get("id").and_then(Value::as_str).map(str::to_string);
        }
        if let Some(choice) = v.pointer("/choices/0") {
            if let Some(t) = choice.pointer("/delta/content").and_then(Value::as_str) {
                out.text.push_str(t);
            }
            if choice.get("finish_reason").and_then(Value::as_str) == Some("length") {
                out.truncated = true;
            }
        }
        if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
            out.usage = parse_usage(u);
        }
        Ok(true)
    })
    .await?;
    Ok(out)
}

pub fn parse_usage(u: &Value) -> Usage {
    let g = |p: &str| u.pointer(p).and_then(Value::as_u64);
    let input = g("/prompt_tokens");
    let output = g("/completion_tokens");
    Usage {
        input_tokens: input,
        output_tokens: output,
        total_tokens: g("/total_tokens").or_else(|| Some(input? + output?)),
        // Subsets of input/output respectively; never added to totals.
        cached_tokens: g("/prompt_tokens_details/cached_tokens"),
        reasoning_tokens: g("/completion_tokens_details/reasoning_tokens"),
    }
}

pub async fn upload(client: &reqwest::Client, ep: &Endpoint, path: PathBuf, mime: &'static str) -> Result<DocRef, CallError> {
    let file = tokio::fs::File::open(&path).await.map_err(|e| CallError::Network(e.to_string()))?;
    let len = file.metadata().await.map(|m| m.len()).unwrap_or(0);
    let stream = tokio_util::io::ReaderStream::with_capacity(file, 64 * 1024);
    let part = reqwest::multipart::Part::stream_with_length(reqwest::Body::wrap_stream(stream), len)
        .file_name("document.pdf")
        .mime_str(mime)
        .map_err(|e| CallError::Protocol(e.to_string()))?;
    let form = reqwest::multipart::Form::new().text("purpose", "user_data").part("file", part);
    let rb = client.post(format!("{}/files", ep.base_url)).multipart(form).timeout(Duration::from_millis(ep.cfg.timeout_ms));
    let resp = auth(rb, ep).send().await.map_err(map_reqwest)?;
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let text = resp.text().await.map_err(map_reqwest)?;
    if !(200..300).contains(&status) {
        return Err(http_error(status, &headers, &text));
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| CallError::Protocol(e.to_string()))?;
    let id = v.get("id").and_then(Value::as_str).ok_or_else(|| CallError::Protocol("upload response missing id".into()))?;
    Ok(DocRef::Uploaded { id: id.to_string(), mime })
}

pub async fn delete(client: &reqwest::Client, ep: &Endpoint, id: &str) -> Result<(), CallError> {
    let rb = client.delete(format!("{}/files/{id}", ep.base_url)).timeout(Duration::from_secs(30));
    auth(rb, ep).send().await.map_err(map_reqwest)?;
    Ok(())
}
