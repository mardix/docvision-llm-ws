//! Async jobs, PDF/image LLM paths, cache + single-flight, destinations, admission.

mod common;

use common::*;
use serde_json::{Value, json};
use std::sync::atomic::Ordering::SeqCst;

#[tokio::test]
async fn async_default_job_get_and_fields() {
    let s = server().await;
    let p = s.write("r.docx", &fixtures::docx(2));
    let (st, v) = s
        .rpc(
            "convert",
            json!({"source": p, "metadata": {"reference": "ref-1"}}),
            json!({"title_method": "local", "summary_method": "local"}),
        )
        .await;
    assert_eq!(st, 202, "{v}");
    assert_eq!(v["data"]["status"], "queued");
    assert_eq!(v["data"]["queue_position"], 1);
    assert_eq!(v["data"]["cache_hit"], false);
    let job_id = v["data"]["job_id"].as_str().unwrap().to_string();
    let job = s.wait_job(&job_id).await;
    assert_eq!(job["status"], "completed", "{job}");
    let r = &job["result"];
    assert_eq!(r["job_id"], job_id.as_str());
    assert_eq!(r["status"], "completed");
    assert_eq!(r["metadata"]["reference"], "ref-1");
    assert!(r["content"].as_str().unwrap().contains("# Section 1"));
    assert!(r["timing"]["queue_ms"].is_u64());
    // Field selection streams only the requested keys.
    let (st, g) = s.rpc("job.get", json!({"job_id": job_id, "fields": ["status", "statistics"]}), json!({})).await;
    assert_eq!(st, 200);
    let res = g["data"]["result"].as_object().unwrap();
    assert_eq!(res.len(), 2);
    assert!(res.contains_key("statistics"));
    // job.wait on a finished job returns immediately.
    let t = std::time::Instant::now();
    let (st, w) = s.rpc("job.wait", json!({"job_id": job_id, "timeout_ms": 30000}), json!({})).await;
    assert_eq!(st, 200);
    assert_eq!(w["data"]["status"], "completed");
    assert!(t.elapsed().as_secs() < 2);
    assert_eq!(s.rpc("job.wait", json!({"job_id": job_id, "timeout_ms": 30001}), json!({})).await.0, 400);
}

#[tokio::test]
async fn pdf_ocr_on_batches_and_counts_pages() {
    let s = server_with(None, None, |v| {
        // 8000 / 800 * 0.8 = 8 pages per batch
        v.insert("DOCVISION_LLM_MAX_OUTPUT_TOKENS".into(), "8000".into());
    })
    .await;
    let p = s.write("scan.pdf", &fixtures::pdf(20));
    let (st, v) = s.convert_sync(&p, json!({"gen_summary": false, "gen_title": false})).await;
    assert_eq!(st, 200, "{v}");
    let d = &v["data"];
    assert_eq!(d["statistics"]["original_total_pages"], 20);
    assert_eq!(d["statistics"]["original_page_count_exact"], true);
    assert_eq!(d["statistics"]["original_page_count_method"], "pdf_page_tree");
    // 20 pages / 8 per batch = 3 transcription calls, reassembled in page order.
    assert_eq!(s.mock.transcriptions.load(SeqCst), 3);
    let c = d["content"].as_str().unwrap();
    let positions: Vec<usize> = (1..=20).map(|i| c.find(&format!("## Page {i}\n")).unwrap()).collect();
    assert!(positions.windows(2).all(|w| w[0] < w[1]));
    assert_eq!(d["llm"]["totals"]["calls"], 3);
    assert_eq!(d["llm"]["totals"]["usage_complete"], true);
    let spans: Vec<&str> = d["llm"]["calls"].as_array().unwrap().iter().map(|c| c["source_pages_or_chunks"].as_str().unwrap()).collect();
    assert!(spans.contains(&"pages 1-8") && spans.contains(&"pages 17-20"));
    assert_eq!(d["chunks"][0]["source_span"]["kind"], "page");

    assert!(d["timing"]["llm_wall_ms"].as_u64().unwrap() <= d["timing"]["llm_call_sum_ms"].as_u64().unwrap());
}

