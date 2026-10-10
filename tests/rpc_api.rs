//! RPC envelope, auth, rejection paths, status codes, native formats (sync).

mod common;

use common::*;
use serde_json::{Value, json};

#[tokio::test]
async fn auth_and_envelope() {
    let s = server().await;
    // Missing / wrong token -> 401.
    let r = s.client.post(format!("{}/rpc", s.url)).json(&json!({"operation": "health"})).send().await.unwrap();
    assert_eq!(r.status(), 401);
    let r = s
        .client
        .post(format!("{}/rpc", s.url))
        .header("x-access-token", "nope")
        .json(&json!({"operation": "health"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "unauthorized");
    assert!(v["request_id"].is_string());
    // /livez needs no token; /metrics does.
    assert_eq!(s.client.get(format!("{}/livez", s.url)).send().await.unwrap().status(), 200);
    assert_eq!(s.client.get(format!("{}/metrics", s.url)).send().await.unwrap().status(), 401);
    let m = s.client.get(format!("{}/metrics", s.url)).header("x-access-token", TOKEN).send().await.unwrap();
    assert_eq!(m.status(), 200);
    assert!(m.text().await.unwrap().contains("docvision_queue_depth"));
    // Envelope shape.
    let (st, v) = s.rpc("health", json!({}), json!({})).await;
    assert_eq!(st, 200);
    assert_eq!(v["ok"], true);
    assert_eq!(v["operation"], "health");
    assert!(v["error"].is_null());
    assert!(v["data"]["features"].as_array().unwrap().iter().any(|f| f == "core"));
    assert!(!v.to_string().contains("sk-mock-secret"));
    assert_eq!(s.rpc("capabilities", json!({}), json!({})).await.0, 400, "capabilities operation was removed");
    let (st, v) = s.rpc("health", json!({}), json!({})).await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["data"]["db_writer"]["alive"], true);
}

#[tokio::test]
async fn rejections() {
    let s = server().await;
    // Unknown operation -> 400; invalid option -> 400; unknown option field -> 400.
    assert_eq!(s.rpc("nope", json!({}), json!({})).await.0, 400);
    assert_eq!(s.rpc("convert", json!({"source": "/x.pdf"}), json!({"ocr": "maybe"})).await.0, 400);
    assert_eq!(s.rpc("convert", json!({"source": "/x.pdf"}), json!({"bogus": 1})).await.0, 400);
    let (st, v) = s.rpc("convert", json!({"source": "/x.pdf"}), json!({"ocr": "auto", "pdf_skip_local_processing": true})).await;
    assert_eq!(st, 400);
    assert_eq!(v["error"]["code"], "invalid_options");
    assert!(v["data"].is_null());
    // Upload-like fields anywhere -> 415.
    for payload in [
        json!({"source": "/a", "file": "x"}),
        json!({"source": "data:application/pdf;base64,AAAA"}),
        json!({"source": "/a", "metadata": {"x": {"content_base64": "AA"}}}),
    ] {
        let (st, v) = s.rpc("convert", payload, json!({})).await;
        assert_eq!(st, 415, "{v}");
    }
    let r = s
        .client
        .post(format!("{}/rpc", s.url))
        .header("x-access-token", TOKEN)
        .header("content-type", "multipart/form-data; boundary=x")
        .body("--x--")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 415);
    // Sync + destination/webhook -> 400.
    let (st, _) = s.rpc("convert", json!({"source": "/a.md", "destination": "/tmp/x.md"}), json!({"execution": "sync"})).await;
    assert_eq!(st, 400);
    // Relative path / non-https -> 400.
    assert_eq!(s.rpc("convert", json!({"source": "relative.pdf"}), json!({})).await.0, 400);
    // Pseudo-filesystems (process environment, devices) are never read.
    let (st, v) = s.convert_sync("/dev/null", json!({})).await;
    assert_eq!(st, 400, "{v}");
    assert_eq!(v["error"]["code"], "invalid_source");
    // Body over 256 KB on a non-text operation -> 413.
    let big = "x".repeat(300 << 10);
    assert_eq!(s.rpc("convert", json!({"source": "/a.md", "metadata": {"pad": big}}), json!({})).await.0, 413);
    // Feature not compiled.
    if !cfg!(feature = "translate") {
        let (st, v) = s.rpc("convert", json!({"source": "/a.md"}), json!({"translate_to": "fr"})).await;
        assert_eq!(st, 400);
        assert_eq!(v["error"]["code"], "feature_not_compiled");
    }
    // Unknown job -> 404.
    assert_eq!(s.rpc("job.get", json!({"job_id": "nope"}), json!({})).await.0, 404);
    assert_eq!(s.rpc("history.get", json!({"request_id": "nope"}), json!({})).await.0, 404);
    // Missing file -> 404 with stage.
    let (st, v) = s.convert_sync("/definitely/not/here.md", json!({})).await;
    assert_eq!(st, 404, "{v}");
    assert_eq!(v["error"]["stage"], "fetch");
}

