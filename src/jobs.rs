//! Job lifecycle: `convert` (async default, sync on request), the conversion pipeline,
//! async workers, `job.get` / `job.wait`, restart recovery and the queue dispatcher.

use crate::App;
use crate::cache;
use crate::config::{ProviderConfig, ProviderKind, Secret};
use crate::db::{JobRow, RequestFinish, RequestRow, WriteOp, now_ms, rfc3339};
use crate::extract::{self, Extracted};
use crate::gateway::Prio;
use crate::llm::{Accounting, Endpoint, LlmSection};
use crate::markdown;
use crate::result::{self, Body, BodySrc, FeatureStatus, FileRef, Header, Statistics, Timing, Trailer};
use crate::rpc::{AppError, CacheMode, ConvertPayload, Execution, Ocr, Options, Priority, ReqCtx, requester_id, typed};
use crate::source::{self, Format, Source, Staged};
use axum::body::Body as HttpBody;
use axum::response::Response;
use chacha20poly1305::aead::{Aead, KeyInit};
use dashmap::DashMap;
use futures_util::{FutureExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};

// ---------------------------------------------------------------- types

#[derive(Debug)]
pub struct JobSpec {
    pub request_id: String,
    pub job_id: Option<String>,
    pub payload: ConvertPayload,
    pub options: Options,
    pub created_at: i64,
    pub enqueued: Instant,
    /// Deadline for optional enrichment (async jobs), ahead of the hard job timeout.
    pub enrich_deadline: Option<tokio::time::Instant>,
}

pub struct JobMsg(pub Arc<JobSpec>);

/// Active-job state watchers (for `job.wait`), plus a bounded read-through cache of terminal
/// job rows (immutable until retention, which clears the cache) for fast `job.get`.
#[derive(Default)]
pub struct Jobs {
    watchers: DashMap<String, watch::Sender<&'static str>>,
    terminal: DashMap<String, Arc<JobRow>>,
}

const TERMINAL_CACHE_MAX: usize = 10_000;

impl Jobs {
    pub fn register(&self, job_id: &str) {
        self.watchers.insert(job_id.to_string(), watch::channel("queued").0);
    }
    pub fn set(&self, job_id: &str, state: &'static str) {
        if let Some(tx) = self.watchers.get(job_id) {
            let _ = tx.send(state);
        }
    }
    pub fn finish(&self, job_id: &str, state: &'static str) {
        if let Some((_, tx)) = self.watchers.remove(job_id) {
            let _ = tx.send(state);
        }
    }
    pub fn subscribe(&self, job_id: &str) -> Option<watch::Receiver<&'static str>> {
        self.watchers.get(job_id).map(|tx| tx.subscribe())
    }
    pub fn active(&self) -> usize {
        self.watchers.len()
    }
    pub fn clear_terminal_cache(&self) {
        self.terminal.clear();
    }
    /// Job row by id; terminal rows are served from memory after the first read.
    pub async fn row(&self, db: &crate::db::Db, job_id: &str) -> Result<Option<Arc<JobRow>>, sqlx::Error> {
        if let Some(r) = self.terminal.get(job_id) {
            return Ok(Some(r.clone()));
        }
        let Some(row) = db.get_job(job_id).await? else { return Ok(None) };
        let row = Arc::new(JobRow { secrets: None, ..row });
        if matches!(row.state.as_str(), "completed" | "partial" | "failed") && !self.watchers.contains_key(job_id) {
            if self.terminal.len() >= TERMINAL_CACHE_MAX {
                self.terminal.clear();
            }
            self.terminal.insert(job_id.to_string(), row.clone());
        }
        Ok(Some(row))
    }
}

/// Body of a result: produced here, or a byte range copied from a cached result file.
#[derive(Clone)]
pub enum BodyRef {
    Owned(Arc<Body>),
    Raw(Arc<Vec<u8>>),
}

impl BodyRef {
    fn src(&self) -> BodySrc<'_> {
        match self {
            BodyRef::Owned(b) => BodySrc::Body(b),
            BodyRef::Raw(r) => BodySrc::Raw(r),
        }
    }
    fn partial(&self) -> bool {
        matches!(self, BodyRef::Owned(b) if b.partial)
    }
    /// Selected fields of the body (parses a raw fragment when needed).
    fn fields(&self, names: &[&str]) -> serde_json::Map<String, Value> {
        let v = match self {
            BodyRef::Owned(b) => serde_json::to_value(&**b).unwrap_or(Value::Null),
            BodyRef::Raw(r) => {
                let mut buf = Vec::with_capacity(r.len() + 2);
                buf.push(b'{');
                buf.extend_from_slice(r);
                buf.push(b'}');
                serde_json::from_slice(&buf).unwrap_or(Value::Null)
            }
        };
        let mut out = serde_json::Map::new();
        if let Value::Object(mut m) = v {
            for n in names {
                if let Some(x) = m.remove(*n) {
                    out.insert(n.to_string(), x);
                }
            }
        }
        out
    }
}

pub struct Converted {
    pub body: BodyRef,
    pub cache_hit: bool,
    pub leader: bool,
    pub cache_key: Option<String>,
    pub fetch_ms: u64,
}

// ---------------------------------------------------------------- helpers