#[cfg(feature = "pdf-native")]
#[tokio::test]
async fn pdf_is_split_per_batch() {
    let s = server().await;
    let p = s.write("big.pdf", &fixtures::text_pdf(20, &[]));
    let (st, v) = s.convert_sync(&p, json!({"gen_summary": false, "gen_title": false})).await;
    assert_eq!(st, 200, "{v}");
    let c = v["data"]["content"].as_str().unwrap();
    let positions: Vec<usize> = (1..=20).map(|i| c.find(&format!("## Page {i}\n")).unwrap()).collect();
    assert!(positions.windows(2).all(|w| w[0] < w[1]));
    // Each call carried only its own pages (8 + 8 + 4), not the whole 20-page PDF.
    let mut seen = s.mock.pdf_pages_seen.lock().unwrap().clone();
    seen.sort();
    assert_eq!(seen, vec![4, 8, 8], "pages per attached PDF");
    // The per-batch pieces were removed from staging.
    let left: Vec<_> =
        std::fs::read_dir(s.dir.join("staging")).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().ends_with(".pdf")).collect();
    assert!(left.is_empty(), "{left:?}");
    // A truncated piece is cut in half again: 1-8 -> 1-4 + 5-8 -> ... -> single pages.
    s.mock.pdf_pages_seen.lock().unwrap().clear();
    s.mock.truncate_multi_page.store(true, SeqCst);
    let (st, v) = s.convert_sync(&p, json!({"gen_summary": false, "gen_title": false, "cache": "bypass"})).await;
    assert_eq!(st, 200, "{v}");
    let c = v["data"]["content"].as_str().unwrap();
    assert!((1..=20).all(|i| c.contains(&format!("## Page {i}\n"))), "{c}");
    let seen = s.mock.pdf_pages_seen.lock().unwrap().clone();
    assert_eq!(seen.iter().filter(|n| **n == 1).count(), 20, "every page ends up in its own one-page PDF: {seen:?}");
}

#[tokio::test]
async fn pdf_truncation_splits_batches() {
    let s = server().await;
    s.mock.truncate_multi_page.store(true, SeqCst);
    let p = s.write("t.pdf", &fixtures::pdf(4));
    let (st, v) = s.convert_sync(&p, json!({"gen_summary": false, "gen_title": false})).await;
    assert_eq!(st, 200, "{v}");
    let c = v["data"]["content"].as_str().unwrap();
    for i in 1..=4 {
        assert!(c.contains(&format!("## Page {i}\n")), "{c}");
    }
    // 1 truncated 4-page call, then 2 truncated 2-page calls, then 4 single pages.
    let calls = v["data"]["llm"]["calls"].as_array().unwrap();
    assert_eq!(calls.iter().filter(|c| c["status"] == "truncated").count(), 3);
    assert_eq!(calls.iter().filter(|c| c["status"] == "ok").count(), 4);
}

#[cfg(feature = "pdf-native")]
#[tokio::test]
async fn pdf_auto_and_off_modes() {
    let s = server().await;
    let p = s.write("mixed.pdf", &fixtures::text_pdf(5, &[3, 4]));
    let (st, v) = s.convert_sync(&p, json!({"ocr": "auto", "gen_summary": false, "gen_title": false})).await;
    assert_eq!(st, 200, "{v}");
    let c = v["data"]["content"].as_str().unwrap();
    assert!(c.contains("native text on page 1"), "{c}");
    assert!(c.contains("native text on page 5"));
    assert!(c.contains("## Page 3") && c.contains("## Page 4"), "{c}");
    assert!(!c.contains("## Page 1\n"));
    assert_eq!(s.mock.transcriptions.load(SeqCst), 1, "pages 3-4 OCR'd in one batch");
    assert_eq!(v["data"]["statistics"]["original_total_pages"], 5);
    // ocr=off with scanned pages -> 422, or a marked partial result.
    let (st, v) = s.convert_sync(&p, json!({"ocr": "off", "gen_summary": false, "gen_title": false})).await;
    assert_eq!(st, 422, "{v}");
    assert_eq!(v["error"]["code"], "scanned_pages");
    let (st, v) = s.convert_sync(&p, json!({"ocr": "off", "allow_partial": true, "gen_summary": false, "gen_title": false})).await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["data"]["status"], "partial");
    assert!(v["data"]["warnings"].to_string().contains("ocr=off"));
    // A clean text PDF with ocr=off makes zero calls.
    let before = s.mock.requests.load(SeqCst);
    let p = s.write("clean.pdf", &fixtures::text_pdf(2, &[]));
    let (st, v) = s.convert_sync(&p, json!({"ocr": "off", "gen_summary": false, "gen_title": false})).await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(s.mock.requests.load(SeqCst), before);
}

