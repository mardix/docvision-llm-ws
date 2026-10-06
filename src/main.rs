//! Startup, runtime construction and graceful shutdown.

use docvision_llm_ws::{Running, config::Config, db::WriteOp, router, start};
use std::process::ExitCode;
use std::time::{Duration, Instant};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn init_tracing(cfg: &Config) {
    let filter = tracing_subscriber::EnvFilter::try_new(format!("{},sqlx=warn,hyper=warn,reqwest=warn", cfg.log_level))
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let b = tracing_subscriber::fmt().with_env_filter(filter).with_target(true);
    if cfg.log_format == "json" {
        b.json().flatten_event(true).init();
    } else {
        b.compact().init();
    }
}

fn main() -> ExitCode {
    let cfg = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("docvision-llm-ws: configuration error: {e}");
            return ExitCode::from(2);
        }
    };
    init_tracing(&cfg);
    // available_parallelism() honours cgroup CPU quotas on Linux.
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(cores)
        .max_blocking_threads(16.max(cores + 4))
        .thread_name("docvision")
        .enable_all()
        .build()
        .expect("tokio runtime");
    match rt.block_on(run(cfg)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(target: "docvision_llm_ws", error = %e, "fatal");
            eprintln!("docvision-llm-ws: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = ctrl_c.await;
}

/// Debug builds: warn when a worker thread is blocked (scheduler wake-up lag).
#[cfg(debug_assertions)]
fn blocking_detector() {
    tokio::spawn(async {
        loop {
            let t = Instant::now();
            tokio::time::sleep(Duration::from_millis(100)).await;
            let lag = t.elapsed().saturating_sub(Duration::from_millis(100));
            if lag > Duration::from_millis(100) {
                tracing::warn!(target: "docvision_llm_ws::blocking", lag_ms = lag.as_millis() as u64, "tokio worker blocked; a blocking call may be running on the I/O lane");
            }
        }
    });
}

async fn run(cfg: Config) -> Result<(), String> {
    let bind = cfg.bind.clone();
    let grace = cfg.shutdown_grace;
    let Running { app, writer_task, shutdown } = start(cfg).await?;
    #[cfg(debug_assertions)]
    blocking_detector();
    let listener = tokio::net::TcpListener::bind(&bind).await.map_err(|e| format!("bind {bind}: {e}"))?;
    tracing::info!(target: "docvision_llm_ws", bind = %bind, features = ?docvision_llm_ws::compiled_features(), "listening");

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(
        axum::serve(listener, router(app.clone()))
            .with_graceful_shutdown(async move {
                let _ = stop_rx.await;
            })
            .into_future(),
    );
    shutdown_signal().await;
    let deadline = Instant::now() + grace;
    tracing::info!(target: "docvision_llm_ws", grace_s = grace.as_secs(), "shutting down: no longer accepting work");

    // Stop dispatching queued jobs (they stay `queued` for the next start) and stop accepting.
    let _ = shutdown.send(true);
    let _ = stop_tx.send(());
    // In-flight sync requests (and running async jobs) get the grace period to finish.
    let _ = tokio::time::timeout_at(deadline.into(), server).await;
    while app.running_jobs.load(std::sync::atomic::Ordering::Relaxed) > 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Jobs still running are marked failed(interrupted) per the restart policy.
    app.writer.send_durable(vec![WriteOp::Recover { at: docvision_llm_ws::db::now_ms() }]).await;
    app.writer.flush().await;
    drop(app);
    writer_task.abort();
    tracing::info!(target: "docvision_llm_ws", "shutdown complete");
    Ok(())
}