#[tokio::test]
async fn legacy_office_is_415() {
    let s = server().await;
    for name in ["old.doc", "old.xls", "old.ppt"] {
        let p = s.write(name, &fixtures::legacy_doc());
        let (st, v) = s.convert_sync(&p, json!({})).await;
        assert_eq!(st, 415, "{v}");
        assert_eq!(v["error"]["code"], "unsupported_format");
        assert!(v["error"]["message"].as_str().unwrap().contains("modern"));
    }
}

fn local() -> Value {
    json!({"summary_method": "local", "title_method": "local"})
}

#[tokio::test]
async fn native_formats_sync() {
    let s = server().await;
    // DOCX
    let p = s.write("report.docx", &fixtures::docx(3));
    let (st, v) = s.convert_sync(&p, local()).await;
    assert_eq!(st, 200, "{v}");
    let d = &v["data"];
    let c = d["content"].as_str().unwrap();
    assert!(c.starts_with("# Quarterly Report\n"), "{c}");
    assert!(c.contains("# Section 0\n"));
    assert!(c.contains("**bold text**"));
    assert!(c.contains("- Bullet one\n- Bullet two\n"));
    assert!(c.contains("1. Step one\n2. Step two\n"));
    assert!(c.contains("| Name | Value |\n| --- | --- |\n| Alpha | 42 |"));
    assert!(c.contains("[the website](https://example.com/)"));
    assert_eq!(d["format"], "docx");
    assert_eq!(d["title"], "Docx Meta Title");
    assert!(d["summary"].as_str().unwrap().starts_with("Extractive summary:"));
    assert!(d["job_id"].is_null());
    assert!(d["dest_file"].is_null() && d["md_file"].is_null() && d["docv_file"].is_null() && d["docv_bytes"].is_null());
    assert_eq!(d["src_bytes"].as_u64().unwrap(), std::fs::metadata(&p).unwrap().len(), "the original's size");
    assert_eq!(d["md_bytes"].as_u64().unwrap(), d["content"].as_str().unwrap().len() as u64);
    assert_eq!(d["llm"]["totals"]["calls"], 0);
    assert!(d["statistics"]["total_words"].as_u64().unwrap() > 100);
    assert!(d["statistics"]["original_total_pages"].is_null());
    assert!(!d["chunks"].as_array().unwrap().is_empty());
    assert!(d["timing"]["total_ms"].is_u64());

    // XLSX: sheet names, shared/inline strings, cached formula values, sparse cells.
    let p = s.write("data.xlsx", &fixtures::xlsx(5));
    let (st, v) = s.convert_sync(&p, local()).await;
    assert_eq!(st, 200, "{v}");
    let c = v["data"]["content"].as_str().unwrap();
    assert!(c.contains("## Sales\n"));
    assert!(c.contains("| Region | Revenue | Total |"));
    assert!(c.contains("| North East | 40 | 80 |"), "{c}");
    assert!(c.contains("## Notes\n"));
    assert!(c.contains("|  | Only B |"), "{c}");
    assert_eq!(v["data"]["statistics"]["sheet_count"], 2);
    assert_eq!(v["data"]["chunks"][0]["source_span"]["kind"], "sheet");

    // PPTX: order from sldIdLst, titles, nested bullets, slide count = pages.
    let p = s.write("deck.pptx", &fixtures::pptx(3));
    let (st, v) = s.convert_sync(&p, local()).await;
    assert_eq!(st, 200, "{v}");
    let c = v["data"]["content"].as_str().unwrap();
    let a = c.find("## Slide 1: Title 1").unwrap();
    let b = c.find("## Slide 2: Title 2").unwrap();
    assert!(a < b);
    assert!(c.contains("  - Sub point"));
    assert_eq!(v["data"]["statistics"]["original_total_pages"], 3);
    assert_eq!(v["data"]["statistics"]["original_page_count_exact"], true);
    assert_eq!(v["data"]["statistics"]["slide_count"], 3);

    // HTML and Markdown/text.
    let p = s.write("page.html", fixtures::HTML.as_bytes());
    let (st, v) = s.convert_sync(&p, local()).await;
    assert_eq!(st, 200);
    assert!(v["data"]["content"].as_str().unwrap().contains("Hello *world* & friends."));
    assert_eq!(v["data"]["title"], "Html Title");
    let p = s.write("notes.md", "# Notes\n\nSome   text.  \n\n\n\nMore text here.\n".as_bytes());
    let (st, v) = s.convert_sync(&p, local()).await;
    assert_eq!(st, 200);
    assert_eq!(v["data"]["content"], "# Notes\n\nSome   text.\n\nMore text here.\n");
    assert_eq!(v["data"]["format"], "markdown");
    assert_eq!(v["data"]["statistics"]["markdown_estimated_total_pages"], 1);
}

