//! Anthropic Messages API (streaming SSE). PDFs and images are attached inline as base64
//! `document` / `image` blocks; structured extraction uses `output_config.format`.

use super::{
    B64_PLACEHOLDER, CallError, DocRef, Endpoint, LlmRequest, LlmResponse, Usage, http_error, map_reqwest, read_sse, streamed_json_body,
};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

const VERSION: &str = "2023-06-01";

/// Models (per endpoint) that rejected `output_config.effort`; later calls skip it.
fn no_effort() -> &'static Mutex<HashSet<String>> {
    static SET: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    SET.get_or_init(Default::default)
}

/// Schema in the form `output_config.format` accepts: every object closed with
/// `additionalProperties: false`, and the constraint keywords it rejects removed (the caller
/// still validates the answer against the original schema).
pub fn strict_schema(schema: &Value) -> Value {
    const UNSUPPORTED: [&str; 9] =
        ["minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum", "multipleOf", "minLength", "maxLength", "maxItems", "uniqueItems"];
    match schema {
        Value::Object(m) => {
            let mut out = serde_json::Map::new();
            for (k, v) in m {
                match (k.as_str(), v) {
                    // Keys under these are property names, not keywords: keep them all.
                    ("properties" | "$defs" | "definitions", Value::Object(named)) => {
                        out.insert(k.clone(), Value::Object(named.iter().map(|(n, s)| (n.clone(), strict_schema(s))).collect()));
                    }
                    (key, _) if UNSUPPORTED.contains(&key) => {}
                    _ => {
                        out.insert(k.clone(), strict_schema(v));
                    }
                }
            }
            let is_object = match out.get("type") {
                Some(Value::String(t)) => t == "object",
                Some(Value::Array(ts)) => ts.iter().any(|t| t == "object"),
                _ => out.contains_key("properties"),
            };
            if is_object {
                out.insert("additionalProperties".into(), Value::Bool(false));
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(strict_schema).collect()),
        v => v.clone(),
    }
}

fn request_body(ep: &Endpoint, req: &LlmRequest, effort: bool, format: bool) -> (Value, Option<PathBuf>) {
    let mut content = Vec::new();
    let mut stream_path = None;
    // The document goes before the instruction text.
    if let Some(DocRef::Inline { path, mime, .. }) = &req.doc {
        let kind = if mime.starts_with("image/") { "image" } else { "document" };
        content.push(json!({"type": kind, "source": {"type": "base64", "media_type": mime, "data": B64_PLACEHOLDER}}));
        stream_path = Some(path.clone());
    }
    content.push(json!({"type": "text", "text": req.user}));
    let mut body = json!({
        "model": ep.model,
        "max_tokens": req.max_output_tokens,
        "stream": true,
        "system": req.system,
        "messages": [{"role": "user", "content": content}],
    });
    let mut cfg = serde_json::Map::new();
    if effort {
        // Thinking is on by default on current models and billed as output: transcription and
        // summaries run at low effort, structured extraction a step higher.
        cfg.insert("effort".into(), json!(if req.schema.is_some() { "medium" } else { "low" }));
    }
    if let (true, Some(schema)) = (format, &req.schema) {
        cfg.insert("format".into(), json!({"type": "json_schema", "schema": strict_schema(schema)}));
    }
    if !cfg.is_empty() {
        body["output_config"] = Value::Object(cfg);
    }
    (body, stream_path)
}

async fn post(client: &reqwest::Client, ep: &Endpoint, body: &Value, stream_path: Option<PathBuf>) -> Result<reqwest::Response, CallError> {
    let mut rb = client
        .post(format!("{}/messages", ep.base_url))
        .header("content-type", "application/json")
        .header("anthropic-version", VERSION)
        .timeout(Duration::from_millis(ep.cfg.timeout_ms));
    if !ep.api_key.expose().is_empty() {
        rb = rb.header("x-api-key", ep.api_key.expose());
    }
    rb.body(streamed_json_body(body, stream_path)).send().await.map_err(map_reqwest)
}

pub async fn call(client: &reqwest::Client, ep: &Endpoint, req: &LlmRequest) -> Result<LlmResponse, CallError> {
    let model_key = format!("{}|{}", ep.base_url, ep.model);
    let mut effort = !no_effort().lock().unwrap().contains(&model_key);
    let mut format = req.schema.is_some();
    let resp = loop {
        let (body, stream_path) = request_body(ep, req, effort, format);
        let resp = post(client, ep, &body, stream_path).await?;
        let status = resp.status().as_u16();
        if (200..300).contains(&status) {
            break resp;
        }
        let headers = resp.headers().clone();
        let text = resp.text().await.unwrap_or_default();
        let lower = text.to_ascii_lowercase();
        // Older models reject `effort`, and some schemas can't be compiled for structured
        // outputs: drop the rejected part and send again. Extraction is validated by the caller
        // either way.
        if status == 400 && effort && lower.contains("effort") {
            no_effort().lock().unwrap().insert(model_key.clone());
            effort = false;
            continue;
        }
        if status == 400 && format && (lower.contains("output_config") || lower.contains("schema") || lower.contains("format")) {
            format = false;
            continue;
        }
        return Err(http_error(status, &headers, &text));
    };
    let header_id = resp.headers().get("request-id").and_then(|v| v.to_str().ok()).map(str::to_string);
    let mut out = LlmResponse { provider_request_id: header_id, ..Default::default() };
    let (mut input, mut cache_read, mut cache_write, mut output) = (None, None, None, None);
    let mut refusal: Option<String> = None;
    read_sse(resp.bytes_stream(), |data| {
        let v: Value = serde_json::from_str(data).map_err(|e| CallError::Protocol(e.to_string()))?;
        let usage = |u: &Value, k: &str| u.get(k).and_then(Value::as_u64);
        match v.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                if out.provider_request_id.is_none() {
                    out.provider_request_id = v.pointer("/message/id").and_then(Value::as_str).map(str::to_string);
                }
                if let Some(u) = v.pointer("/message/usage") {
                    input = usage(u, "input_tokens");
                    cache_read = usage(u, "cache_read_input_tokens");
                    cache_write = usage(u, "cache_creation_input_tokens");
                    output = usage(u, "output_tokens");
                }
            }
            Some("content_block_delta") => {
                // Only answer text is output; thinking deltas are skipped.
                if v.pointer("/delta/type").and_then(Value::as_str) == Some("text_delta")
                    && let Some(t) = v.pointer("/delta/text").and_then(Value::as_str)
                {
                    out.text.push_str(t);
                }
            }
            Some("message_delta") => {
                match v.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    Some("max_tokens") => out.truncated = true,
                    Some("refusal") => {
                        let category = v.pointer("/delta/stop_details/category").and_then(Value::as_str).unwrap_or("policy");
                        refusal = Some(category.to_string());
                    }
                    _ => {}
                }
                if let Some(u) = v.get("usage") {
                    output = usage(u, "output_tokens").or(output);
                    input = usage(u, "input_tokens").or(input);
                    cache_read = usage(u, "cache_read_input_tokens").or(cache_read);
                    cache_write = usage(u, "cache_creation_input_tokens").or(cache_write);
                }
            }
            Some("error") => {
                let kind = v.pointer("/error/type").and_then(Value::as_str).unwrap_or("");
                let message: String =
                    v.pointer("/error/message").and_then(Value::as_str).unwrap_or("stream error").chars().take(200).collect();
                return Err(match kind {
                    "overloaded_error" => CallError::Http { status: 529, retry_after: None, message },
                    "rate_limit_error" => CallError::Http { status: 429, retry_after: None, message },
                    "api_error" => CallError::Http { status: 500, retry_after: None, message },
                    _ => CallError::Protocol(message),
                });
            }
            _ => {}
        }
        Ok(true)
    })
    .await?;
    out.usage = normalize_usage(input, cache_read, cache_write, output);
    if let Some(category) = refusal {
        return Err(CallError::Protocol(format!("the model declined this request (refusal: {category})")));
    }
    Ok(out)
}