/// Resolve the LLM endpoint from options + config. `required` makes absence a 400.
pub fn endpoint(app: &App, opts: &Options, required: bool) -> Result<Option<Endpoint>, AppError> {
    let name = opts.llm_provider.as_deref().map(str::to_ascii_lowercase).or_else(|| app.cfg.default_provider.clone());
    // A request may fully define a provider: `openai`/`gemini` with a model use their public
    // endpoints; any other name needs `base_url` + `model` (OpenAI-compatible).
    let adhoc_kind = match name.as_deref() {
        Some("openai") => Some(ProviderKind::Openai),
        Some("gemini") => Some(ProviderKind::Gemini),
        _ if opts.llm_base_url.is_some() => Some(ProviderKind::Compatible),
        _ => None,
    };
    let cfg = match name.as_deref().and_then(|n| app.cfg.providers.get(n)) {
        Some(c) => c.clone(),
        None if opts.llm_provider.is_some() && opts.llm_model.is_none() => {
            return Err(AppError::bad_request(
                "provider_not_configured",
                format!(
                    "LLM provider `{}` is not the configured default; pass llm_model (and llm_api_key){}",
                    opts.llm_provider.as_deref().unwrap_or(""),
                    if adhoc_kind.is_some() { "" } else { " and llm_base_url" }
                ),
            ));
        }
        None if opts.llm_model.is_some() && adhoc_kind.is_some() => {
            let kind = adhoc_kind.unwrap_or(ProviderKind::Compatible);
            ProviderConfig {
                name: name.clone().unwrap_or_else(|| "custom".into()),
                kind,
                api_key: Secret(String::new()),
                base_url: match kind {
                    ProviderKind::Openai => "https://api.openai.com/v1".into(),
                    ProviderKind::Gemini => "https://generativelanguage.googleapis.com/v1beta".into(),
                    ProviderKind::Compatible => String::new(),
                },
                model: String::new(),
                max_concurrency: 16,
                timeout_ms: 300_000,
                max_retries: 3,
                max_output_tokens: 16_384,
                max_input_tokens: 128_000,
                tokens_per_page: 800,
            }
        }
        None if opts.llm_provider.is_some() => {
            return Err(AppError::bad_request(
                "provider_not_configured",
                format!(
                    "LLM provider `{}` is not configured; pass options.llm_base_url for a custom OpenAI-compatible endpoint",
                    opts.llm_provider.as_deref().unwrap_or("")
                ),
            ));
        }
        None if required => {
            return Err(AppError::bad_request(
                "provider_not_configured",
                "this conversion needs an LLM; set DOCVISION_LLM_MODEL (and DOCVISION_LLM_API_KEY) or pass llm_model in the options",
            ));
        }
        None => return Ok(None),
    };
    Ok(Some(Endpoint::resolve(&cfg, opts.llm_model.as_deref(), opts.llm_base_url.as_deref(), opts.llm_api_key.as_ref())))
}

pub fn seal(key: &[u8; 32], plain: &[u8]) -> Vec<u8> {
    let cipher = chacha20poly1305::ChaCha20Poly1305::new(key.into());
    let mut nonce = [0u8; 12];
    getrandom::getrandom(&mut nonce).expect("os rng");
    let mut out = nonce.to_vec();
    out.extend(cipher.encrypt(&nonce.into(), plain).expect("encrypt"));
    out
}

pub fn open(key: &[u8; 32], data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < 12 {
        return None;
    }
    let cipher = chacha20poly1305::ChaCha20Poly1305::new(key.into());
    let nonce: [u8; 12] = data[..12].try_into().ok()?;
    cipher.decrypt(&nonce.into(), &data[12..]).ok()
}

/// What an async job needs to run after a restart; sealed into `jobs.secrets`.
#[derive(Serialize, Deserialize)]
pub struct StoredJob {
    pub payload: ConvertPayload,
    pub options: Options,
    pub api_key: Option<String>,
}

pub fn stored_job(app: &App, row: &JobRow) -> Option<StoredJob> {
    let plain = open(app.cfg.encryption_key.expose(), row.secrets.as_deref()?)?;
    let mut s: StoredJob = serde_json::from_slice(&plain).ok()?;
    s.options.llm_api_key = s.api_key.take().map(Secret);
    Some(s)
}

fn limits(app: &App) -> extract::Limits {
    extract::Limits {
        max_archive_ratio: app.cfg.max_archive_ratio,
        max_archive_bytes: app.cfg.max_archive_bytes,
        max_pdf_pages: app.cfg.max_pdf_pages,
        max_decoded_pixels: app.cfg.max_decoded_pixels,
    }
}

fn cpu_err(e: crate::cpu::CpuError) -> AppError {
    match e {
        crate::cpu::CpuError::Panicked(m) => {
            AppError::new(422, "corrupt_document", format!("document could not be parsed ({})", m.chars().take(120).collect::<String>()))
                .stage("extraction")
        }
        crate::cpu::CpuError::Cancelled => AppError::internal("CPU task cancelled"),
    }
}

fn db_err(_: sqlx::Error) -> AppError {
    AppError::new(503, "db_unavailable", "database unavailable").retry_after(5)
}

// ---------------------------------------------------------------- convert

pub async fn convert(app: &Arc<App>, ctx: &ReqCtx, payload: Value, options: Value) -> Result<Response, AppError> {
    let payload: ConvertPayload = typed(payload, "payload")?;
    let opts: Options = typed(options, "options")?;
    opts.validate()?;
    let sync = opts.execution == Execution::Sync;
    if sync && (payload.destination.is_some() || !payload.webhooks().is_empty()) {
        return Err(AppError::bad_request("invalid_options", "destinations and webhooks require async execution"));
    }
    if let Some(d) = &payload.destination {
        result::parse_dest(d)?;
    }
    if payload.webhooks().len() > crate::webhook::MAX_WEBHOOKS {
        return Err(AppError::bad_request("invalid_webhook", format!("at most {} webhooks", crate::webhook::MAX_WEBHOOKS)));
    }
    for w in payload.webhooks() {
        crate::webhook::validate(w)?;
    }
    requester_id(payload.requester_id.as_deref())?;
    let src = source::parse(&payload.source)?;
    // Structured extraction always needs an LLM: fail now, not after the conversion ran.
    endpoint(app, &opts, opts.extract_schema.is_some())?;
    let spec = Arc::new(JobSpec {
        request_id: ctx.request_id.clone(),
        job_id: None,
        payload,
        options: opts,
        created_at: ctx.created_at,
        enqueued: Instant::now(),
        enrich_deadline: None,
    });
    if sync { run_sync(app, ctx, spec, src).await } else { accept_async(app, ctx, spec, src).await }
}

fn request_row(spec: &JobSpec, mode: &str, status: &str, job_id: Option<String>) -> RequestRow {
    RequestRow {
        request_id: spec.request_id.clone(),
        operation: "convert".into(),
        mode: Some(mode.into()),
        source: Some(source::sanitize(&spec.payload.source)),
        options: Some(spec.options.sanitized().to_string()),
        metadata: spec.payload.metadata.as_ref().map(|m| Value::Object(m.clone()).to_string()),
        request_status: status.into(),
        execution_status: Some(if mode == "async" { "queued".into() } else { "running".into() }),
        job_id,
        http_status: None,
        created_at: spec.created_at,
        requester_id: requester_id(spec.payload.requester_id.as_deref()).ok(),
    }
}