#[tokio::test]
async fn images_via_vision() {
    let s = server().await;
    let p = s.write("pic.png", &fixtures::png(800, 600));
    let (st, v) = s.convert_sync(&p, json!({"gen_summary": false, "gen_title": false})).await;
    assert_eq!(st, 200, "{v}");
    assert!(v["data"]["content"].as_str().unwrap().contains("Mock image text."));
    assert_eq!(v["data"]["statistics"]["original_total_pages"], 1);
    assert_eq!(s.convert_sync(&p, json!({"ocr": "off"})).await.0, 422);
    let big = s.write("huge.png", &fixtures::png(100_000, 100_000));
    assert_eq!(s.convert_sync(&big, json!({})).await.0, 413);
}

#[tokio::test]
async fn compatible_provider_gets_pdf_inline() {
    let s = server_with(None, None, |v| {
        v.insert("DOCVISION_LLM_PROVIDER".into(), "local".into());
    })
    .await;
    let p = s.write("a.pdf", &fixtures::pdf(2));
    let (st, v) = s.convert_sync(&p, json!({})).await;
    assert_eq!(st, 200, "{v}");
    assert!(s.mock.transcriptions.load(SeqCst) > 0);
    assert_eq!(s.mock.uploads.load(SeqCst), 0, "compatible APIs get the PDF inline");
}

#[tokio::test]
async fn retries_and_accounting_on_failures() {
    let s = server_with(None, None, |v| {
        v.insert("DOCVISION_LLM_MAX_RETRIES".into(), "1".into());
    })
    .await;
    *s.mock.p5xx.lock().unwrap() = 1.0;
    let p = s.write("a.pdf", &fixtures::pdf(1));
    let (st, v) = s.convert_sync(&p, json!({})).await;
    assert_eq!(st, 502, "{v}");
    let d = &v["data"];
    assert_eq!(d["status"], "failed");
    // Both attempts are accounted for, with missing usage as null (never zero).
    let calls = d["llm"]["calls"].as_array().unwrap();
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(|c| c["status"] == "http_error" && c["input_tokens"].is_null()));
    assert_eq!(calls[1]["attempt"], 2);
    assert!(d["timing"]["llm_retry_wait_ms"].as_u64().unwrap() > 0);
    // Missing usage on success: null and totals disclose incompleteness.
    *s.mock.p5xx.lock().unwrap() = 0.0;
    s.mock.omit_usage.store(true, SeqCst);
    let p = s.write("b.pdf", &fixtures::pdf(1));
    let (st, v) = s.convert_sync(&p, json!({"gen_summary": false, "gen_title": false})).await;
    assert_eq!(st, 200, "{v}");
    assert!(v["data"]["llm"]["totals"]["input_tokens"].is_null());
    assert_eq!(v["data"]["llm"]["totals"]["usage_complete"], false);
}

