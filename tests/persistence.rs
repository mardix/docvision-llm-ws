//! DB writer batching/durability, history pagination, restart recovery, retention,
//! webhooks and secret redaction.

mod common;

use common::*;
use docvision_llm_ws::db::{RequestRow, WriteOp, now_ms};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::Ordering::SeqCst;

#[tokio::test]
async fn writer_batches_and_acks_durably() {
    let s = server().await;
    let before = s.app.metrics.writer_batches.load(SeqCst);
    for i in 0..5000 {
        let ok = s.app.writer.send(WriteOp::InsertRequest(RequestRow {
            request_id: format!("bulk-{i}"),
            operation: "bulk".into(),
            request_status: "completed".into(),
            created_at: now_ms(),
            ..Default::default()
        }));
        assert!(ok);
    }
    assert!(s.app.writer.send_durable(vec![]).await);
    let batches = s.app.metrics.writer_batches.load(SeqCst) - before;
    assert!(batches <= 100, "5000 ops committed in {batches} batches (group commit)");
    let (_, v) = s.rpc("history.list", json!({"operation": "bulk", "limit": 1}), json!({})).await;
    assert_eq!(v["data"]["items"][0]["request_id"], "bulk-4999");
    // An async accept is durable before 202: the job row is readable immediately.
    let p = s.write("a.md", b"# A\n");
    let (st, v) = s.rpc("convert", json!({"source": p}), json!({})).await;
    assert_eq!(st, 202);
    let job = s.app.db.get_job(v["data"]["job_id"].as_str().unwrap()).await.unwrap();
    assert!(job.is_some());
}

#[tokio::test]
async fn history_pagination_is_stable_under_inserts() {
    let s = server().await;
    for i in 0..30 {
        let (st, _) = s.rpc("chunk", json!({"source_content": format!("# Doc {i}\n")}), json!({})).await;
        assert_eq!(st, 200);
    }
    s.flush().await;
    let mut seen = Vec::new();
    let (_, v) = s.rpc("history.list", json!({"operation": "chunk", "limit": 10}), json!({})).await;
    let mut cursor = v["data"]["next_cursor"].as_str().unwrap().to_string();
    seen.extend(v["data"]["items"].as_array().unwrap().iter().map(|i| i["request_id"].as_str().unwrap().to_string()));
    // Inserts during pagination never shift pages.
    for i in 0..5 {
        s.rpc("chunk", json!({"source_content": format!("# Late {i}\n")}), json!({})).await;
    }
    s.flush().await;
    loop {
        let (st, v) = s.rpc("history.list", json!({"operation": "chunk", "limit": 10, "cursor": cursor}), json!({})).await;
        assert_eq!(st, 200, "{v}");
        seen.extend(v["data"]["items"].as_array().unwrap().iter().map(|i| i["request_id"].as_str().unwrap().to_string()));
        if v["data"]["has_more"] == false {
            assert!(v["data"]["next_cursor"].is_null());
            break;
        }
        cursor = v["data"]["next_cursor"].as_str().unwrap().to_string();
    }
    let mut uniq = seen.clone();
    uniq.sort();
    uniq.dedup();
    assert_eq!(seen.len(), 30);
    assert_eq!(uniq.len(), 30, "no duplicates");
    // Changed filters or a tampered cursor -> 400.
    let (st, _) = s.rpc("history.list", json!({"operation": "convert", "cursor": cursor}), json!({})).await;
    assert_eq!(st, 400);
    let mut bad = cursor.clone();
    bad.push('x');
    assert_eq!(s.rpc("history.list", json!({"operation": "chunk", "cursor": bad}), json!({})).await.0, 400);
    // Time filters.
    let (_, v) =
        s.rpc("history.list", json!({"created_from": "2000-01-01T00:00:00Z", "created_to": "2000-01-02T00:00:00Z"}), json!({})).await;
    assert_eq!(v["data"]["items"], json!([]));
    assert_eq!(s.rpc("history.list", json!({"created_from": "yesterday"}), json!({})).await.0, 400);
    assert_eq!(s.rpc("history.list", json!({"limit": 101}), json!({})).await.0, 400);
}