async fn accept_async(app: &Arc<App>, ctx: &ReqCtx, spec: Arc<JobSpec>, src: Source) -> Result<Response, AppError> {
    let low = spec.options.priority == Priority::Low;
    let tx = if low { app.admission.low.clone() } else { app.admission.normal.clone() };
    let permit = tx.try_reserve_owned().map_err(|_| {
        AppError::new(429, "queue_full", "the async queue is full; retry later").retry_after(app.admission.retry_after_secs())
    })?;
    let job_id = uuid::Uuid::now_v7().to_string();

    // Cheap pre-check for local sources: a cache hit completes immediately.
    if spec.options.cache == CacheMode::Use {
        if let Source::Local(_) = &src {
            if let Some(resp) = async_cache_hit(app, ctx, &spec, &src, &job_id).await? {
                return Ok(resp);
            }
        }
    }

    let stored = StoredJob {
        payload: spec.payload.clone(),
        options: {
            let mut o = spec.options.clone();
            o.llm_api_key = None;
            o
        },
        api_key: spec.options.llm_api_key.as_ref().map(|k| k.expose().clone()),
    };
    let sealed = seal(app.cfg.encryption_key.expose(), &serde_json::to_vec(&stored).map_err(|_| AppError::internal("encode job"))?);
    let mut row = request_row(&spec, "async", "accepted", Some(job_id.clone()));
    row.http_status = Some(202);
    let ops = vec![
        WriteOp::InsertRequest(row),
        WriteOp::InsertJob(JobRow {
            job_id: job_id.clone(),
            request_id: spec.request_id.clone(),
            state: "queued".into(),
            priority: if low { "low".into() } else { "normal".into() },
            created_at: spec.created_at,
            started_at: None,
            finished_at: None,
            result_path: None,
            error: None,
            secrets: Some(sealed),
            cache_key: None,
            has_webhook: !spec.payload.webhooks().is_empty(),
        }),
    ];
    if !app.writer.send_durable(ops).await {
        return Err(AppError::new(503, "db_unavailable", "the job could not be durably recorded; retry later").retry_after(5));
    }
    app.jobs.register(&job_id);
    app.metrics.job_state(None, "queued");
    // The reserved channel slot is already counted in the queue length.
    let position = app.admission.queue_len();
    let spec = Arc::new(JobSpec { job_id: Some(job_id.clone()), enqueued: Instant::now(), ..clone_spec(&spec) });
    permit.send(JobMsg(spec));
    app.admission.update_queue_metrics();
    let free = app.job_slots.available_permits();
    Ok(crate::rpc::ok(
        "convert",
        &ctx.request_id,
        202,
        &serde_json::json!({
            "status": "queued",
            "job_id": job_id,
            "queue_position": position,
            "estimated_start_ms": app.admission.estimated_start_ms(position, free),
            "cache_hit": false,
        }),
    ))
}

fn clone_spec(s: &JobSpec) -> JobSpec {
    JobSpec {
        request_id: s.request_id.clone(),
        job_id: s.job_id.clone(),
        payload: s.payload.clone(),
        options: s.options.clone(),
        created_at: s.created_at,
        enqueued: s.enqueued,
        enrich_deadline: s.enrich_deadline,
    }
}

/// Async cache hit at accept time (local sources only): write the job's result now.
async fn async_cache_hit(
    app: &Arc<App>,
    ctx: &ReqCtx,
    spec: &Arc<JobSpec>,
    src: &Source,
    job_id: &str,
) -> Result<Option<Response>, AppError> {
    let fctx = source::FetchCtx { http: source::http_client(), staging: &app.staging_dir(), max_bytes: app.cfg.max_input_bytes };
    let staged = source::fetch(&fctx, src).await?;
    let ep = endpoint(app, &spec.options, false)?;
    let (p, m) = ep.as_ref().map(|e| (e.provider.as_str(), e.model.as_str())).unwrap_or(("none", "none"));
    let key = cache::key(&staged.hash, &spec.options, p, m);
    let Some(raw) = cache_lookup(app, &key).await else { return Ok(None) };
    app.metrics.cache_hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let spec = Arc::new(JobSpec { job_id: Some(job_id.to_string()), ..clone_spec(spec) });
    let mut row = request_row(&spec, "async", "accepted", Some(job_id.to_string()));
    row.http_status = Some(202);
    let ops = vec![
        WriteOp::InsertRequest(row),
        WriteOp::InsertJob(JobRow {
            job_id: job_id.to_string(),
            request_id: spec.request_id.clone(),
            state: "running".into(),
            priority: "normal".into(),
            created_at: spec.created_at,
            started_at: Some(now_ms()),
            finished_at: None,
            result_path: None,
            error: None,
            secrets: Some(seal(
                app.cfg.encryption_key.expose(),
                &serde_json::to_vec(&StoredJob {
                    payload: spec.payload.clone(),
                    options: {
                        let mut o = spec.options.clone();
                        o.llm_api_key = None;
                        o
                    },
                    api_key: None,
                })
                .unwrap_or_default(),
            )),
            cache_key: Some(key.clone()),
            has_webhook: !spec.payload.webhooks().is_empty(),
        }),
    ];
    if !app.writer.send_durable(ops).await {
        return Err(AppError::new(503, "db_unavailable", "the job could not be durably recorded; retry later").retry_after(5));
    }
    let conv = Converted { body: BodyRef::Raw(raw), cache_hit: true, leader: false, cache_key: Some(key), fetch_ms: 0 };
    let acct = Arc::new(Accounting::new(spec.request_id.clone(), Some(app.writer.clone())));
    app.metrics.job_state(None, "running");
    let state = finalize_async(app, &spec, Ok(conv), &acct, 0, Instant::now()).await;
    Ok(Some(crate::rpc::ok(
        "convert",
        &ctx.request_id,
        202,
        &serde_json::json!({"status": state, "job_id": job_id, "queue_position": 0, "estimated_start_ms": 0, "cache_hit": true}),
    )))
}

/// Records a sync request as cancelled if the client disconnects mid-flight.
struct SyncLog<'a> {
    app: &'a App,
    request_id: String,
    done: bool,
}
impl Drop for SyncLog<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.app.writer.send(WriteOp::FinishRequest(RequestFinish {
                request_id: self.request_id.clone(),
                request_status: "cancelled".into(),
                execution_status: Some("cancelled".into()),
                finished_at: now_ms(),
                ..Default::default()
            }));
        }
    }
}