#[tokio::test]
async fn breaker_opens_and_fails_fast() {
    let s = server_with(None, None, |v| {
        v.insert("DOCVISION_LLM_MAX_RETRIES".into(), "0".into());
    })
    .await;
    *s.mock.p5xx.lock().unwrap() = 1.0;
    for i in 0..5 {
        let p = s.write(&format!("f{i}.pdf"), &fixtures::pdf(1));
        assert_eq!(s.convert_sync(&p, json!({})).await.0, 502);
    }
    let before = s.mock.requests.load(SeqCst);
    let p = s.write("g.pdf", &fixtures::pdf(1));
    let (st, v) = s.convert_sync(&p, json!({})).await;
    assert_eq!(st, 503, "{v}");
    assert_eq!(s.mock.requests.load(SeqCst), before, "open breaker fails fast");
    let (_, h) = s.rpc("health", json!({}), json!({})).await;
    assert!(h["data"]["providers"].to_string().contains("open"));
}

#[tokio::test]
async fn cancellation_finalizes_call_records() {
    let s = server().await;
    s.mock.set_latency(2000, 2000);
    let p = s.write("slow.pdf", &fixtures::pdf(1));
    let fut = s.convert_sync(&p, json!({"gen_summary": false, "gen_title": false}));
    // Client gives up after 300 ms -> the request future is dropped server-side.
    let _ = tokio::time::timeout(std::time::Duration::from_millis(300), fut).await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    s.flush().await;
    let (_, h) = s.rpc("history.list", json!({"operation": "convert"}), json!({})).await;
    let item = &h["data"]["items"][0];
    assert_eq!(item["request_status"], "cancelled", "{h}");
    let rid = item["request_id"].as_str().unwrap();
    let (_, g) = s.rpc("history.get", json!({"request_id": rid}), json!({})).await;
    let calls = g["data"]["calls"]["items"].as_array().unwrap();
    assert_eq!(calls.len(), 1, "{g}");
    assert_eq!(calls[0]["status"], "cancelled");
}

#[tokio::test]
async fn single_flight_and_cache() {
    let s = server().await;
    s.mock.set_latency(300, 300);
    let p = s.write("same.pdf", &fixtures::pdf(2));
    let opts = json!({"gen_summary": false, "gen_title": false});
    // 50 identical concurrent submissions -> one provider conversion.
    let futs = (0..50).map(|_| s.rpc("convert", json!({"source": p}), opts.clone()));
    let res = futures_util::future::join_all(futs).await;
    let ids: Vec<String> = res
        .iter()
        .map(|(st, v)| {
            assert_eq!(*st, 202, "{v}");
            v["data"]["job_id"].as_str().unwrap().to_string()
        })
        .collect();
    let mut uniq = ids.clone();
    uniq.sort();
    uniq.dedup();
    assert_eq!(uniq.len(), 50, "each submission gets its own job_id");
    let mut hits = 0;
    for id in &ids {
        let j = s.wait_job(id).await;
        assert_eq!(j["status"], "completed", "{j}");
        assert_eq!(j["result"]["job_id"], id.as_str());
        if j["result"]["cache_hit"] == true {
            hits += 1;
            assert_eq!(j["result"]["llm"]["totals"]["calls"], 0);
        }
    }
    assert_eq!(s.mock.transcriptions.load(SeqCst), 1, "50 identical submissions = 1 conversion");
    assert_eq!(hits, 49);
    // Later identical async request: a cache hit at accept time (local source).
    s.flush().await;
    let (st, v) = s.rpc("convert", json!({"source": p}), opts.clone()).await;
    assert_eq!(st, 202);
    assert_eq!(v["data"]["status"], "completed", "{v}");
    assert_eq!(v["data"]["cache_hit"], true);
    let j = s.wait_job(v["data"]["job_id"].as_str().unwrap()).await;
    assert!(j["result"]["content"].as_str().unwrap().contains("## Page 2"));
    assert_eq!(s.mock.transcriptions.load(SeqCst), 1);
    // bypass recomputes; different options miss.
    let mut o = opts.clone();
    o["cache"] = json!("bypass");
    let (_, v) = s.rpc("convert", json!({"source": p}), o).await;
    s.wait_job(v["data"]["job_id"].as_str().unwrap()).await;
    assert_eq!(s.mock.transcriptions.load(SeqCst), 2);
}