#[tokio::test]
async fn history_get_has_events_and_calls() {
    let s = server().await;
    let p = s.write("x.docx", &fixtures::docx(1));
    let (st, v) = s.convert_sync(&p, json!({})).await;
    assert_eq!(st, 200);
    let rid = v["request_id"].as_str().unwrap();
    s.flush().await;
    let (st, g) = s.rpc("history.get", json!({"request_id": rid}), json!({})).await;
    assert_eq!(st, 200, "{g}");
    let req = &g["data"]["request"];
    assert_eq!(req["execution"], "sync");
    assert_eq!(req["request_status"], "completed");
    assert!(req["statistics"]["total_words"].as_u64().unwrap() > 0);
    assert_eq!(req["usage"]["calls"], 1);
    let events: Vec<&str> = g["data"]["events"]["items"].as_array().unwrap().iter().map(|e| e["stage"].as_str().unwrap()).collect();
    assert!(events.contains(&"fetch") && events.contains(&"extraction") && events.contains(&"done"), "{events:?}");
    assert_eq!(g["data"]["calls"]["items"][0]["purpose"], "title_summary_language");
    // Detail pagination.
    let (_, g) = s.rpc("history.get", json!({"request_id": rid, "limit": 1}), json!({})).await;
    let c = g["data"]["events"]["next_cursor"].as_str().unwrap();
    let (st, g2) = s.rpc("history.get", json!({"request_id": rid, "limit": 1, "events_cursor": c}), json!({})).await;
    assert_eq!(st, 200);
    assert_ne!(g["data"]["events"]["items"][0], g2["data"]["events"]["items"][0]);
}

#[test]
fn restart_recovery_after_kill() {
    // Mock provider lives in its own runtime so it survives the "crash".
    let mrt = tokio::runtime::Runtime::new().unwrap();
    let mock = Arc::new(mock::MockState::default());
    mock.set_latency(1500, 1500);
    let mock_url = mrt.block_on(mock::spawn(mock.clone()));
    let dir = temp_dir("restart");
    let tweak = |v: &mut docvision_llm_ws::config::Vars| {
        v.insert("DOCVISION_MAX_CONCURRENT_JOBS".into(), "1".into());
    };

    let rt1 = tokio::runtime::Runtime::new().unwrap();
    let ids: Vec<String> = rt1.block_on(async {
        let s = server_with(Some(dir.clone()), Some((mock.clone(), mock_url.clone())), tweak).await;
        let mut ids = Vec::new();
        for i in 0..3 {
            let p = s.write(&format!("r{i}.pdf"), &fixtures::pdf(1));
            let (st, v) =
                s.rpc("convert", json!({"source": p}), json!({"cache": "bypass", "gen_summary": false, "gen_title": false})).await;
            assert_eq!(st, 202);
            ids.push(v["data"]["job_id"].as_str().unwrap().to_string());
        }
        // Wait until the first job is running (its provider call is in flight).
        loop {
            let (_, w) = s.rpc("job.wait", json!({"job_id": ids[0], "timeout_ms": 1000}), json!({})).await;
            if w["data"]["status"] == "running" {
                break;
            }
        }
        while mock.requests.load(SeqCst) == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        s.flush().await;
        ids
    });
    // kill -9: drop every task without any shutdown logic.
    rt1.shutdown_background();
    let calls_before = mock.transcriptions.load(SeqCst);
    assert_eq!(calls_before, 1);

    let rt2 = tokio::runtime::Runtime::new().unwrap();
    rt2.block_on(async {
        mock.set_latency(10, 10);
        let s = server_with(Some(dir.clone()), Some((mock.clone(), mock_url.clone())), tweak).await;
        let (_, g) = s.rpc("job.get", json!({"job_id": ids[0]}), json!({})).await;
        assert_eq!(g["data"]["status"], "failed", "{g}");
        assert_eq!(g["data"]["error"]["code"], "interrupted");
        for id in &ids[1..] {
            let j = s.wait_job(id).await;
            assert_eq!(j["status"], "completed", "queued job survives the crash: {j}");
        }
        // The interrupted job was not re-run: exactly two more transcriptions.
        assert_eq!(mock.transcriptions.load(SeqCst), calls_before + 2);
    });
}