async fn run_sync(app: &Arc<App>, ctx: &ReqCtx, spec: Arc<JobSpec>, src: Source) -> Result<Response, AppError> {
    app.writer.send(WriteOp::InsertRequest(request_row(&spec, "sync", "running", None)));
    let mut log = SyncLog { app, request_id: spec.request_id.clone(), done: false };
    let acct = Arc::new(Accounting::new(spec.request_id.clone(), Some(app.writer.clone())));
    let started = Instant::now();
    let hint = source::size_hint(&src).await;
    let res = async {
        let mut mem = app.admission.reserve(app.admission.estimate(hint, None), false).await?;
        let needs = needs_llm_extract(&spec, None);
        if needs {
            if let Some(ep) = endpoint(app, &spec.options, false)? {
                if app.gateway.lane(&ep).state() == "open" {
                    return Err(AppError::new(503, "provider_unavailable", "provider circuit breaker is open").retry_after(30));
                }
            }
        }
        pipeline(app, &spec, &src, Prio::Sync, Prio::Sync, &mut mem, acct.clone(), false).await
    }
    .await;
    let timing_base = (0u64, started);
    let (status, body, err, fetch_ms, cache_hit) = match res {
        Ok(c) => {
            let st = if c.body.partial() { "partial" } else { "completed" };
            (st, Some(c.body), None, c.fetch_ms, c.cache_hit)
        }
        Err(e) => ("failed", None, Some(e), 0, false),
    };
    let timing = timing_for(&acct, body.as_ref(), fetch_ms, timing_base.0, timing_base.1, 0);
    let llm = if cache_hit { LlmSection::default() } else { acct.section() };
    let src_shown = source::sanitize(&spec.payload.source);
    let header = Header {
        schema_version: result::SCHEMA_VERSION,
        request_id: &spec.request_id,
        job_id: None,
        status,
        cache_hit,
        src_file: &src_shown,
        dest_file: None,
        files: &[],
        metadata: spec.payload.metadata.as_ref(),
    };
    let empty = Body::default();
    let err_body = err.as_ref().map(|e| e.body());
    let trailer = Trailer { error: err_body.as_ref(), llm: &llm, timing: &timing };
    let bsrc = body.as_ref().map(|b| b.src()).unwrap_or(BodySrc::Body(&empty));
    let bytes = result::result_bytes(app.cfg.max_result_bytes, &header, bsrc, &trailer);
    log.done = true;
    let finish = |http: u16, e: Option<&AppError>| {
        app.writer.send(stage_events(&spec.request_id, &timing, status, e));
        let stats = body.as_ref().map(|b| b.fields(&["statistics", "warnings", "format"])).unwrap_or_default();
        app.writer.send(WriteOp::FinishRequest(RequestFinish {
            request_id: spec.request_id.clone(),
            request_status: if e.is_some() { "failed".into() } else { "completed".into() },
            execution_status: Some(status.into()),
            execution_stage: e.and_then(|e| e.stage.map(str::to_string)),
            http_status: Some(http as i64),
            finished_at: now_ms(),
            timings: serde_json::to_string(&timing).ok(),
            statistics: stats_with_format(&stats),
            format: stats.get("format").and_then(Value::as_str).map(str::to_string),
            usage: serde_json::to_string(&llm.totals).ok(),
            warnings: stats.get("warnings").map(Value::to_string),
            error: e.map(|e| serde_json::to_string(&e.body()).unwrap_or_default()),
            result_available: false,
        }));
    };
    match (err, bytes) {
        (None, Ok(b)) => {
            finish(200, None);
            Ok(crate::rpc::json_response(200, crate::rpc::envelope("convert", &ctx.request_id, Some(&b), None), None))
        }
        (None, Err(e)) => {
            finish(e.status, Some(&e));
            Err(e)
        }
        (Some(mut e), b) => {
            finish(e.status, Some(&e));
            // Validation-style failures carry no data; later failures carry the failed result.
            if !matches!(e.status, 400 | 413 | 415 | 503) || e.stage.is_some() {
                e.data = b.ok();
            }
            Err(e)
        }
    }
}

/// Stage events for a request, buffered and flushed with the terminal state transition.
fn stage_events(request_id: &str, t: &Timing, status: &str, err: Option<&AppError>) -> WriteOp {
    let at = now_ms();
    let ev = |stage: &str, event: &str, detail: Option<String>| crate::db::EventRow {
        request_id: request_id.to_string(),
        at,
        stage: stage.into(),
        event: event.into(),
        detail,
    };
    let mut v = Vec::with_capacity(7);
    if t.queue_ms > 0 {
        v.push(ev("queue", "completed", Some(format!("{{\"ms\":{}}}", t.queue_ms))));
    }
    for (stage, ms) in [
        ("fetch", t.stages.fetch_ms),
        ("extraction", t.stages.extraction_ms),
        ("enrichment", t.stages.enrichment_ms),
        ("translation", t.stages.translation_ms),
        ("storage", t.stages.storage_ms),
    ] {
        if ms > 0 || matches!(stage, "fetch" | "extraction" | "enrichment") {
            v.push(ev(stage, "completed", Some(format!("{{\"ms\":{ms}}}"))));
        }
    }
    match err {
        Some(e) => v.push(ev(e.stage.unwrap_or("processing"), "failed", serde_json::to_string(&e.body()).ok())),
        None => v.push(ev("done", status, None)),
    }
    WriteOp::Events(v)
}

/// Stored statistics plus the detected format (used by the `stats` operation).
fn stats_with_format(fields: &serde_json::Map<String, Value>) -> Option<String> {
    let mut s = fields.get("statistics")?.clone();
    if let (Some(o), Some(f)) = (s.as_object_mut(), fields.get("format")) {
        o.insert("format".into(), f.clone());
    }
    Some(s.to_string())
}

fn timing_for(acct: &Accounting, body: Option<&BodyRef>, fetch_ms: u64, queue_ms: u64, started: Instant, storage_ms: u64) -> Timing {
    let (wall, sum) = acct.wall_and_sum_ms();
    let mut t = Timing {
        queue_ms,
        llm_wall_ms: wall,
        llm_call_sum_ms: sum,
        llm_retry_wait_ms: acct.retry_wait_ms.load(std::sync::atomic::Ordering::Relaxed),
        ..Default::default()
    };
    if let Some(BodyRef::Owned(b)) = body {
        t.stages = b.stages.clone();
    }
    t.stages.fetch_ms = fetch_ms;
    t.stages.storage_ms = storage_ms;
    t.processing_ms = started.elapsed().as_millis() as u64;
    t.total_ms = t.queue_ms + t.processing_ms;
    t
}

fn needs_llm_extract(spec: &JobSpec, format: Option<Format>) -> bool {
    match format {
        Some(Format::Pdf) => spec.options.ocr != Ocr::Off,
        Some(f) if f.is_image() => spec.options.ocr != Ocr::Off,
        Some(_) => false,
        None => {
            let s = spec.payload.source.to_ascii_lowercase();
            let s = s.split('?').next().unwrap_or("");
            spec.options.ocr != Ocr::Off && [".pdf", ".png", ".jpg", ".jpeg", ".webp"].iter().any(|e| s.ends_with(e))
        }
    }
}