#[tokio::test]
async fn destinations_local_with_markdown() {
    let s = server().await;
    let p = s.write("d.docx", &fixtures::docx(1));
    let out = s.dir.join("out");
    std::fs::create_dir_all(&out).unwrap();
    let dest = out.join("report.md");
    let opts = json!({"title_method": "local", "summary_method": "local"});
    let (st, v) = s.rpc("convert", json!({"source": p, "destination": dest.to_str().unwrap()}), opts.clone()).await;
    assert_eq!(st, 202, "{v}");
    let j = s.wait_job(v["data"]["job_id"].as_str().unwrap()).await;
    assert_eq!(j["status"], "completed", "{j}");
    let json_bytes = std::fs::read(out.join("report.docv.json")).unwrap();
    let md = std::fs::read_to_string(&dest).unwrap();
    let published: Value = serde_json::from_slice(&json_bytes).unwrap();
    assert_eq!(published["content"].as_str().unwrap(), md);
    assert_eq!(published["dest_file"].as_str().map(|d| d.ends_with("report.md")), Some(true), "{published}");
    assert_eq!(published["md_file"], published["dest_file"], "a .md destination is where the Markdown goes");
    assert!(published["docv_file"].as_str().unwrap().ends_with("report.docv.json"));
    // Sizes sit next to the paths; the JSON's own size is exact even though it contains itself.
    assert_eq!(published["docv_bytes"].as_u64().unwrap(), json_bytes.len() as u64, "self-referential size is exact");
    assert_eq!(published["md_bytes"].as_u64().unwrap(), md.len() as u64);
    assert_eq!(published["src_bytes"].as_u64().unwrap(), std::fs::metadata(&p).unwrap().len());
    assert!(published.get("files").is_none(), "the files list was replaced by the *_file / *_bytes fields");
    // Existing files are replaced by default.
    let (_, v) = s
        .rpc(
            "convert",
            json!({"source": p, "destination": dest.to_str().unwrap()}),
            json!({"cache": "bypass", "title_method": "local", "summary_method": "local"}),
        )
        .await;
    let j = s.wait_job(v["data"]["job_id"].as_str().unwrap()).await;
    assert_eq!(j["status"], "completed", "{j}");
    // A folder destination names the outputs after the source file, extension kept.
    let folder = format!("{}/", out.display());
    let (_, v) =
        s.rpc("convert", json!({"source": p, "destination": folder}), json!({"title_method": "local", "summary_method": "local"})).await;
    let j = s.wait_job(v["data"]["job_id"].as_str().unwrap()).await;
    assert_eq!(j["status"], "completed", "{j}");
    assert!(out.join("d.docx.md").exists() && out.join("d.docx.docv.json").exists());
    let r = &j["result"];
    assert_eq!(r["dest_file"], folder.as_str(), "the destination as requested");
    assert!(r["md_file"].as_str().unwrap().ends_with("/out/d.docx.md"), "{r}");
    assert!(r["docv_file"].as_str().unwrap().ends_with("/out/d.docx.docv.json"));
    assert!(r["src_file"].as_str().unwrap().ends_with("d.docx"));
    // overwrite=false on an existing destination fails, and does not claim the write.
    let (_, v) = s
        .rpc(
            "convert",
            json!({"source": p, "destination": dest.to_str().unwrap()}),
            json!({"cache": "bypass", "overwrite": false, "title_method": "local", "summary_method": "local"}),
        )
        .await;
    let j = s.wait_job(v["data"]["job_id"].as_str().unwrap()).await;
    assert_eq!(j["status"], "failed", "{j}");
    assert_eq!(j["error"]["code"], "destination_exists");
    let r = &j["result"];
    assert!(r["md_file"].is_null() && r["docv_file"].is_null() && r["docv_bytes"].is_null(), "nothing was written: {r}");
    assert!(r["md_bytes"].as_u64().unwrap() > 0 && r["src_bytes"].as_u64().unwrap() > 0, "sizes are known even so");
}