#[tokio::test]
async fn retention_is_independent_for_results_and_history() {
    let s = server().await;
    let p = s.write("a.md", b"# A\n\ntext\n");
    let (_, v) = s.rpc("convert", json!({"source": p}), json!({"summary_method": "local", "title_method": "local"})).await;
    let job_id = v["data"]["job_id"].as_str().unwrap().to_string();
    s.wait_job(&job_id).await;
    s.flush().await;
    let rid = v["request_id"].as_str().unwrap().to_string();
    // Expire results only.
    let future = now_ms() + 1000;
    assert!(s.app.writer.send_durable(vec![WriteOp::Retention { result_cutoff: future, history_cutoff: 0, limit: 1000 }]).await);
    s.app.jobs.clear_terminal_cache(); // as the retention task does after each batch
    assert_eq!(s.rpc("job.get", json!({"job_id": job_id}), json!({})).await.0, 404);
    let (st, g) = s.rpc("history.get", json!({"request_id": rid}), json!({})).await;
    assert_eq!(st, 200, "history survives result retention");
    assert_eq!(g["data"]["request"]["result_available"], false);
    // Expire history.
    assert!(s.app.writer.send_durable(vec![WriteOp::Retention { result_cutoff: 0, history_cutoff: future, limit: 1000 }]).await);
    assert_eq!(s.rpc("history.get", json!({"request_id": rid}), json!({})).await.0, 404);
    // The input file is never touched.
    assert!(std::path::Path::new(&p).exists());
}

struct Hook {
    hits: std::sync::Mutex<Vec<(axum::http::HeaderMap, Vec<u8>)>>,
    fail_first: std::sync::atomic::AtomicU64,
}