async fn cache_lookup(app: &App, key: &str) -> Option<Arc<Vec<u8>>> {
    let min = now_ms() - app.cfg.cache_ttl.as_millis() as i64;
    let (path, s, e) = app.db.cache_get(key, min).await.ok()??;
    let read = async {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        let mut f = tokio::fs::File::open(&path).await?;
        f.seek(std::io::SeekFrom::Start(s as u64)).await?;
        let mut buf = vec![0u8; (e - s) as usize];
        f.read_exact(&mut buf).await?;
        Ok::<_, std::io::Error>(buf)
    };
    match read.await {
        Ok(b) => Some(Arc::new(b)),
        Err(_) => {
            app.writer.send(WriteOp::CacheDelete { key: key.to_string() });
            None
        }
    }
}

// ---------------------------------------------------------------- pipeline

#[allow(clippy::too_many_arguments)]
async fn pipeline(
    app: &Arc<App>,
    spec: &Arc<JobSpec>,
    src: &Source,
    prio: Prio,
    enrich_prio: Prio,
    mem: &mut crate::admission::MemPermit,
    acct: Arc<Accounting>,
    wait_mem: bool,
) -> Result<Converted, AppError> {
    let t = Instant::now();
    let fctx = source::FetchCtx { http: source::http_client(), staging: &app.staging_dir(), max_bytes: app.cfg.max_input_bytes };
    let staged = source::fetch(&fctx, src).await?;
    let fetch_ms = t.elapsed().as_millis() as u64;
    app.metrics.bytes_fetched.fetch_add(staged.size, std::sync::atomic::Ordering::Relaxed);
    let name = source::file_name(&spec.payload.source);
    let (path, n2) = (staged.path.clone(), name.clone());
    let format = app.cpu.run(move || source::sniff(&path, &n2)).await.map_err(cpu_err)??;
    app.admission.adjust(mem, app.admission.estimate(Some(staged.size), Some(format)), wait_mem).await?;
    let opts = &spec.options;
    let ep = endpoint(app, opts, needs_llm_extract(spec, Some(format)))?;
    let (p, m) = ep.as_ref().map(|e| (e.provider.clone(), e.model.clone())).unwrap_or(("none".into(), "none".into()));
    let key = cache::key(&staged.hash, opts, &p, &m);
    if opts.cache == CacheMode::Use {
        if let Some(raw) = cache_lookup(app, &key).await {
            app.metrics.cache_hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return Ok(Converted { body: BodyRef::Raw(raw), cache_hit: true, leader: false, cache_key: Some(key), fetch_ms });
        }
        app.metrics.cache_misses.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    let make = {
        let (app, spec, acct, ep) = (app.clone(), spec.clone(), acct.clone(), ep.clone());
        move || convert_body(app, spec, staged, format, ep, acct, prio, enrich_prio, name).boxed()
    };
    if opts.cache == CacheMode::Bypass {
        let body = make().await?;
        return Ok(Converted { body: BodyRef::Owned(body), cache_hit: false, leader: true, cache_key: None, fetch_ms });
    }
    let (leader, fut) = app.cache.flight(&key, make);
    if !leader {
        app.metrics.singleflight_joins.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    let body = fut.await?;
    Ok(Converted { body: BodyRef::Owned(body), cache_hit: !leader, leader, cache_key: Some(key), fetch_ms })
}

/// Live stage of an async job, shown by `history.list` while it runs.
fn set_stage(app: &App, spec: &JobSpec, stage: &str) {
    if let Some(job_id) = &spec.job_id {
        app.writer.send(WriteOp::RequestExecution {
            job_id: job_id.clone(),
            execution_status: "running".into(),
            stage: Some(stage.into()),
        });
    }
}

#[allow(clippy::too_many_arguments)]
async fn convert_body(
    app: Arc<App>,
    spec: Arc<JobSpec>,
    staged: Staged,
    format: Format,
    ep: Option<Endpoint>,
    acct: Arc<Accounting>,
    prio: Prio,
    enrich_prio: Prio,
    name: String,
) -> cache::FlightResult {
    let opts = &spec.options;
    let sem = Semaphore::new(app.cfg.llm_per_request_concurrency);
    set_stage(&app, &spec, "extraction");
    let t = Instant::now();
    let ex: Extracted = match format {
        Format::Pdf => extract::pdf::run(&app, &staged, opts, ep.as_ref(), &acct, &sem, prio).await?,
        f if f.is_image() => extract::image::run(&app, &staged, f, opts, ep.as_ref(), &acct, &sem, prio).await?,
        _ => {
            let (path, lim) = (staged.path.clone(), limits(&app));
            app.cpu.run(move || extract::extract_native(&path, format, &lim)).await.map_err(cpu_err)??
        }
    };
    drop(staged);
    let extraction_ms = t.elapsed().as_millis() as u64;
    if ex.partial && !opts.allow_partial {
        return Err(AppError::new(
            422,
            "partial_extraction",
            format!("some content could not be extracted: {}; set allow_partial=true to accept a partial result", ex.warnings.join("; ")),
        )
        .stage("extraction"));
    }
    let (target, overlap) = (opts.effective_chunk_size(), opts.effective_chunk_overlap());
    let Extracted { content, title_hint, segments, pages, sheet_count, slide_count, warnings, partial } = ex;
    let (content, chunks, stats) = app
        .cpu
        .run(move || {
            let (mut content, mut segs) = (content, segments);
            markdown::normalize_in_place(&mut content, &mut segs);
            let (stats, mut chunks) = markdown::analyze(&content, target, &segs);
            markdown::apply_overlap(&content, &mut chunks, overlap, &segs);
            (content, chunks, stats)
        })
        .await
        .map_err(cpu_err)?;
    let wpp = opts.words_per_page;
    let mut body = Body {
        format: Some(format),
        statistics: Statistics {
            original_total_pages: pages.map(|p| p.total),
            original_page_count_method: pages.map(|p| p.method),
            original_page_count_exact: pages.is_some_and(|p| p.exact),
            markdown_estimated_total_pages: stats.total_words.div_ceil(wpp as u64),
            markdown_words_per_page: wpp,
            total_words: stats.total_words,
            word_count_method: markdown::WORD_METHOD,
            total_characters: stats.total_characters,
            content_bytes: stats.content_bytes,
            sheet_count,
            slide_count,
            chunk_count: if opts.gen_chunks { chunks.len() } else { 0 },
        },
        chunks: if opts.gen_chunks { chunks } else { Vec::new() },
        content,
        warnings,
        partial,
        ..Default::default()
    };
    body.feature_status.insert("chunks", if opts.gen_chunks { FeatureStatus::done("structural") } else { FeatureStatus::disabled() });
    body.stages.extraction_ms = extraction_ms;
    let t2 = Instant::now();
    set_stage(&app, &spec, "enrichment");
    let cx = crate::enrich::Cx { app: &app, ep: ep.as_ref(), acct: &acct, sem: &sem, prio: enrich_prio, deadline: spec.enrich_deadline };
    crate::enrich::enrich(&cx, opts, title_hint.as_deref(), &name, &mut body).await;
    body.stages.enrichment_ms = t2.elapsed().as_millis() as u64;
    app.metrics.observe_conversion(format.name(), t.elapsed().as_millis() as f64);
    Ok(Arc::new(body))
}

// ---------------------------------------------------------------- async worker

/// Counts a job as running for as long as it is alive.
struct Running<'a>(&'a std::sync::atomic::AtomicUsize);
impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

pub async fn run_job(app: Arc<App>, spec: Arc<JobSpec>, _slot: OwnedSemaphorePermit) {
    app.running_jobs.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let _running = Running(&app.running_jobs);
    // Optional features must finish before the hard timeout, leaving time to store the result.
    let margin = (app.cfg.job_timeout / 10).min(Duration::from_secs(30));
    let spec = Arc::new(JobSpec { enrich_deadline: Some(tokio::time::Instant::now() + app.cfg.job_timeout - margin), ..clone_spec(&spec) });
    let job_id = spec.job_id.clone().unwrap_or_default();
    let queue_ms = spec.enqueued.elapsed().as_millis() as u64;
    let started = Instant::now();
    app.jobs.set(&job_id, "running");
    app.metrics.job_state(Some("queued"), "running");
    app.writer.send(WriteOp::JobStarted { job_id: job_id.clone(), at: now_ms() });
    app.writer.send(WriteOp::RequestExecution { job_id: job_id.clone(), execution_status: "running".into(), stage: Some("fetch".into()) });
    let acct = Arc::new(Accounting::new(spec.request_id.clone(), Some(app.writer.clone())));
    let (prio, enrich_prio) = if spec.options.priority == Priority::Low { (Prio::Low, Prio::Low) } else { (Prio::Extract, Prio::Enrich) };
    let res = match source::parse(&spec.payload.source) {
        Err(e) => Err(e),
        Ok(src) => {
            let work = async {
                let hint = source::size_hint(&src).await;
                let mut mem = app.admission.reserve(app.admission.estimate(hint, None), true).await?;
                pipeline(&app, &spec, &src, prio, enrich_prio, &mut mem, acct.clone(), true).await
            };
            match tokio::time::timeout(app.cfg.job_timeout, work).await {
                Ok(r) => r,
                Err(_) => Err(AppError::new(
                    504,
                    "job_timeout",
                    format!("job exceeded DOCVISION_JOB_TIMEOUT ({}s)", app.cfg.job_timeout.as_secs()),
                )
                .stage("processing")),
            }
        }
    };
    finalize_async(&app, &spec, res, &acct, queue_ms, started).await;
}

/// Write the result file, publish destinations, record the terminal state, fire the webhook.
async fn finalize_async(
    app: &Arc<App>,
    spec: &Arc<JobSpec>,
    res: Result<Converted, AppError>,
    acct: &Arc<Accounting>,
    queue_ms: u64,
    started: Instant,
) -> &'static str {
    let job_id = spec.job_id.clone().unwrap_or_default();
    let dest = spec.payload.destination.as_deref().and_then(|d| result::parse_dest(d).ok());
    let path = app.results_dir().join(format!("{job_id}.json"));
    let src_shown = source::sanitize(&spec.payload.source);
    let (mut status, body, mut err, fetch_ms, cache_hit, leader, key) = match res {
        Ok(c) => {
            (if c.body.partial() { "partial" } else { "completed" }, Some(c.body), None, c.fetch_ms, c.cache_hit, c.leader, c.cache_key)
        }
        Err(e) => ("failed", None, Some(e), 0, false, false, None),
    };
    let llm = if cache_hit { LlmSection::default() } else { acct.section() };
    let empty = Arc::new(Body::default());
    let body_ref = body.clone().unwrap_or(BodyRef::Owned(empty));
    let t_store = Instant::now();
    let content_for_md = || -> Option<String> {
        dest.as_ref()?;
        match &body_ref {
            BodyRef::Owned(b) => Some(b.content.clone()),
            BodyRef::Raw(_) => body_ref.fields(&["content"]).get("content").and_then(Value::as_str).map(str::to_string),
        }
    };
    let md = if err.is_none() { content_for_md() } else { None };

    // Timing is fixed once so repeated writes (self-referential sizes) are byte-stable.
    let fixed_timing = timing_for(acct, body.as_ref(), fetch_ms, queue_ms, started, 0);
    let write = |status: &'static str, files: Vec<FileRef>, err: Option<&AppError>| {
        let (app2, path2, body2, llm2, spec2, src2) =
            (app.clone(), path.clone(), body_ref.clone(), llm.clone(), spec.clone(), src_shown.clone());
        let mut timing = fixed_timing.clone();
        if err.is_some() {
            timing.stages.storage_ms = t_store.elapsed().as_millis() as u64;
        }
        let dest_s = dest.as_ref().map(|d| d.display());
        let eb = err.map(|e| e.body());
        let cpu = app2.cpu.clone();
        async move {
            let r = cpu
                .run(move || {
                    let h = Header {
                        schema_version: result::SCHEMA_VERSION,
                        request_id: &spec2.request_id,
                        job_id: spec2.job_id.as_deref(),
                        status,
                        cache_hit,
                        src_file: &src2,
                        dest_file: dest_s.as_deref(),
                        files: &files,
                        metadata: spec2.payload.metadata.as_ref(),
                    };
                    let t = Trailer { error: eb.as_ref(), llm: &llm2, timing: &timing };
                    result::write_file_atomic(&path2, app2.cfg.max_result_bytes, &h, body2.src(), &t).map(|r| (r, timing))
                })
                .await;
            match r {
                Ok(Ok(v)) => Ok(v),
                Ok(Err(e)) => Err(result::map_io(e)),
                Err(e) => Err(cpu_err(e)),
            }
        }
    };

    // Published file list with sizes: the JSON copy is byte-identical to the local file.
    let files = match (&dest, err.is_none()) {
        (Some(d), true) => {
            let md_len = md.as_ref().map_or(0, |m| m.len() as u64);
            vec![
                FileRef { kind: "result_json", location: d.json_display(), bytes: 0 },
                FileRef { kind: "markdown", location: d.display(), bytes: md_len },
            ]
        }
        _ => Vec::new(),
    };
    let mut written = if files.is_empty() {
        write(status, files, err.as_ref()).await
    } else {
        // Size is self-referential: iterate until the digit count is stable.
        let mut files = files;
        let mut out = write(status, files.clone(), None).await;
        for _ in 0..3 {
            match &out {
                Ok(((_, _, size), _)) if files[0].bytes != *size => {
                    files[0].bytes = *size;
                    out = write(status, files.clone(), None).await;
                }
                _ => break,
            }
        }
        out
    };
    if let (Ok(_), Some(d)) = (&written, &dest) {
        if err.is_none() {
            if let Err(e) = result::publish(d, &path, md.as_deref().unwrap_or(""), spec.options.overwrite).await {
                // Never claim a destination was written when storage failed.
                status = "failed";
                written = write(status, Vec::new(), Some(&e)).await;
                err = Some(e);
            }
        }
    }
    if let Err(e) = &written {
        status = "failed";
        err = Some(e.clone());
        let _ = write(status, Vec::new(), Some(e)).await;
    }
    let ((bs, be, size), timing) = written.unwrap_or(((0, 0, 0), Timing::default()));
    if status != "failed" && leader && spec.options.cache != CacheMode::Bypass {
        if let Some(k) = &key {
            app.writer.send(WriteOp::CachePut {
                key: k.clone(),
                path: path.display().to_string(),
                body_start: bs as i64,
                body_end: be as i64,
                size: size as i64,
                at: now_ms(),
            });
        }
    }
    let stats = body.as_ref().map(|b| b.fields(&["statistics", "warnings", "format"])).unwrap_or_default();
    let error_json = err.as_ref().map(|e| serde_json::to_string(&e.body()).unwrap_or_default());
    // The terminal state is committed before it is announced (job.wait) or delivered (webhook).
    let terminal = vec![
        stage_events(&spec.request_id, &timing, status, err.as_ref()),
        WriteOp::JobFinished {
            job_id: job_id.clone(),
            state: status.into(),
            at: now_ms(),
            result_path: Some(path.display().to_string()),
            error: error_json.clone(),
        },
        WriteOp::FinishRequest(RequestFinish {
            request_id: spec.request_id.clone(),
            request_status: "accepted".into(),
            execution_status: Some(status.into()),
            execution_stage: err.as_ref().and_then(|e| e.stage.map(str::to_string)),
            http_status: None,
            finished_at: now_ms(),
            timings: serde_json::to_string(&timing).ok(),
            statistics: stats_with_format(&stats),
            format: stats.get("format").and_then(Value::as_str).map(str::to_string),
            usage: serde_json::to_string(&llm.totals).ok(),
            warnings: stats.get("warnings").map(Value::to_string),
            error: error_json,
            result_available: true,
        }),
    ];
    if !app.writer.send_durable(terminal).await {
        tracing::error!(target: "docvision_llm_ws::jobs", job_id = %job_id, fallback = true, "terminal state could not be persisted");
    }
    app.jobs.finish(&job_id, status);
    app.metrics.job_state(Some("running"), status);
    app.admission.job_finished();
    for (i, w) in spec.payload.webhooks().iter().enumerate() {
        crate::webhook::schedule(app.clone(), job_id.clone(), spec.request_id.clone(), w.clone(), i, path.clone(), 0);
    }
    status
}

