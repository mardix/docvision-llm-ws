//! `history.list` / `history.get`: newest-first keyset pagination with signed opaque cursors.
//! The first page captures a high-water sequence so later inserts never shift pages.

use crate::App;
use crate::db::{HistoryFilter, rfc3339};
use crate::rpc::{AppError, ReqCtx, typed};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sqlx::Row;
use std::sync::Arc;

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct Cursor {
    h: i64,
    l: i64,
    f: String,
}

fn mac(app: &App, payload: &str) -> String {
    blake3::keyed_hash(&app.history_key, payload.as_bytes()).to_hex()[..32].to_string()
}

fn sign(app: &App, c: &Cursor) -> String {
    let p = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(c).unwrap_or_default());
    format!("{p}.{}", mac(app, &p))
}

fn verify(app: &App, s: &str, expect_filter: &str) -> Result<Cursor, AppError> {
    let bad = || AppError::bad_request("invalid_cursor", "malformed or tampered cursor");
    let (p, m) = s.split_once('.').ok_or_else(bad)?;
    if !bool::from(subtle::ConstantTimeEq::ct_eq(mac(app, p).as_bytes(), m.as_bytes())) {
        return Err(bad());
    }
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(p).map_err(|_| bad())?;
    let c: Cursor = serde_json::from_slice(&raw).map_err(|_| bad())?;
    if c.f != expect_filter {
        return Err(AppError::bad_request("invalid_cursor", "cursor was created with different filters"));
    }
    Ok(c)
}

fn parse_time(s: &Option<String>, name: &str) -> Result<Option<i64>, AppError> {
    s.as_deref()
        .map(|v| {
            time::OffsetDateTime::parse(v, &time::format_description::well_known::Rfc3339)
                .map(|t| (t.unix_timestamp_nanos() / 1_000_000) as i64)
                .map_err(|_| AppError::bad_request("invalid_payload", format!("{name} must be an RFC 3339 UTC timestamp")))
        })
        .transpose()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListP {
    #[serde(default)]
    limit: Option<i64>,
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    operation: Option<String>,
    #[serde(default)]
    execution: Option<String>,
    #[serde(default)]
    execution_status: Option<String>,
    #[serde(default)]
    requester_id: Option<String>,
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    in_progress: bool,
    #[serde(default)]
    created_from: Option<String>,
    #[serde(default)]
    created_to: Option<String>,
}

fn json_col(r: &sqlx::any::AnyRow, c: &str) -> Value {
    r.try_get::<Option<String>, _>(c).ok().flatten().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null)
}

fn item(r: &sqlx::any::AnyRow) -> Value {
    let s = |c: &str| r.try_get::<Option<String>, _>(c).ok().flatten();
    let i = |c: &str| r.try_get::<Option<i64>, _>(c).ok().flatten();
    let usage = json_col(r, "usage");
    // File paths and sizes are stored with the statistics; they are shown as their own fields.
    let mut statistics = json_col(r, "statistics");
    let files = statistics.as_object_mut().and_then(|o| o.remove("files")).unwrap_or(Value::Null);
    serde_json::json!({
        "request_id": s("request_id"),
        "requester_id": s("requester_id"),
        "format": s("format"),
        "operation": s("operation"),
        "execution": s("mode"),
        "source": s("source"),
        "src_bytes": files.get("src_bytes"),
        "dest_file": files.get("dest_file"),
        "md_file": files.get("md_file"),
        "md_bytes": files.get("md_bytes"),
        "docv_file": files.get("docv_file"),
        "docv_bytes": files.get("docv_bytes"),
        "request_status": s("request_status"),
        "execution_status": s("execution_status"),
        "execution_stage": s("execution_stage"),
        "job_id": s("job_id"),
        "http_status": i("http_status"),
        "created_at": i("created_at").map(rfc3339),
        "finished_at": i("finished_at").map(rfc3339),
        "timings": json_col(r, "timings"),
        "statistics": statistics,
        "llm_provider": usage.get("provider"),
        "llm_model": usage.get("model"),
        "usage": usage,
        "warnings": json_col(r, "warnings"),
        "error": json_col(r, "error"),
        "result_available": i("result_available").unwrap_or(0) != 0,
    })
}