async fn hook_server(h: Arc<Hook>) -> String {
    use axum::routing::post;
    let app = axum::Router::new().route(
        "/hook",
        post(move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
            let h = h.clone();
            async move {
                h.hits.lock().unwrap().push((headers, body.to_vec()));
                if h.fail_first.load(SeqCst) > 0 {
                    h.fail_first.fetch_sub(1, SeqCst);
                    return axum::http::StatusCode::INTERNAL_SERVER_ERROR;
                }
                axum::http::StatusCode::OK
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{a}/hook")
}

#[cfg(feature = "webhooks")]
#[tokio::test]
async fn webhooks_sign_retry_and_dedupe() {
    let s = server_cfg(|c| c.webhook_retry_base = std::time::Duration::from_millis(20)).await;
    let hook = Arc::new(Hook { hits: Default::default(), fail_first: 1.into() });
    let url = hook_server(hook.clone()).await;
    let p = s.write("w.docx", &fixtures::docx(1));
    let (st, v) = s
        .rpc(
            "convert",
            json!({"source": p, "metadata": {"reference": "abc"}, "webhook": {"url": url, "headers": {"Authorization": "Bearer cb-token"}, "secret": "hmac-secret"}}),
            json!({"summary_method": "local", "title_method": "local"}),
        )
        .await;
    assert_eq!(st, 202, "{v}");
    let job_id = v["data"]["job_id"].as_str().unwrap().to_string();
    let job = s.wait_job(&job_id).await;
    for _ in 0..100 {
        if hook.hits.lock().unwrap().len() >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let hits = hook.hits.lock().unwrap().clone();
    assert_eq!(hits.len(), 2, "first attempt 500, retried once");
    let ids: Vec<&str> = hits.iter().map(|(h, _)| h["x-docvision-event-id"].to_str().unwrap()).collect();
    assert_eq!(ids[0], ids[1], "stable event id for deduplication");
    let (h, body) = &hits[1];
    assert_eq!(h["authorization"], "Bearer cb-token");
    let ts = h["x-docvision-timestamp"].to_str().unwrap();
    assert_eq!(h["x-docvision-signature"].to_str().unwrap(), docvision_llm_ws::webhook::sign("hmac-secret", ts, body));
    let payload: serde_json::Value = serde_json::from_slice(body).unwrap();
    assert_eq!(payload["job_id"], job_id.as_str());
    assert_eq!(payload["metadata"]["reference"], "abc");
    // Polling agrees with the callback.
    assert_eq!(payload["status"], job["status"]);
    assert_eq!(payload["statistics"], job["result"]["statistics"]);
    assert!(payload.get("content").is_none(), "reference payloads carry no content");
    // The event says where things are and how big, so the receiver can decide what to fetch.
    assert_eq!(payload["src_file"], job["result"]["src_file"]);
    assert!(payload["src_bytes"].as_u64().unwrap() > 0 && payload["md_bytes"].as_u64().unwrap() > 0, "{payload}");
    assert!(payload["md_file"].is_null() && payload["docv_file"].is_null(), "no destination was requested");
    s.flush().await;
    let (_, g) = s.rpc("job.get", json!({"job_id": job_id, "fields": ["status"]}), json!({})).await;
    let wh = &g["data"]["webhooks"][0];
    assert_eq!(wh["status"], "delivered", "{g}");
    assert_eq!(wh["attempts"], 2);
}

#[tokio::test]
async fn secrets_never_reach_storage_or_responses() {
    let s = server().await;
    let secret_key = "sk-request-secret-123";
    let p = s.write("s.docx", &fixtures::docx(1));
    let (st, v) = s.rpc("convert", json!({"source": p}), json!({"execution": "sync", "llm_api_key": secret_key})).await;
    assert_eq!(st, 200);
    assert!(!v.to_string().contains(secret_key));
    // Credential-bearing S3 DSN and a signed HTTPS URL (both fail to fetch here).
    s.rpc("convert", json!({"source": "s3://AKIAEXAMPLE:supersecretkey@bucket/k.pdf?region=eu-west-1"}), json!({"execution": "sync"}))
        .await;
    s.rpc("convert", json!({"source": "https://127.0.0.1:1/doc.pdf?X-Amz-Signature=signedsecret"}), json!({"execution": "sync"})).await;
    let hook = Arc::new(Hook { hits: Default::default(), fail_first: 0.into() });
    let url = hook_server(hook.clone()).await;
    let payload = if cfg!(feature = "webhooks") {
        json!({"source": p, "webhook": {"url": url, "headers": {"Authorization": "Bearer cbsecret"}}})
    } else {
        json!({"source": p})
    };
    let (_, v) = s.rpc("convert", payload, json!({"llm_api_key": secret_key, "summary_method": "local", "title_method": "local"})).await;
    s.wait_job(v["data"]["job_id"].as_str().unwrap()).await;
    s.flush().await;
    let (_, h) = s.rpc("history.list", json!({"limit": 100}), json!({})).await;
    let txt = h.to_string();
    for secret in [secret_key, "supersecretkey", "signedsecret", "cbsecret", "sk-mock-secret", TOKEN] {
        assert!(!txt.contains(secret), "history leaks {secret}");
    }
    if cfg!(feature = "s3") {
        assert!(txt.contains("s3://bucket/k.pdf?region=eu-west-1"));
    }
    // Raw DB bytes (main file + WAL) contain no plaintext secrets either.
    let mut raw = Vec::new();
    for f in ["docvision.sqlite", "docvision.sqlite-wal"] {
        if let Ok(b) = std::fs::read(s.dir.join(f)) {
            raw.extend(b);
        }
    }
    let raw = String::from_utf8_lossy(&raw);
    for secret in [secret_key, "supersecretkey", "signedsecret", "cbsecret", TOKEN] {
        assert!(!raw.contains(secret), "database leaks {secret}");
    }
}

#[cfg(feature = "webhooks")]
#[tokio::test]
async fn full_webhook_streams_result_with_valid_signature() {
    let s = server().await;
    let hook = Arc::new(Hook { hits: Default::default(), fail_first: 0.into() });
    let url = hook_server(hook.clone()).await;
    let p = s.write("f.docx", &fixtures::docx(2));
    let (_, v) = s
        .rpc(
            "convert",
            json!({"source": p, "webhook": {"url": url, "secret": "s3cr3t", "body": "$"}}),
            json!({"summary_method": "local", "title_method": "local"}),
        )
        .await;
    let job_id = v["data"]["job_id"].as_str().unwrap().to_string();
    s.wait_job(&job_id).await;
    for _ in 0..100 {
        if !hook.hits.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let (h, body) = hook.hits.lock().unwrap()[0].clone();
    let full: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(full["job_id"], job_id.as_str());
    assert!(full["content"].as_str().unwrap().contains("# Section 1"));
    let ts = h["x-docvision-timestamp"].to_str().unwrap();
    assert_eq!(h["x-docvision-signature"].to_str().unwrap(), docvision_llm_ws::webhook::sign("s3cr3t", ts, &body));
}

#[cfg(feature = "webhooks")]
#[tokio::test]
async fn templated_webhook_body() {
    let s = server().await;
    let hook = Arc::new(Hook { hits: Default::default(), fail_first: 0.into() });
    let url = hook_server(hook.clone()).await;
    let p = s.write("f.docx", &fixtures::docx(2));
    let body = json!({"doc": {"md": "$.content", "title": "$.title"}, "first_chunk": "$.chunks[0].content", "event": "$.event", "kind": "converted"});
    let (_, v) = s
        .rpc(
            "convert",
            json!({"source": p, "webhook": {"url": url, "body": body}}),
            json!({"summary_method": "local", "title_method": "local"}),
        )
        .await;
    s.wait_job(v["data"]["job_id"].as_str().unwrap()).await;
    for _ in 0..100 {
        if !hook.hits.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let (_, body) = hook.hits.lock().unwrap()[0].clone();
    let got: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(got["doc"]["md"].as_str().unwrap().contains("# Section 1"), "{got}");
    assert_eq!(got["doc"]["title"], "Docx Meta Title");
    assert!(got["first_chunk"].is_string());
    assert_eq!(got["event"], "job.finished");
    assert_eq!(got["kind"], "converted");
    assert_eq!(got.as_object().unwrap().len(), 4, "only the requested fields are sent");
}

#[tokio::test]
async fn stats_and_dashboard() {
    let s = server().await;
    let p = s.write("d.docx", &fixtures::docx(1));
    let (st, _) = s.convert_sync(&p, json!({"summary_method": "local", "title_method": "local"})).await;
    assert_eq!(st, 200);
    let (st, _) = s.convert_sync("/missing/file.md", json!({})).await;
    assert_eq!(st, 404);
    s.rpc("nope", json!({}), json!({})).await;
    s.flush().await;
    let (st, v) = s.rpc("stats", json!({"window": "1h"}), json!({})).await;
    assert_eq!(st, 200, "{v}");
    let d = &v["data"];
    assert_eq!(d["requests"]["by_status"]["completed"], 1, "{d}");
    assert_eq!(d["requests"]["by_status"]["failed"], 1);
    assert_eq!(d["requests"]["by_status"]["rejected"], 1);
    assert_eq!(d["requests"]["by_format"]["docx"], 1);
    assert!(d["documents"]["words"].as_u64().unwrap() > 0);
    assert!(d["duration_ms"]["p95"].is_u64());
    assert_eq!(d["recent_failures"][0]["code"], "source_not_found");
    assert_eq!(d["live"]["default_llm"], "openai/mock-model");
    assert_eq!(d["live"]["running"], 0, "idle service reports no running jobs");
    assert_eq!(s.rpc("stats", json!({"window": "2y"}), json!({})).await.0, 400);
    // The page is public HTML with a strict CSP; its data needs the token.
    let r = s.client.get(format!("{}/_/dashboard", s.url)).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.headers()["content-security-policy"].to_str().unwrap().contains("connect-src 'self'"));
    let html = r.text().await.unwrap();
    assert!(html.contains("DOCVISION-LLM dashboard") && !html.contains(TOKEN));
    let r = s.client.post(format!("{}/rpc", s.url)).json(&json!({"operation": "stats"})).send().await.unwrap();
    assert_eq!(r.status(), 401);
}

#[cfg(feature = "webhooks")]
#[tokio::test]
async fn multiple_webhooks_and_fire_and_forget() {
    use serde_json::Value;
    let s = server_cfg(|c| c.webhook_retry_base = std::time::Duration::from_millis(20)).await;
    let ok = Arc::new(Hook { hits: Default::default(), fail_first: 0.into() });
    let down = Arc::new(Hook { hits: Default::default(), fail_first: 100.into() });
    let (ok_url, down_url) = (hook_server(ok.clone()).await, hook_server(down.clone()).await);
    let p = s.write("m.md", b"# Multi\n\nBody.\n");
    let hooks = json!([
        {"url": ok_url, "body": {"title": "$.title"}},
        {"url": down_url, "fire_and_forget": true}
    ]);
    let (st, v) =
        s.rpc("convert", json!({"source": p, "webhook": hooks}), json!({"summary_method": "local", "title_method": "local"})).await;
    assert_eq!(st, 202, "{v}");
    let job_id = v["data"]["job_id"].as_str().unwrap().to_string();
    s.wait_job(&job_id).await;
    let mut g = Value::Null;
    for _ in 0..200 {
        s.flush().await;
        g = s.rpc("job.get", json!({"job_id": job_id, "fields": ["status"]}), json!({})).await.1;
        let w = g["data"]["webhooks"].as_array().cloned().unwrap_or_default();
        if w.len() == 2 && w.iter().all(|w| w["status"] != "pending") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let w = g["data"]["webhooks"].as_array().unwrap();
    assert_eq!(w[0]["status"], "delivered", "{g}");
    assert_eq!(w[1]["status"], "sent", "fire-and-forget ignores the 500: {g}");
    assert_eq!(w[1]["attempts"], 1);
    assert_ne!(w[0]["event_id"], w[1]["event_id"]);
    assert_eq!(down.hits.lock().unwrap().len(), 1, "no retries");
    let (h, body) = ok.hits.lock().unwrap()[0].clone();
    assert_eq!(serde_json::from_slice::<Value>(&body).unwrap(), json!({"title": "Multi"}));
    assert_eq!(h["x-docvision-event-id"].to_str().unwrap(), w[0]["event_id"].as_str().unwrap());
    // Too many webhooks are rejected.
    let many: Vec<Value> = (0..11).map(|_| json!({"url": "https://example.com/h"})).collect();
    assert_eq!(s.rpc("convert", json!({"source": p, "webhook": many}), json!({})).await.0, 400);
}

#[tokio::test]
async fn requester_model_in_progress_and_doc_page() {
    use serde_json::Value;
    let s = server().await;
    // A slow LLM keeps the async PDF job in progress for a while.
    s.mock.set_latency(800, 800);
    let pdf = s.write("slow.pdf", &fixtures::pdf(1));
    let (st, v) = s.rpc("convert", json!({"source": pdf, "requester_id": "team-a"}), json!({})).await;
    assert_eq!(st, 202, "{v}");
    let job_id = v["data"]["job_id"].as_str().unwrap().to_string();
    s.flush().await;
    let (_, p) = s.rpc("history.list", json!({"in_progress": true}), json!({})).await;
    let items = p["data"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{p}");
    assert_eq!(items[0]["requester_id"], "team-a");
    assert!(["queued", "running"].contains(&items[0]["execution_status"].as_str().unwrap()));
    s.wait_job(&job_id).await;

    let md = s.write("n.md", b"# Notes\n\nSome text here.\n");
    assert_eq!(s.convert_sync(&md, json!({"title_method": "local", "summary_method": "local"})).await.0, 200);
    assert_eq!(s.rpc("chunk", json!({"source_content": "# A\n\nB.\n", "requester_id": "team-b"}), json!({})).await.0, 200);
    assert_eq!(s.rpc("chunk", json!({"source_content": "x", "requester_id": "bad\nid"}), json!({})).await.0, 400);
    s.flush().await;

    let (_, a) = s.rpc("history.list", json!({"requester_id": "team-a"}), json!({})).await;
    let items = a["data"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{a}");
    assert_eq!(items[0]["llm_provider"], "openai");
    assert_eq!(items[0]["llm_model"], "mock-model");
    let (_, g) = s.rpc("history.get", json!({"request_id": items[0]["request_id"]}), json!({})).await;
    assert_eq!(g["data"]["request"]["llm_model"], "mock-model");
    assert_eq!(g["data"]["request"]["requester_id"], "team-a");
    let (_, u) = s.rpc("history.list", json!({"requester_id": "unknown", "operation": "convert"}), json!({})).await;
    assert_eq!(u["data"]["items"][0]["llm_model"], Value::Null, "local methods make no LLM call");
    assert_eq!(s.rpc("history.list", json!({"in_progress": true}), json!({})).await.1["data"]["items"], json!([]));
    // Format is recorded and filterable.
    let (_, f) = s.rpc("history.list", json!({"format": "pdf"}), json!({})).await;
    let items = f["data"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{f}");
    assert_eq!(items[0]["format"], "pdf");
    assert_eq!(items[0]["requester_id"], "team-a");
    let (_, f) = s.rpc("history.list", json!({"format": "markdown"}), json!({})).await;
    assert!(f["data"]["items"].as_array().unwrap().iter().all(|i| i["format"] == "markdown"));

    let (_, st) = s.rpc("stats", json!({"window": "1h"}), json!({})).await;
    let by_req = st["data"]["requests"]["by_requester"].as_array().unwrap();
    assert!(by_req.iter().any(|r| r["name"] == "team-a" && r["count"] == 1), "{by_req:?}");
    assert!(st["data"]["requests"]["by_model"].as_array().unwrap().iter().any(|r| r["name"] == "openai/mock-model"));

    // The docs page is public.
    let r = s.client.get(format!("{}/_/doc", s.url)).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let html = r.text().await.unwrap();
    assert!(html.contains("id=\"webhooks\"") && html.contains("<table>") && html.contains("/_/dashboard"));
}
