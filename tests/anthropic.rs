//! Anthropic provider: request shape, streaming, structured outputs, fallbacks and refusals.

mod common;

use common::*;
use serde_json::{Value, json};
use std::sync::atomic::Ordering::SeqCst;

async fn anthropic_server() -> TestServer {
    server_with(None, None, |v| {
        v.insert("DOCVISION_LLM_PROVIDER".into(), "anthropic".into());
    })
    .await
}

fn receipt_schema() -> Value {
    json!({
        "type": "object",
        "required": ["merchant", "total"],
        "properties": {
            "merchant": {"type": "string", "maxLength": 80},
            "total": {"type": "number", "minimum": 0},
            "items": {"type": "array", "items": {"type": "object", "properties": {"name": {"type": "string"}}}}
        }
    })
}

#[tokio::test]
async fn converts_pdf_and_enriches() {
    let s = anthropic_server().await;
    let p = s.write("a.pdf", &fixtures::pdf(3));
    let (st, v) = s.convert_sync(&p, json!({})).await;
    assert_eq!(st, 200, "{v}");
    let d = &v["data"];
    let c = d["content"].as_str().unwrap();
    assert!((1..=3).all(|i| c.contains(&format!("## Page {i}\n"))), "{c}");
    assert_eq!(d["title"], "Mock Title", "title/summary JSON is parsed without a provider JSON mode");
    assert_eq!(d["llm"]["totals"]["provider"], "anthropic");
    assert_eq!(d["llm"]["totals"]["model"], "mock-model");
    // Usage: cache reads are folded into input and reported as a subset of it.
    let call = &d["llm"]["calls"][0];
    assert_eq!(call["status"], "ok");
    assert_eq!(call["cached_tokens"], 7);
    assert!(call["input_tokens"].as_u64().unwrap() > 7);
    assert_eq!(call["total_tokens"].as_u64().unwrap(), call["input_tokens"].as_u64().unwrap() + call["output_tokens"].as_u64().unwrap());
    assert_eq!(call["provider_request_id"], "req_mock");
    // Request shape: key header, version header, low effort for transcription, PDF attached inline.
    let seen = s.mock.anthropic_seen.lock().unwrap().clone();
    assert!(!seen.is_empty());
    for (key, version, _) in &seen {
        assert_eq!(key, "sk-mock-secret");
        assert_eq!(version, "2023-06-01");
    }
    assert_eq!(seen[0].2["effort"], "low");
    assert_eq!(s.mock.pdf_pages_seen.lock().unwrap().clone(), vec![3], "the PDF went inline as a document block");
    assert_eq!(s.mock.uploads.load(SeqCst), 0);
    assert!(!v.to_string().contains("sk-mock-secret"));
}

#[tokio::test]
async fn truncated_output_splits_the_batch() {
    let s = anthropic_server().await;
    s.mock.truncate_multi_page.store(true, SeqCst);
    let p = s.write("t.pdf", &fixtures::pdf(4));
    let (st, v) = s.convert_sync(&p, json!({"gen_title": false, "gen_summary": false})).await;
    assert_eq!(st, 200, "{v}");
    let c = v["data"]["content"].as_str().unwrap();
    assert!((1..=4).all(|i| c.contains(&format!("## Page {i}\n"))), "{c}");
    let calls = v["data"]["llm"]["calls"].as_array().unwrap();
    assert_eq!(calls.iter().filter(|c| c["status"] == "truncated").count(), 3, "stop_reason max_tokens is detected");
}

#[tokio::test]
async fn structured_extraction_uses_output_config_format() {
    let s = anthropic_server().await;
    let p = s.write("r.md", b"# Corner Cafe\n\nTotal: 9.00\n");
    let opts = json!({"extract_schema": receipt_schema(), "title_method": "local", "summary_method": "local"});
    let (st, v) = s.convert_sync(&p, opts).await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["data"]["extracted"]["merchant"], "mock");
    assert_eq!(v["data"]["feature_status"]["extraction"]["status"], "completed");
    let seen = s.mock.anthropic_seen.lock().unwrap().clone();
    let cfg = &seen.last().unwrap().2;
    assert_eq!(cfg["effort"], "medium");
    let schema = &cfg["format"]["schema"];
    assert_eq!(cfg["format"]["type"], "json_schema");
    assert_eq!(schema["additionalProperties"], false, "objects are closed for structured outputs");
    assert_eq!(schema["properties"]["items"]["items"]["additionalProperties"], false);
    assert!(schema["properties"]["total"].get("minimum").is_none() && schema["properties"]["merchant"].get("maxLength").is_none());
}

#[tokio::test]
async fn falls_back_when_effort_or_format_is_rejected() {
    let s = anthropic_server().await;
    s.mock.anthropic_reject_effort.store(true, SeqCst);
    s.mock.anthropic_reject_format.store(true, SeqCst);
    let p = s.write("r.md", b"# Corner Cafe\n\nTotal: 9.00\n");
    let opts = json!({"extract_schema": receipt_schema(), "title_method": "local", "summary_method": "local"});
    let (st, v) = s.convert_sync(&p, opts.clone()).await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["data"]["extracted"]["total"], 42.5, "extraction still works from the prompt alone");
    // effort+format rejected, then format rejected, then a plain request succeeds.
    let seen = s.mock.anthropic_seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 3, "{seen:?}");
    assert!(seen[2].2.is_null(), "the last attempt carries no output_config");
    // The model is remembered as not taking `effort`: the next call doesn't send it.
    let (st, _) = s
        .convert_sync(
            &p,
            json!({"extract_schema": receipt_schema(), "cache": "bypass", "title_method": "local", "summary_method": "local"}),
        )
        .await;
    assert_eq!(st, 200);
    let seen = s.mock.anthropic_seen.lock().unwrap().clone();
    assert!(seen[3].2.get("effort").is_none(), "{:?}", seen[3]);
}

#[tokio::test]
async fn refusal_is_a_clear_error() {
    let s = anthropic_server().await;
    s.mock.anthropic_refuse.store(true, SeqCst);
    let p = s.write("a.pdf", &fixtures::pdf(1));
    let (st, v) = s.convert_sync(&p, json!({"gen_title": false, "gen_summary": false})).await;
    assert_eq!(st, 502, "{v}");
    assert_eq!(v["error"]["code"], "provider_error");
    assert!(v["error"]["message"].as_str().unwrap().contains("declined this request (refusal: cyber)"), "{v}");
    // A refusal is not retried.
    assert_eq!(s.mock.anthropic_seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn overload_is_retried() {
    let s = anthropic_server().await;
    // The next two requests get `529 overloaded_error`; the third attempt succeeds.
    s.mock.anthropic_overload_next.store(2, SeqCst);
    let p = s.write("r.md", b"# Notes\n\nSome text for a summary.\n");
    let (st, v) = s.convert_sync(&p, json!({})).await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["data"]["title"], "Mock Title");
    let calls = v["data"]["llm"]["calls"].as_array().unwrap();
    let statuses: Vec<&str> = calls.iter().map(|c| c["status"].as_str().unwrap()).collect();
    assert_eq!(statuses, ["http_error", "http_error", "ok"], "each attempt is recorded");
    assert!(calls[0]["error"].as_str().unwrap().contains("529"));
    assert_eq!(s.mock.errors_5xx.load(SeqCst), 2);
}