/// Anthropic reports cache reads and writes outside `input_tokens`; fold them in so input
/// means "everything sent" and cached tokens are a subset of it, as for the other providers.
pub fn normalize_usage(input: Option<u64>, cache_read: Option<u64>, cache_write: Option<u64>, output: Option<u64>) -> Usage {
    let input = input.map(|i| i + cache_read.unwrap_or(0) + cache_write.unwrap_or(0));
    Usage {
        input_tokens: input,
        output_tokens: output,
        total_tokens: match (input, output) {
            (Some(i), Some(o)) => Some(i + o),
            _ => None,
        },
        cached_tokens: cache_read,
        reasoning_tokens: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_normalization() {
        let u = normalize_usage(Some(100), Some(900), Some(50), Some(40));
        assert_eq!((u.input_tokens, u.cached_tokens, u.output_tokens, u.total_tokens), (Some(1050), Some(900), Some(40), Some(1090)));
        assert_eq!(normalize_usage(None, None, None, None).total_tokens, None);
    }

    #[test]
    fn strict_schema_closes_objects_and_drops_unsupported() {
        let s = json!({
            "type": "object",
            "required": ["total"],
            "properties": {
                "total": {"type": "number", "minimum": 0},
                "maximum": {"type": "number"},
                "note": {"type": ["string", "null"], "maxLength": 80, "description": "kept"},
                "items": {"type": "array", "maxItems": 5, "items": {"type": "object", "properties": {"qty": {"type": "integer"}}}}
            }
        });
        let out = strict_schema(&s);
        assert_eq!(out["additionalProperties"], false);
        assert_eq!(out["properties"]["items"]["items"]["additionalProperties"], false);
        assert!(out["properties"]["total"].get("minimum").is_none());
        assert!(out["properties"]["note"].get("maxLength").is_none());
        assert_eq!(out["properties"]["note"]["description"], "kept");
        assert!(out["properties"]["items"].get("maxItems").is_none());
        assert_eq!(out["required"], json!(["total"]));
        assert_eq!(out["properties"]["maximum"]["type"], "number", "a property named like a keyword is kept");
        assert!(out["properties"]["total"].get("additionalProperties").is_none());
    }
}