#[tokio::test]
async fn disabled_features_make_zero_calls() {
    let s = server().await;
    let p = s.write("r.docx", &fixtures::docx(1));
    let (st, v) = s.convert_sync(&p, json!({"gen_summary": false, "gen_title": false, "gen_chunks": false})).await;
    assert_eq!(st, 200, "{v}");
    let d = &v["data"];
    assert!(d["summary"].is_null());
    assert!(d["title"].is_null());
    assert_eq!(d["chunks"], json!([]));
    assert_eq!(d["llm"]["totals"]["calls"], 0);
    assert_eq!(d["feature_status"]["summary"]["status"], "disabled");
    assert_eq!(s.mock.requests.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[tokio::test]
async fn llm_enrichment_combined_call() {
    let s = server().await;
    let p = s.write("r.docx", &fixtures::docx(1));
    let (st, v) = s.convert_sync(&p, json!({})).await;
    assert_eq!(st, 200, "{v}");
    let d = &v["data"];
    assert_eq!(d["title"], "Mock Title");
    assert_eq!(d["summary"], "Mock summary of the document.");
    // Title + summary in ONE call.
    assert_eq!(d["llm"]["totals"]["calls"], 1);
    assert_eq!(s.mock.enrich_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    let call = &d["llm"]["calls"][0];
    assert_eq!(call["status"], "ok");
    assert_eq!(call["usage_source"], "provider");
    assert!(call["input_tokens"].as_u64().unwrap() > 0);
    assert_eq!(call["purpose"], "title_summary_language");
    let txt = v.to_string();
    assert!(!txt.contains("sk-mock-secret"));
}

#[tokio::test]
async fn llm_title_failure_falls_back_to_local() {
    let s = server_with(None, None, |v| {
        v.insert("DOCVISION_LLM_MAX_RETRIES".into(), "0".into());
    })
    .await;
    *s.mock.p5xx.lock().unwrap() = 1.0;
    let p = s.write("r.docx", &fixtures::docx(1));
    let (st, v) = s.convert_sync(&p, json!({})).await;
    assert_eq!(st, 200, "{v}");
    let d = &v["data"];
    assert_eq!(d["title"], "Docx Meta Title");
    assert_eq!(d["feature_status"]["title"]["status"], "completed");
    assert_eq!(d["feature_status"]["title"]["method"], "local_fallback");
    assert!(d["feature_status"]["title"]["error"].is_string());
    assert_eq!(d["feature_status"]["summary"]["status"], "failed");
}

#[tokio::test]
async fn summarize_and_chunk_operations() {
    let s = server().await;
    let md = "# Title\n\nFirst paragraph sentence is here. Another one follows.\n\n## Part\n\nMore.\n";
    let (st, v) = s.rpc("summarize", json!({"source_content": md}), json!({"summary_method": "local", "title_method": "local"})).await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["data"]["title"], "Title");
    assert!(v["data"]["summary"].as_str().unwrap().contains("First paragraph"));
    let (st, v) = s.rpc("summarize", json!({"source_content": md}), json!({})).await;
    assert_eq!(st, 200);
    assert_eq!(v["data"]["title"], "Mock Title");
    let (st, v) = s.rpc("chunk", json!({"source_content": md}), json!({"chunk_size": 16})).await;
    assert_eq!(st, 200, "{v}");
    let chunks = v["data"]["chunks"].as_array().unwrap();
    // A short document is one chunk: tiny sections are merged, never returned on their own.
    assert_eq!(chunks.len(), 1, "{v}");
    assert!(chunks[0]["content"].as_str().unwrap().contains("## Part"));
    assert_eq!(chunks[0]["tokens_estimated"], true);
    // A source that needs conversion is refused without converting.
    let p = s.write("x.docx", &fixtures::docx(1));
    let (st, v) = s.rpc("summarize", json!({"source": p}), json!({})).await;
    assert_eq!(st, 400);
    assert_eq!(v["error"]["code"], "conversion_required");
    // Text sources are accepted.
    let p = s.write("x.md", md.as_bytes());
    let (st, _) = s.rpc("chunk", json!({"source": p}), json!({})).await;
    assert_eq!(st, 200);
}

#[tokio::test]
async fn zip_bomb_is_rejected() {
    let s = server_cfg(|c| {
        c.max_archive_ratio = 2;
        c.max_archive_bytes = 1 << 20;
    })
    .await;
    // Highly compressible 20 MB document part.
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let o = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    w.start_file("word/document.xml", o).unwrap();
    use std::io::Write;
    w.write_all(b"<w:document><w:body><w:p><w:r><w:t>").unwrap();
    w.write_all(&vec![b'a'; 20 << 20]).unwrap();
    w.write_all(b"</w:t></w:r></w:p></w:body></w:document>").unwrap();
    let bytes = w.finish().unwrap().into_inner();
    let p = s.write("bomb.docx", &bytes);
    let (st, v) = s.convert_sync(&p, local()).await;
    assert_eq!(st, 413, "{v}");
    assert_eq!(v["error"]["code"], "archive_too_large");
}

#[tokio::test]
async fn request_defined_providers() {
    // No providers configured at all.
    let s = server_with(None, None, |v| v.retain(|k, _| !k.starts_with("DOCVISION_LLM_"))).await;
    let p = s.write("r.md", b"# Doc\n\nSome text here for the summary.\n");
    // Known provider + model from the request alone: accepted (the call itself then goes to the public API,
    // so only validation is checked here via an unreachable base_url override).
    let (st, v) = s.rpc("convert", json!({"source": p}), json!({"llm_provider": "gemini", "llm_model": "m", "llm_api_key": "k", "llm_base_url": "http://127.0.0.1:1/v1beta", "gen_title": false})).await;
    assert_eq!(st, 202, "{v}");
    // Same, sync, against the mock as an ad-hoc OpenAI-kind endpoint.
    let (st, v) = s.convert_sync(&p, json!({"llm_provider": "openai", "llm_model": "mock-model", "llm_api_key": "k", "llm_base_url": s.mock_url, "gen_summary": false})).await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["data"]["title"], "Mock Title");
    assert_eq!(v["data"]["llm"]["calls"][0]["provider"], "openai");
    // Missing model -> actionable 400.
    let (st, v) = s.rpc("convert", json!({"source": p}), json!({"llm_provider": "gemini", "llm_api_key": "k"})).await;
    assert_eq!(st, 400);
    assert!(v["error"]["message"].as_str().unwrap().contains("llm_model"), "{v}");
    // Unknown name without base_url -> 400.
    assert_eq!(s.rpc("convert", json!({"source": p}), json!({"llm_provider": "foo", "llm_model": "m"})).await.0, 400);
}

#[tokio::test]
async fn flat_options_and_chunk_overlap() {
    let s = server().await;
    // Old nested option names are rejected with a clear 400.
    let (st, v) = s.rpc("chunk", json!({"source_content": "# A\n"}), json!({"chunks": {"target_tokens": 100}})).await;
    assert_eq!(st, 400);
    assert!(v["error"]["message"].as_str().unwrap().contains("unknown field `chunks`"), "{v}");
    // Overlap must be smaller than the chunk size.
    assert_eq!(s.rpc("chunk", json!({"source_content": "# A\n"}), json!({"chunk_size": 200, "chunk_overlap": 200})).await.0, 400);
    // Each chunk after the first starts with the end of the previous one.
    let mut md = String::new();
    for i in 0..40 {
        md.push_str(&format!("Paragraph number {i} has some words in it.\n\n"));
    }
    let (st, v) = s.rpc("chunk", json!({"source_content": md}), json!({"chunk_size": 100, "chunk_overlap": 20})).await;
    assert_eq!(st, 200, "{v}");
    let chunks = v["data"]["chunks"].as_array().unwrap();
    assert!(chunks.len() >= 3);
    for w in chunks.windows(2) {
        let prev = w[0]["content"].as_str().unwrap();
        let first_line = w[1]["content"].as_str().unwrap().lines().next().unwrap();
        assert!(prev.contains(first_line), "overlap line {first_line:?} not in previous chunk");
    }
    // Below the minimum, chunk_size is raised to 128 (overlap 15% of that = 19).
    let (st, v) = s.rpc("chunk", json!({"source_content": md}), json!({"chunk_size": 10})).await;
    assert_eq!(st, 200, "{v}");
    assert!(v["data"]["chunks"].as_array().unwrap().iter().all(|c| c["tokens"].as_u64().unwrap() <= 2 * (128 + 19) + 20));
    assert_eq!(
        s.rpc("chunk", json!({"source_content": md}), json!({"chunk_size": 40})).await.0,
        200,
        "small chunk_size works without an explicit overlap"
    );
    // Without overlap, chunks join back into the original text.
    let (_, v) = s.rpc("chunk", json!({"source_content": md}), json!({"chunk_size": 100, "chunk_overlap": 0})).await;
    let joined: Vec<&str> = v["data"]["chunks"].as_array().unwrap().iter().map(|c| c["content"].as_str().unwrap()).collect();
    assert_eq!(joined.join("\n\n"), md.trim_end());
}

fn receipt_schema() -> Value {
    json!({
        "type": "object",
        "required": ["merchant", "total"],
        "properties": {
            "merchant": {"type": "string"},
            "total": {"type": "number"},
            "currency": {"enum": ["USD", "EUR"]},
            "items": {"type": "array", "items": {"type": "object", "properties": {"name": {"type": "string"}, "qty": {"type": "integer"}}}}
        }
    })
}

#[tokio::test]
async fn structured_extraction() {
    use std::sync::atomic::Ordering::SeqCst;
    let s = server().await;
    let p = s.write("receipt.md", b"# Corner Cafe\n\nLatte x2  9.00\nTotal: 9.00 USD\n");
    let opts = json!({"extract_schema": receipt_schema(), "title_method": "local", "summary_method": "local"});
    let (st, v) = s.convert_sync(&p, opts.clone()).await;
    assert_eq!(st, 200, "{v}");
    let d = &v["data"];
    assert_eq!(d["extracted"], json!({"merchant": "mock", "total": 42.5, "currency": "USD", "items": [{"name": "mock", "qty": 2}]}));
    assert_eq!(d["feature_status"]["extraction"]["status"], "completed");
    assert!(d["content"].as_str().unwrap().contains("Corner Cafe"), "markdown is still returned");
    assert_eq!(d["llm"]["calls"][0]["purpose"], "structured_extraction");

    // An answer that misses required fields is retried once with feedback.
    s.mock.bad_extractions.store(1, SeqCst);
    let (_, v) = s
        .convert_sync(
            &p,
            json!({"extract_schema": receipt_schema(), "cache": "bypass", "title_method": "local", "summary_method": "local"}),
        )
        .await;
    assert_eq!(v["data"]["extracted"]["total"], 42.5, "{v}");
    assert_eq!(v["data"]["llm"]["totals"]["calls"], 2);

    // Two bad answers: the conversion still succeeds, without `extracted`.
    s.mock.bad_extractions.store(2, SeqCst);
    let (st, v) = s
        .convert_sync(
            &p,
            json!({"extract_schema": receipt_schema(), "cache": "bypass", "title_method": "local", "summary_method": "local"}),
        )
        .await;
    assert_eq!(st, 200, "{v}");
    assert!(v["data"]["extracted"].is_null());
    assert_eq!(v["data"]["feature_status"]["extraction"]["status"], "failed");
    assert!(v["data"]["feature_status"]["extraction"]["error"].as_str().unwrap().contains("$.merchant is required"));

    // Without extract_schema nothing changes.
    let (_, v) = s.convert_sync(&p, json!({"title_method": "local", "summary_method": "local", "cache": "bypass"})).await;
    assert!(v["data"]["extracted"].is_null());
    assert_eq!(v["data"]["feature_status"]["extraction"]["status"], "disabled");

    // Standalone `extract` on text.
    let (st, v) = s
        .rpc(
            "extract",
            json!({"source_content": "Corner Cafe, total 9.00 USD", "requester_id": "expenses"}),
            json!({"extract_schema": receipt_schema()}),
        )
        .await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["data"]["extracted"]["merchant"], "mock");
    assert_eq!(v["data"]["llm"]["totals"]["calls"], 1);
    s.mock.bad_extractions.store(2, SeqCst);
    let (st, v) = s.rpc("extract", json!({"source_content": "Corner Cafe"}), json!({"extract_schema": receipt_schema()})).await;
    assert_eq!(st, 422, "{v}");
    assert_eq!(v["error"]["code"], "extraction_failed");

    // Validation.
    assert_eq!(s.rpc("extract", json!({"source_content": "x"}), json!({})).await.0, 400);
    assert_eq!(s.rpc("extract", json!({"source_content": "x"}), json!({"extract_schema": {"type": "array"}})).await.0, 400);
}

#[tokio::test]
async fn extraction_needs_an_llm() {
    let s = server_with(None, None, |v| {
        v.remove("DOCVISION_LLM_MODEL");
    })
    .await;
    let p = s.write("r.md", b"# R\n\nTotal 1\n");
    let (st, v) = s.convert_sync(&p, json!({"extract_schema": receipt_schema()})).await;
    assert_eq!(st, 400, "{v}");
    assert_eq!(v["error"]["code"], "provider_not_configured");
}