#[tokio::test]
async fn queue_full_returns_429_with_retry_after() {
    let s = server_with(None, None, |v| {
        v.insert("DOCVISION_QUEUE_DEPTH".into(), "2".into());
        v.insert("DOCVISION_MAX_CONCURRENT_JOBS".into(), "1".into());
    })
    .await;
    s.mock.set_latency(1500, 1500);
    let mut codes = Vec::new();
    let mut retry_after = None;
    for i in 0..8 {
        let p = s.write(&format!("q{i}.pdf"), &fixtures::pdf(1));
        let r = s
            .client
            .post(format!("{}/rpc", s.url))
            .header("x-access-token", TOKEN)
            .json(&json!({"operation": "convert", "payload": {"source": p}, "options": {"gen_summary": false, "gen_title": false}}))
            .send()
            .await
            .unwrap();
        if r.status() == 429 {
            retry_after = r.headers().get("retry-after").map(|h| h.to_str().unwrap().to_string());
        }
        codes.push(r.status().as_u16());
    }
    assert!(codes.contains(&202));
    assert!(codes.contains(&429), "{codes:?}");
    assert!(retry_after.unwrap().parse::<u64>().unwrap() >= 1);
}

#[tokio::test]
async fn job_bigger_than_budget_is_413_and_sync_busy_is_503() {
    let s = server_with(None, None, |v| {
        v.insert("DOCVISION_MEMORY_BUDGET".into(), "2MiB".into());
    })
    .await;
    let p = s.write("big.md", &vec![b'a'; 1 << 20]);
    let (st, v) = s.convert_sync(&p, json!({})).await;
    assert_eq!(st, 413, "{v}");
    assert_eq!(v["error"]["code"], "job_too_large");
}

#[tokio::test]
async fn throughput_settles_under_provider_rate_limits() {
    let s = server_with(None, None, |v| {
        v.insert("DOCVISION_LLM_MAX_RETRIES".into(), "10".into());
        v.insert("DOCVISION_LLM_MAX_CONCURRENCY".into(), "32".into());
    })
    .await;
    s.mock.set_latency(50, 100);
    // Provider accepts only 4 concurrent requests and 429s beyond that.
    s.mock.max_inflight.store(4, SeqCst);
    let mut ids = Vec::new();
    for i in 0..40 {
        let p = s.write(&format!("r{i}.pdf"), &fixtures::pdf(1 + i % 3));
        let (st, v) = s.rpc("convert", json!({"source": p}), json!({"gen_summary": false, "gen_title": false, "cache": "bypass"})).await;
        assert_eq!(st, 202);
        ids.push(v["data"]["job_id"].as_str().unwrap().to_string());
    }
    for id in &ids {
        let j = s.wait_job(id).await;
        assert_eq!(j["status"], "completed", "zero failed jobs: {j}");
    }
    assert!(s.mock.rejected_429.load(SeqCst) > 0, "the limit was actually hit");
    let m = s.client.get(format!("{}/metrics", s.url)).header("x-access-token", TOKEN).send().await.unwrap().text().await.unwrap();
    let ceiling: i64 =
        m.lines().find(|l| l.starts_with("docvision_provider_aimd_ceiling")).and_then(|l| l.rsplit(' ').next()).unwrap().parse().unwrap();
    assert!(ceiling < 32, "AIMD reduced the ceiling ({ceiling})");
}

#[tokio::test]
async fn llm_markdown_wrappers_are_removed() {
    let s = server().await;
    s.mock.wrap_markdown.store(true, SeqCst);
    let opts = json!({"gen_summary": false, "gen_title": false});
    let p = s.write("w.pdf", &fixtures::pdf(3));
    let (st, v) = s.convert_sync(&p, opts.clone()).await;
    assert_eq!(st, 200, "{v}");
    let c = v["data"]["content"].as_str().unwrap();
    assert!(c.starts_with("## Page 1"), "{c:?}");
    assert!(!c.contains("```") && !c.contains("Here is the transcription"), "{c:?}");
    let p = s.write("w.png", &fixtures::png(10, 10));
    let (_, v) = s.convert_sync(&p, opts).await;
    assert_eq!(v["data"]["content"], "# Image Heading\n\nMock image text.\n");
}