// ---------------------------------------------------------------- dispatcher & recovery

pub async fn dispatcher(app: Arc<App>, rx: crate::admission::Receivers, mut shutdown: watch::Receiver<bool>) {
    let (mut normal, mut low) = rx;
    loop {
        let slot = tokio::select! {
            s = app.job_slots.clone().acquire_owned() => match s { Ok(s) => s, Err(_) => break },
            _ = shutdown.changed() => break,
        };
        // Normal drains before low.
        let msg = tokio::select! {
            biased;
            _ = shutdown.changed() => break,
            Some(m) = normal.recv() => m,
            Some(m) = low.recv() => m,
            else => break,
        };
        app.admission.update_queue_metrics();
        tokio::spawn(run_job(app.clone(), msg.0, slot));
    }
}

/// Startup recovery: interrupted running jobs fail (never re-run billed work); queued jobs
/// are re-enqueued; pending webhook deliveries resume.
pub async fn recover(app: &Arc<App>) -> Result<(), sqlx::Error> {
    let running = app.db.jobs_in_state("running").await?;
    app.writer.send_durable(vec![WriteOp::Recover { at: now_ms() }]).await;
    if !running.is_empty() {
        tracing::warn!(target: "docvision_llm_ws::jobs", count = running.len(), "marked interrupted running jobs as failed");
    }
    let queued = app.db.jobs_in_state("queued").await?;
    let n = queued.len();
    let app2 = app.clone();
    tokio::spawn(async move {
        for row in queued {
            let Some(stored) = stored_job(&app2, &row) else {
                tracing::error!(target: "docvision_llm_ws::jobs", job_id = %row.job_id, "cannot decrypt queued job; marking failed");
                app2.writer.send(WriteOp::JobFinished {
                    job_id: row.job_id.clone(),
                    state: "failed".into(),
                    at: now_ms(),
                    result_path: None,
                    error: Some(r#"{"code":"unrecoverable","message":"job secrets could not be decrypted","stage":"restart"}"#.into()),
                });
                continue;
            };
            let low = row.priority == "low";
            let spec = Arc::new(JobSpec {
                request_id: row.request_id.clone(),
                job_id: Some(row.job_id.clone()),
                payload: stored.payload,
                options: stored.options,
                created_at: row.created_at,
                // Queue time counts from the original acceptance (saturating at process start).
                enqueued: Instant::now()
                    .checked_sub(Duration::from_millis((now_ms() - row.created_at).max(0) as u64))
                    .unwrap_or_else(Instant::now),
                enrich_deadline: None,
            });
            app2.jobs.register(&row.job_id);
            app2.metrics.job_state(None, "queued");
            let tx = if low { app2.admission.low.clone() } else { app2.admission.normal.clone() };
            if tx.send(JobMsg(spec)).await.is_err() {
                break;
            }
        }
    });
    if n > 0 {
        tracing::info!(target: "docvision_llm_ws::jobs", count = n, "re-enqueued queued jobs");
    }
    #[cfg(feature = "webhooks")]
    crate::webhook::resume(app).await;
    Ok(())
}

// ---------------------------------------------------------------- job.get / job.wait

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JobGet {
    job_id: String,
    #[serde(default)]
    fields: Option<Vec<String>>,
}

fn parse_err(s: &Option<String>) -> Value {
    s.as_deref().and_then(|e| serde_json::from_str(e).ok()).unwrap_or(Value::Null)
}

async fn job_info(app: &App, row: &JobRow) -> serde_json::Map<String, Value> {
    let mut m = serde_json::Map::new();
    m.insert("job_id".into(), row.job_id.clone().into());
    m.insert("request_id".into(), row.request_id.clone().into());
    m.insert("status".into(), row.state.clone().into());
    m.insert("priority".into(), row.priority.clone().into());
    m.insert("created_at".into(), rfc3339(row.created_at).into());
    m.insert("started_at".into(), row.started_at.map(rfc3339).into());
    m.insert("finished_at".into(), row.finished_at.map(rfc3339).into());
    m.insert("error".into(), parse_err(&row.error));
    #[cfg(feature = "webhooks")]
    {
        // Only jobs submitted with a webhook pay for the second query.
        let wh = if row.has_webhook { app.db.webhooks_for_job(&row.job_id).await.unwrap_or_default() } else { Vec::new() };
        m.insert("webhooks".into(), serde_json::to_value(wh).unwrap_or(Value::Null));
    }
    #[cfg(not(feature = "webhooks"))]
    let _ = app;
    m
}

/// Stream only the requested top-level fields of a result file (unselected values are
/// skipped by the parser without being materialized).
pub fn select_fields(path: &std::path::Path, fields: &[String]) -> std::io::Result<serde_json::Map<String, Value>> {
    use serde::de::{Deserializer, IgnoredAny, MapAccess, Visitor};
    struct V<'a>(&'a [String]);
    impl<'de> Visitor<'de> for V<'_> {
        type Value = serde_json::Map<String, Value>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a result object")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut out = serde_json::Map::new();
            while let Some(k) = map.next_key::<String>()? {
                if self.0.contains(&k) {
                    out.insert(k, map.next_value::<Value>()?);
                } else {
                    map.next_value::<IgnoredAny>()?;
                }
            }
            Ok(out)
        }
    }
    let f = std::io::BufReader::with_capacity(64 * 1024, std::fs::File::open(path)?);
    let mut de = serde_json::Deserializer::from_reader(f);
    de.deserialize_map(V(fields)).map_err(std::io::Error::other)
}

