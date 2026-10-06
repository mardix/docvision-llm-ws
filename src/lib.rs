//! docvision-llm-ws: a lean, high-throughput document-to-Markdown HTTP service.

pub mod admission;
pub mod cache;
pub mod config;
pub mod cpu;
pub mod db;
pub mod doc;
pub mod enrich;
pub mod extract;
pub mod gateway;
pub mod history;
pub mod jobs;
pub mod llm;
pub mod markdown;
pub mod metrics;
pub mod result;
pub mod retention;
pub mod rpc;
pub mod schema;
pub mod source;
pub mod webhook;
pub mod writer;

use axum::Router;
use axum::routing::{get, post};
use config::Config;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{Semaphore, watch};

pub struct App {
    pub cfg: Config,
    pub token_hash: [u8; 32],
    pub db: db::Db,
    pub writer: writer::Writer,
    pub metrics: Arc<metrics::Metrics>,
    pub cpu: cpu::Cpu,
    pub admission: Arc<admission::Admission>,
    pub gateway: gateway::Gateway,
    pub cache: cache::Cache,
    pub jobs: jobs::Jobs,
    pub reject_cap: rpc::RateCap,
    pub job_slots: Arc<Semaphore>,
    /// Async jobs currently executing.
    pub running_jobs: std::sync::atomic::AtomicUsize,
    pub history_key: [u8; 32],
    pub started: Instant,
}

impl App {
    pub fn staging_dir(&self) -> PathBuf {
        self.cfg.data_dir.join("staging")
    }
    pub fn results_dir(&self) -> PathBuf {
        self.cfg.data_dir.join("results")
    }
}

pub struct Running {
    pub app: Arc<App>,
    pub writer_task: tokio::task::JoinHandle<()>,
    pub shutdown: watch::Sender<bool>,
}

/// Build the application: directories, DB + migrations, writer, gates, recovery, background tasks.
pub async fn start(cfg: Config) -> Result<Running, String> {
    let t0 = Instant::now();
    let step = |name: &str| tracing::debug!(target: "docvision_llm_ws::startup", step = name, elapsed_us = t0.elapsed().as_micros() as u64, "startup");
    for d in [cfg.data_dir.clone(), cfg.data_dir.join("staging"), cfg.data_dir.join("results")] {
        std::fs::create_dir_all(&d).map_err(|e| format!("cannot create {}: {e}", d.display()))?;
    }
    let db = db::Db::connect(cfg.database_url.expose()).await.map_err(|e| format!("database: {e}"))?;
    step("db_connected_and_migrated");
    let metrics = Arc::new(metrics::Metrics::default());
    let (writer, writer_task) = writer::Writer::start(&db, metrics.clone()).await.map_err(|e| format!("database writer: {e}"))?;
    let (admission, receivers) = admission::Admission::new(cfg.memory_budget, cfg.queue_depth, cfg.multipliers.clone(), metrics.clone());
    step("writer_started");
    let (shutdown, shutdown_rx) = watch::channel(false);
    let app = Arc::new(App {
        token_hash: rpc::hash_token(cfg.token.expose()),
        history_key: blake3::derive_key("doc2md-llm-ws history cursor v1", cfg.encryption_key.expose()),
        job_slots: Arc::new(Semaphore::new(cfg.max_concurrent_jobs)),
        running_jobs: Default::default(),
        cpu: cpu::Cpu::new(metrics.clone()),
        gateway: gateway::Gateway::new(metrics.clone()),
        admission: Arc::new(admission),
        cache: cache::Cache::default(),
        jobs: jobs::Jobs::default(),
        reject_cap: rpc::RateCap::default(),
        started: Instant::now(),
        metrics,
        writer,
        db,
        cfg,
    });
    step("app_built");
    jobs::recover(&app).await.map_err(|e| format!("recovery: {e}"))?;
    step("recovered");
    // Build the TLS clients off the startup path so neither startup nor the first request waits.
    let warm = app.clone();
    tokio::task::spawn_blocking(move || {
        warm.gateway.http();
        source::http_client();
    });
    tokio::spawn(jobs::dispatcher(app.clone(), receivers, shutdown_rx.clone()));
    tokio::spawn(retention::run(app.clone(), shutdown_rx));
    Ok(Running { app, writer_task, shutdown })
}

pub fn router(app: Arc<App>) -> Router {
    use tower_http::compression::CompressionLayer;
    use tower_http::compression::predicate::{NotForContentType, Predicate, SizeAbove};
    let body_cap = app.cfg.max_body_bytes.max(app.cfg.max_inline_text_bytes + (64 << 10));
    Router::new()
        .route("/rpc", post(rpc::rpc))
        .route("/metrics", get(rpc::metrics))
        .route("/livez", get(rpc::livez))
        .route("/_/dashboard", get(rpc::dashboard))
        .route("/_/doc", get(doc::doc))
        .layer(CompressionLayer::new().compress_when(SizeAbove::new(4096).and(NotForContentType::IMAGES)))
        .layer(tower_http::limit::RequestBodyLimitLayer::new(body_cap))
        .with_state(app)
}

pub fn compiled_features() -> Vec<&'static str> {
    let mut f = vec!["core"];
    for (on, name) in [
        (cfg!(feature = "s3"), "s3"),
        (cfg!(feature = "webhooks"), "webhooks"),
        (cfg!(feature = "pdf-native"), "pdf-native"),
        (cfg!(feature = "translate"), "translate"),
        (cfg!(feature = "lang-detect"), "lang-detect"),
        (cfg!(feature = "odf-epub"), "odf-epub"),
        (cfg!(feature = "pdf-render"), "pdf-render"),
        (cfg!(feature = "postgres"), "postgres"),
    ] {
        if on {
            f.push(name);
        }
    }
    f
}

pub async fn health(app: &App, ctx: &rpc::ReqCtx) -> axum::response::Response {
    let alive = app.writer.is_alive();
    let lag = app.writer.lag_ms();
    let breakers: serde_json::Map<String, Value> = app.gateway.breaker_states().into_iter().map(|(k, s)| (k, Value::from(s))).collect();
    let ready = alive && lag < 5_000;
    let data = json!({
        "status": if ready { "ok" } else { "degraded" },
        "version": env!("CARGO_PKG_VERSION"),
        "features": compiled_features(),
        "uptime_ms": app.started.elapsed().as_millis() as u64,
        "db_writer": {"alive": alive, "lag_ms": lag, "pending_ops": app.writer.pending()},
        "queue": {"depth": app.admission.queue_len(), "capacity": app.cfg.queue_depth * 2, "running_jobs": app.running_jobs.load(std::sync::atomic::Ordering::Relaxed)},
        "memory": {"in_use_kib": app.admission.memory_in_use_kib(), "budget_kib": app.admission.total_kib()},
        "providers": breakers,
        "single_flight_in_progress": app.cache.in_flight(),
    });
    if ready {
        rpc::ok("health", &ctx.request_id, 200, &data)
    } else {
        let mut e = rpc::AppError::new(503, "not_ready", "service is degraded").retry_after(5);
        e.data = serde_json::to_vec(&data).ok();
        rpc::err_response("health", &ctx.request_id, &e)
    }
}
