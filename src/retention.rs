//! Batched cleanup every 60 s: expired service-owned result/staging files, jobs, cache rows
//! and history. Never touches inputs or caller destinations.

use crate::App;
use crate::db::{WriteOp, now_ms};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::watch;

const BATCH: usize = 1000;

/// Delete up to `BATCH` files in `dir` older than `max_age`. Returns how many were removed.
pub fn sweep_dir(dir: &Path, max_age: Duration) -> usize {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    let now = SystemTime::now();
    let mut n = 0;
    for e in rd.flatten() {
        if n >= BATCH {
            break;
        }
        let Ok(meta) = e.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let old = meta.modified().ok().and_then(|m| now.duration_since(m).ok()).is_some_and(|age| age >= max_age);
        if old && std::fs::remove_file(e.path()).is_ok() {
            n += 1;
        }
    }
    n
}

pub async fn run(app: Arc<App>, mut shutdown: watch::Receiver<bool>) {
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = shutdown.changed() => return,
        }
        let now = now_ms();
        app.writer
            .send_durable(vec![WriteOp::Retention {
                result_cutoff: now - app.cfg.result_retention.as_millis() as i64,
                history_cutoff: now - app.cfg.history_retention.as_millis() as i64,
                limit: BATCH as i64,
            }])
            .await;
        // Expired job rows are gone now; drop cached copies.
        app.jobs.clear_terminal_cache();
        let (results, staging) = (app.results_dir(), app.staging_dir());
        let keep = app.cfg.result_retention;
        let stale_staging = (app.cfg.job_timeout * 2).max(Duration::from_secs(3600));
        let removed = app.cpu.run(move || sweep_dir(&results, keep) + sweep_dir(&staging, stale_staging)).await.unwrap_or(0);
        if removed > 0 {
            tracing::info!(target: "docvision_llm_ws::retention", removed, "retention removed expired files");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sweeps_only_old_files() {
        let dir = std::env::temp_dir().join(format!("docvision-ret-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.json"), b"x").unwrap();
        assert_eq!(sweep_dir(&dir, Duration::from_secs(3600)), 0);
        assert_eq!(sweep_dir(&dir, Duration::ZERO), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