/// The `n` largest counts, largest first.
fn top(m: std::collections::BTreeMap<String, u64>, n: usize) -> Vec<Value> {
    let mut v: Vec<(String, u64)> = m.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v.into_iter().take(n).map(|(k, c)| serde_json::json!({"name": k, "count": c})).collect()
}

fn db_err(_: sqlx::Error) -> AppError {
    AppError::new(503, "db_unavailable", "database unavailable").retry_after(5)
}

pub async fn list(app: &Arc<App>, ctx: &ReqCtx, payload: Value) -> Result<axum::response::Response, AppError> {
    let p: ListP = typed(payload, "payload")?;
    let limit = p.limit.unwrap_or(25);
    if !(1..=100).contains(&limit) {
        return Err(AppError::bad_request("invalid_payload", "limit must be between 1 and 100"));
    }
    if let Some(e) = &p.execution
        && !matches!(e.as_str(), "sync" | "async")
    {
        return Err(AppError::bad_request("invalid_payload", "execution must be sync or async"));
    }
    let f = HistoryFilter {
        operation: p.operation.clone(),
        execution: p.execution.clone(),
        execution_status: p.execution_status.clone(),
        requester_id: p.requester_id.clone(),
        format: p.format.clone(),
        in_progress: p.in_progress,
        created_from: parse_time(&p.created_from, "created_from")?,
        created_to: parse_time(&p.created_to, "created_to")?,
    };
    let fhash = blake3::hash(format!("{f:?}").as_bytes()).to_hex()[..16].to_string();
    let (high, after) = match &p.cursor {
        Some(c) => {
            let c = verify(app, c, &fhash)?;
            (c.h, Some(c.l))
        }
        None => (app.db.max_request_seq().await.map_err(db_err)?, None),
    };
    let mut rows = app.db.history_list(&f, high, after, limit + 1).await.map_err(db_err)?;
    let has_more = rows.len() as i64 > limit;
    rows.truncate(limit as usize);
    let next =
        if has_more { rows.last().map(|r| sign(app, &Cursor { h: high, l: r.get::<i64, _>("seq"), f: fhash.clone() })) } else { None };
    let items: Vec<Value> = rows.iter().map(item).collect();
    Ok(crate::rpc::ok(
        "history.list",
        &ctx.request_id,
        200,
        &serde_json::json!({"items": items, "has_more": has_more, "next_cursor": next}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GetP {
    request_id: String,
    #[serde(default)]
    limit: Option<i64>,
    #[serde(default)]
    events_cursor: Option<String>,
    #[serde(default)]
    calls_cursor: Option<String>,
}

pub async fn get(app: &Arc<App>, ctx: &ReqCtx, payload: Value) -> Result<axum::response::Response, AppError> {
    let p: GetP = typed(payload, "payload")?;
    let limit = p.limit.unwrap_or(100);
    if !(1..=500).contains(&limit) {
        return Err(AppError::bad_request("invalid_payload", "limit must be between 1 and 500"));
    }
    let row = app
        .db
        .history_get(&p.request_id)
        .await
        .map_err(db_err)?
        .ok_or_else(|| AppError::new(404, "request_not_found", "unknown request_id"))?;
    let mut req = item(&row);
    if let Value::Object(m) = &mut req {
        m.insert("options".into(), json_col(&row, "options"));
        m.insert("metadata".into(), json_col(&row, "metadata"));
    }
    let ef = format!("events:{}", p.request_id);
    let cf = format!("calls:{}", p.request_id);
    let ea = p.events_cursor.as_deref().map(|c| verify(app, c, &ef)).transpose()?.map_or(0, |c| c.l);
    let ca = p.calls_cursor.as_deref().map(|c| verify(app, c, &cf)).transpose()?.map_or(0, |c| c.l);
    let mut events = app.db.events_page(&p.request_id, ea, limit + 1).await.map_err(db_err)?;
    let mut calls = app.db.calls_page(&p.request_id, ca, limit + 1).await.map_err(db_err)?;
    let page =
        |more: bool, last: Option<i64>, f: &str| if more { last.map(|l| sign(app, &Cursor { h: 0, l, f: f.to_string() })) } else { None };
    let em = events.len() as i64 > limit;
    events.truncate(limit as usize);
    let cm = calls.len() as i64 > limit;
    calls.truncate(limit as usize);
    let enext = page(em, events.last().map(|e| e.0), &ef);
    let cnext = page(cm, calls.last().map(|c| c.0), &cf);
    let ev: Vec<Value> = events
        .into_iter()
        .map(|(_, e)| serde_json::json!({"at": rfc3339(e.at), "stage": e.stage, "event": e.event, "detail": e.detail}))
        .collect();
    let cl: Vec<Value> = calls.into_iter().map(|(_, c)| serde_json::from_str(&c).unwrap_or(Value::Null)).collect();
    let mut data = Map::new();
    data.insert("request".into(), req);
    data.insert("events".into(), serde_json::json!({"items": ev, "has_more": em, "next_cursor": enext}));
    data.insert("calls".into(), serde_json::json!({"items": cl, "has_more": cm, "next_cursor": cnext}));
    Ok(crate::rpc::ok("history.get", &ctx.request_id, 200, &Value::Object(data)))
}

// ---------------------------------------------------------------- stats

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatsP {
    #[serde(default)]
    window: Option<String>,
}

const STATS_MAX_ROWS: i64 = 100_000;

/// Activity summary for a time window (`1h`, `24h` default, `7d`, `30d`), plus a live snapshot.
pub async fn stats(app: &Arc<App>, ctx: &ReqCtx, payload: Value) -> Result<axum::response::Response, AppError> {
    use std::collections::BTreeMap;
    let p: StatsP = typed(payload, "payload")?;
    let window = p.window.unwrap_or_else(|| "24h".into());
    let dur = match window.as_str() {
        "1h" | "24h" | "7d" | "30d" => crate::config::parse_duration(&window).unwrap_or_default(),
        _ => return Err(AppError::bad_request("invalid_payload", "window must be one of 1h, 24h, 7d, 30d")),
    };
    let since = crate::db::now_ms() - dur.as_millis() as i64;
    let rows = app.db.stats_rows(since, STATS_MAX_ROWS).await.map_err(db_err)?;

    let mut by_status: BTreeMap<String, u64> = BTreeMap::new();
    let mut by_operation: BTreeMap<String, u64> = BTreeMap::new();
    let mut by_format: BTreeMap<String, u64> = BTreeMap::new();
    let mut by_requester: BTreeMap<String, u64> = BTreeMap::new();
    let mut by_model: BTreeMap<String, u64> = BTreeMap::new();
    let mut durations: Vec<u64> = Vec::new();
    let (mut pages, mut words) = (0u64, 0u64);
    let (mut calls, mut failed_calls, mut input, mut output) = (0u64, 0u64, 0u64, 0u64);
    let mut failures = Vec::new();
    for r in &rows {
        let s = |c: &str| r.try_get::<Option<String>, _>(c).ok().flatten();
        let op = s("operation").unwrap_or_default();
        // Outcome: execution status for work that ran, request status otherwise (rejected, cancelled, …).
        let outcome = match (s("request_status").as_deref(), s("execution_status")) {
            (Some("rejected"), _) => "rejected".to_string(),
            (Some("cancelled"), _) => "cancelled".to_string(),
            (Some("interrupted"), _) => "interrupted".to_string(),
            (_, Some(e)) => e,
            (Some(r), None) => r.to_string(),
            _ => "unknown".into(),
        };
        *by_status.entry(outcome.clone()).or_default() += 1;
        *by_operation.entry(op.clone()).or_default() += 1;
        *by_requester.entry(s("requester_id").unwrap_or_else(|| "unknown".into())).or_default() += 1;
        let t = json_col(r, "timings");
        if let Some(ms) = t.get("total_ms").and_then(Value::as_u64) {
            durations.push(ms);
        }
        let st = json_col(r, "statistics");
        if let Some(f) = st.get("format").and_then(Value::as_str) {
            *by_format.entry(f.to_string()).or_default() += 1;
        }
        pages += st.get("original_total_pages").and_then(Value::as_u64).unwrap_or(0);
        words += st.get("total_words").and_then(Value::as_u64).unwrap_or(0);
        let u = json_col(r, "usage");
        calls += u.get("calls").and_then(Value::as_u64).unwrap_or(0);
        if let (Some(p), Some(m)) = (u.get("provider").and_then(Value::as_str), u.get("model").and_then(Value::as_str)) {
            *by_model.entry(format!("{p}/{m}")).or_default() += 1;
        }
        failed_calls += u.get("failed_calls").and_then(Value::as_u64).unwrap_or(0);
        input += u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
        output += u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
        let err = json_col(r, "error");
        if !err.is_null() && failures.len() < 10 && outcome != "rejected" {
            failures.push(serde_json::json!({
                "at": r.try_get::<Option<i64>, _>("created_at").ok().flatten().map(rfc3339),
                "operation": op,
                "requester_id": s("requester_id"),
                "code": err.get("code"),
                "message": err.get("message"),
                "stage": err.get("stage"),
            }));
        }
    }
    durations.sort_unstable();
    let pct = |q: f64| durations.get(((durations.len() as f64 * q) as usize).min(durations.len().saturating_sub(1))).copied();
    let avg = (!durations.is_empty()).then(|| durations.iter().sum::<u64>() / durations.len() as u64);

    let breakers: serde_json::Map<String, Value> = app
        .gateway
        .breaker_states()
        .into_iter()
        .map(|(k, s)| (k.split('/').take(2).collect::<Vec<_>>().join("/"), Value::from(s)))
        .collect();
    let data = serde_json::json!({
        "window": window,
        "since": rfc3339(since),
        "truncated": rows.len() as i64 >= STATS_MAX_ROWS,
        "requests": {
            "total": rows.len(),
            "by_status": by_status,
            "by_operation": by_operation,
            "by_format": by_format,
            "by_requester": top(by_requester, 10),
            "by_model": top(by_model, 10),
        },
        "duration_ms": { "avg": avg, "p50": pct(0.5), "p95": pct(0.95), "max": durations.last() },
        "documents": { "pages": pages, "words": words },
        "llm": {
            "calls": calls,
            "failed_calls": failed_calls,
            "input_tokens": input,
            "output_tokens": output,
        },
        "recent_failures": failures,
        "live": {
            "uptime_ms": app.started.elapsed().as_millis() as u64,
            "queued": app.admission.queue_len(),
            "running": app.running_jobs.load(std::sync::atomic::Ordering::Relaxed),
            "max_running": app.cfg.max_concurrent_jobs,
            "memory_in_use_kib": app.admission.memory_in_use_kib(),
            "memory_budget_kib": app.admission.total_kib(),
            "db_writer_alive": app.writer.is_alive(),
            "default_llm": app.cfg.default_provider.as_ref().and_then(|n| app.cfg.providers.get(n)).map(|p| format!("{}/{}", p.name, p.model)),
            "providers": breakers,
        },
    });
    Ok(crate::rpc::ok("stats", &ctx.request_id, 200, &data))
}
