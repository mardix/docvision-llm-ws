//! Repository contract suite: runs against SQLite always and against PostgreSQL when
//! built with `--features postgres` and `DOCVISION_TEST_POSTGRES_URL` is set.

use docvision_llm_ws::db::{Db, EventRow, HistoryFilter, JobRow, RequestFinish, RequestRow, WebhookRow, WriteOp, now_ms};
use docvision_llm_ws::metrics::Metrics;
use docvision_llm_ws::writer::Writer;
use std::sync::Arc;

fn job(id: &str, state: &str) -> JobRow {
    JobRow {
        job_id: id.into(),
        request_id: format!("req-{id}"),
        state: state.into(),
        priority: "normal".into(),
        created_at: now_ms(),
        started_at: None,
        finished_at: None,
        result_path: None,
        error: None,
        secrets: Some(vec![1, 2, 3, 0, 255]),
        cache_key: None,
        has_webhook: true,
    }
}

async fn contract(url: &str) {
    let db = Db::connect(url).await.unwrap();
    // Migrations are idempotent.
    let db = {
        drop(db);
        Db::connect(url).await.unwrap()
    };
    let (w, _task) = Writer::start(&db, Arc::new(Metrics::default())).await.unwrap();
    let tag = uuid::Uuid::now_v7().to_string();
    let t0 = now_ms();

    // Requests + filters + keyset pagination.
    let mut ops = Vec::new();
    for i in 0..12 {
        ops.push(WriteOp::InsertRequest(RequestRow {
            request_id: format!("{tag}-r{i}"),
            operation: format!("op-{tag}"),
            mode: Some(if i % 2 == 0 { "sync" } else { "async" }.into()),
            request_status: "running".into(),
            created_at: t0 + i,
            ..Default::default()
        }));
    }
    ops.push(WriteOp::FinishRequest(RequestFinish {
        request_id: format!("{tag}-r0"),
        request_status: "completed".into(),
        execution_status: Some("completed".into()),
        finished_at: t0 + 100,
        statistics: Some("{\"total_words\":3}".into()),
        result_available: true,
        ..Default::default()
    }));
    assert!(w.send_durable(ops).await);
    let high = db.max_request_seq().await.unwrap();
    let f = HistoryFilter { operation: Some(format!("op-{tag}")), ..Default::default() };
    let page1 = db.history_list(&f, high, None, 5).await.unwrap();
    assert_eq!(page1.len(), 5);
    use sqlx::Row;
    let last: i64 = page1.last().unwrap().get("seq");
    let page2 = db.history_list(&f, high, Some(last), 100).await.unwrap();
    assert_eq!(page2.len(), 7);
    let fs = HistoryFilter { execution: Some("sync".into()), ..f.clone() };
    assert_eq!(db.history_list(&fs, high, None, 100).await.unwrap().len(), 6);
    let ft = HistoryFilter { created_from: Some(t0 + 10), ..f.clone() };
    assert_eq!(db.history_list(&ft, high, None, 100).await.unwrap().len(), 2);
    let r0 = db.history_get(&format!("{tag}-r0")).await.unwrap().unwrap();
    assert_eq!(r0.get::<String, _>("request_status"), "completed");

    // Jobs, binary secrets, state transitions, recovery.
    let (a, b) = (format!("{tag}-a"), format!("{tag}-b"));
    assert!(w.send_durable(vec![WriteOp::InsertJob(job(&a, "queued")), WriteOp::InsertJob(job(&b, "queued"))]).await);
    assert_eq!(db.get_job(&a).await.unwrap().unwrap().secrets.unwrap(), vec![1, 2, 3, 0, 255]);
    assert!(w.send_durable(vec![WriteOp::JobStarted { job_id: a.clone(), at: now_ms() }]).await);
    assert!(db.jobs_in_state("running").await.unwrap().iter().any(|j| j.job_id == a));
    assert!(w.send_durable(vec![WriteOp::Recover { at: now_ms() }]).await);
    let ja = db.get_job(&a).await.unwrap().unwrap();
    assert_eq!(ja.state, "failed");
    assert!(ja.error.unwrap().contains("interrupted"));
    assert_eq!(db.get_job(&b).await.unwrap().unwrap().state, "queued", "queued jobs are untouched");
    assert!(
        w.send_durable(vec![WriteOp::JobFinished {
            job_id: b.clone(),
            state: "completed".into(),
            at: now_ms(),
            result_path: Some("/x.json".into()),
            error: None
        }])
        .await
    );
    assert_eq!(db.get_job(&b).await.unwrap().unwrap().result_path.as_deref(), Some("/x.json"));

    // Events and calls pagination.
    let rid = format!("{tag}-r1");
    let evs = (0..3)
        .map(|i| EventRow { request_id: rid.clone(), at: now_ms(), stage: format!("s{i}"), event: "completed".into(), detail: None })
        .collect();
    assert!(
        w.send_durable(vec![
            WriteOp::Events(evs),
            WriteOp::Call { call_id: "c1".into(), request_id: rid.clone(), record: "{\"status\":\"ok\"}".into(), at: now_ms() }
        ])
        .await
    );
    let e1 = db.events_page(&rid, 0, 2).await.unwrap();
    assert_eq!(e1.len(), 2);
    assert_eq!(db.events_page(&rid, e1[1].0, 10).await.unwrap().len(), 1);
    assert_eq!(db.calls_page(&rid, 0, 10).await.unwrap().len(), 1);

    // Webhook upsert and cache TTL / upsert.
    let mut wh = WebhookRow {
        event_id: format!("{tag}-e"),
        job_id: b.clone(),
        status: "pending".into(),
        attempts: 1,
        last_http_status: Some(500),
        last_error: None,
        next_attempt_at: None,
        created_at: now_ms(),
        delivered_at: None,
    };
    assert!(w.send_durable(vec![WriteOp::Webhook(wh.clone())]).await);
    wh.status = "delivered".into();
    wh.attempts = 2;
    assert!(w.send_durable(vec![WriteOp::Webhook(wh)]).await);
    let whs = db.webhooks_for_job(&b).await.unwrap();
    assert_eq!(whs.len(), 1);
    assert_eq!(whs[0].attempts, 2);
    let key = format!("{tag}-k");
    assert!(
        w.send_durable(vec![WriteOp::CachePut { key: key.clone(), path: "/p1".into(), body_start: 1, body_end: 2, size: 3, at: now_ms() }])
            .await
    );
    assert!(
        w.send_durable(vec![WriteOp::CachePut {
            key: key.clone(),
            path: "/p2".into(),
            body_start: 4,
            body_end: 9,
            size: 10,
            at: now_ms()
        }])
        .await
    );
    assert_eq!(db.cache_get(&key, 0).await.unwrap(), Some(("/p2".into(), 4, 9)));
    assert_eq!(db.cache_get(&key, now_ms() + 10_000).await.unwrap(), None, "expired by TTL");

    // Retention keeps pending-webhook jobs and is independent per data class.
    assert!(w.send_durable(vec![WriteOp::Retention { result_cutoff: now_ms() + 1000, history_cutoff: 0, limit: 1000 }]).await);
    assert!(db.get_job(&b).await.unwrap().is_none());
    assert_eq!(db.cache_get(&key, 0).await.unwrap(), None);
    assert!(db.history_get(&format!("{tag}-r0")).await.unwrap().is_some());
}

#[tokio::test]
async fn sqlite_contract() {
    let dir = std::env::temp_dir().join(format!("docvision-contract-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    contract(&format!("sqlite://{}?mode=rwc", dir.join("c.sqlite").display())).await;
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn postgres_contract() {
    match docvision_llm_ws::db::postgres::contract_test_url() {
        Some(url) => contract(&url).await,
        None => eprintln!("DOCVISION_TEST_POSTGRES_URL not set; skipping PostgreSQL contract suite"),
    }
}
