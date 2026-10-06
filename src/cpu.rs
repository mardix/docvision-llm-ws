//! CPU lane: parsers run on blocking threads, gated to `available_parallelism()` at a time,
//! each wrapped in `catch_unwind` so a malformed document becomes a 422, never a crash.

use crate::metrics::Metrics;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use tokio::sync::Semaphore;

#[derive(Clone)]
pub struct Cpu {
    permits: Arc<Semaphore>,
    pub size: usize,
    metrics: Arc<Metrics>,
}

#[derive(Debug)]
pub enum CpuError {
    Panicked(String),
    Cancelled,
}

impl Cpu {
    pub fn new(metrics: Arc<Metrics>) -> Cpu {
        let size = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2);
        Cpu { permits: Arc::new(Semaphore::new(size)), size, metrics }
    }

    pub async fn run<T, F>(&self, f: F) -> Result<T, CpuError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let _permit = self.permits.acquire().await.map_err(|_| CpuError::Cancelled)?;
        let m = self.metrics.clone();
        m.cpu_busy.fetch_add(1, Relaxed);
        let res = tokio::task::spawn_blocking(move || catch_unwind(AssertUnwindSafe(f))).await;
        self.metrics.cpu_busy.fetch_sub(1, Relaxed);
        match res {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(p)) => {
                let msg = p
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| p.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "parser panicked".into());
                Err(CpuError::Panicked(msg))
            }
            Err(_) => Err(CpuError::Cancelled),
        }
    }
}