pub async fn job_get(app: &Arc<App>, ctx: &ReqCtx, payload: Value) -> Result<Response, AppError> {
    let p: JobGet = typed(payload, "payload")?;
    let row =
        app.jobs.row(&app.db, &p.job_id).await.map_err(db_err)?.ok_or_else(|| AppError::new(404, "job_not_found", "unknown job_id"))?;
    let info = job_info(app, &row).await;
    let result_path =
        row.result_path.clone().map(PathBuf::from).filter(|_| matches!(row.state.as_str(), "completed" | "partial" | "failed"));
    let mut head = serde_json::to_vec(&Value::Object(info)).map_err(|_| AppError::internal("encode"))?;
    head.pop(); // strip '}' to append "result"
    head.extend_from_slice(b",\"result\":");
    let mut prefix = crate::rpc::envelope("job.get", &ctx.request_id, Some(b""), None);
    // envelope(..., data="") ends with `"data":,"error":null}`; splice our data in.
    let tail = b",\"error\":null}";
    prefix.truncate(prefix.len() - tail.len());
    prefix.extend_from_slice(&head);
    let file = match &result_path {
        Some(p) => tokio::fs::File::open(p).await.ok(),
        None => None,
    };
    let mut suffix = Vec::with_capacity(16);
    suffix.push(b'}');
    suffix.extend_from_slice(tail);
    match (file, p.fields) {
        (None, _) => {
            prefix.extend_from_slice(b"null");
            prefix.extend_from_slice(&suffix);
            Ok(crate::rpc::json_response(200, prefix, None))
        }
        (Some(_), Some(fields)) => {
            let path = result_path.unwrap();
            let sel = app
                .cpu
                .run(move || select_fields(&path, &fields))
                .await
                .map_err(cpu_err)?
                .map_err(|_| AppError::internal("cannot read result"))?;
            serde_json::to_writer(&mut prefix, &sel).map_err(|_| AppError::internal("encode"))?;
            prefix.extend_from_slice(&suffix);
            Ok(crate::rpc::json_response(200, prefix, None))
        }
        (Some(f), None) => {
            // Stream the result file through without loading it into memory.
            let stream = futures_util::stream::once(async move { Ok::<_, std::io::Error>(bytes::Bytes::from(prefix)) })
                .chain(tokio_util::io::ReaderStream::with_capacity(f, 64 * 1024))
                .chain(futures_util::stream::once(async move { Ok(bytes::Bytes::from(suffix)) }));
            let mut r = Response::new(HttpBody::from_stream(stream));
            r.headers_mut().insert(axum::http::header::CONTENT_TYPE, axum::http::HeaderValue::from_static("application/json"));
            Ok(r)
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JobWait {
    job_id: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

pub async fn job_wait(app: &Arc<App>, ctx: &ReqCtx, payload: Value) -> Result<Response, AppError> {
    let p: JobWait = typed(payload, "payload")?;
    let timeout = p.timeout_ms.unwrap_or(30_000);
    if timeout > 30_000 {
        return Err(AppError::bad_request("invalid_payload", "timeout_ms must be <= 30000"));
    }
    let mut changed = false;
    if let Some(mut rx) = app.jobs.subscribe(&p.job_id) {
        let before = *rx.borrow_and_update();
        if let Ok(Ok(())) = tokio::time::timeout(Duration::from_millis(timeout), rx.changed()).await {
            changed = *rx.borrow() != before;
        }
    }
    let row =
        app.jobs.row(&app.db, &p.job_id).await.map_err(db_err)?.ok_or_else(|| AppError::new(404, "job_not_found", "unknown job_id"))?;
    let mut info = job_info(app, &row).await;
    info.insert("changed".into(), changed.into());
    Ok(crate::rpc::ok("job.wait", &ctx.request_id, 200, &Value::Object(info)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn seal_roundtrip() {
        let k = [7u8; 32];
        let s = seal(&k, b"secret payload");
        assert_ne!(&s[12..], b"secret payload");
        assert_eq!(open(&k, &s).unwrap(), b"secret payload");
        assert!(open(&[8u8; 32], &s).is_none());
    }
}
