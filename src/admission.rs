//! Admission control: bounded async queue lanes, a memory-byte semaphore (KiB permits),
//! optional per-key fairness, and Retry-After from the observed drain rate.

use crate::config::Multipliers;
use crate::metrics::Metrics;
use crate::rpc::AppError;
use crate::source::Format;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError, mpsc};

pub struct Admission {
    mem: Arc<Semaphore>,
    total_kib: usize,
    pub normal: mpsc::Sender<crate::jobs::JobMsg>,
    pub low: mpsc::Sender<crate::jobs::JobMsg>,
    drain: Mutex<Drain>,
    mult: Multipliers,
    metrics: Arc<Metrics>,
}

struct Drain {
    window_start: Instant,
    count: u64,
    rate: f64,
}

pub struct MemPermit {
    permit: Option<OwnedSemaphorePermit>,
    metrics: Arc<Metrics>,
}

impl MemPermit {
    pub fn kib(&self) -> usize {
        self.permit.as_ref().map_or(0, |p| p.num_permits())
    }
}

impl Drop for MemPermit {
    fn drop(&mut self) {
        if let Some(p) = self.permit.take() {
            self.metrics.memory_permits_used_kib.fetch_sub(p.num_permits() as i64, Relaxed);
        }
    }
}

pub type Receivers = (mpsc::Receiver<crate::jobs::JobMsg>, mpsc::Receiver<crate::jobs::JobMsg>);

impl Admission {
    pub fn new(budget_bytes: u64, queue_depth: usize, mult: Multipliers, metrics: Arc<Metrics>) -> (Admission, Receivers) {
        let total_kib = (budget_bytes / 1024).clamp(1, Semaphore::MAX_PERMITS as u64) as usize;
        let (ntx, nrx) = mpsc::channel(queue_depth);
        let (ltx, lrx) = mpsc::channel(queue_depth);
        (
            Admission {
                mem: Arc::new(Semaphore::new(total_kib)),
                total_kib,
                normal: ntx,
                low: ltx,
                drain: Mutex::new(Drain { window_start: Instant::now(), count: 0, rate: 0.0 }),
                mult,
                metrics,
            },
            (nrx, lrx),
        )
    }

    /// Bytes of memory a job is expected to need, from input size and format.
    pub fn estimate(&self, size: Option<u64>, format: Option<Format>) -> u64 {
        let size = size.unwrap_or(8 << 20) as f64;
        let m = match format {
            None => self.mult.ooxml,
            Some(Format::Pdf) => self.mult.pdf,
            Some(Format::Xlsx | Format::Ods) => self.mult.xlsx,
            Some(Format::Docx | Format::Pptx | Format::Odt | Format::Odp | Format::Epub) => self.mult.ooxml,
            Some(f) if f.is_image() => self.mult.image,
            Some(_) => self.mult.text,
        };
        (size * m) as u64 + (256 << 10)
    }

    fn kib(bytes: u64) -> usize {
        bytes.div_ceil(1024).max(1) as usize
    }

    fn too_big(&self, kib: usize) -> AppError {
        AppError::new(
            413,
            "job_too_large",
            format!("job needs ~{} MiB, more than the entire memory budget ({} MiB)", kib / 1024, self.total_kib / 1024),
        )
    }

    /// Seconds until the queue is expected to have room, from the observed drain rate
    /// (10 s before any job has finished).
    pub fn retry_after_secs(&self) -> u64 {
        let rate = self.drain.lock().unwrap().rate;
        if rate <= 0.0 {
            return 10;
        }
        ((self.queue_len() as f64 / rate).ceil() as u64).clamp(1, 300)
    }

    fn busy(&self) -> AppError {
        AppError::new(503, "busy", "not enough memory budget available for a synchronous conversion right now")
            .retry_after(self.retry_after_secs().min(30))
    }

    /// Reserve memory. `wait=false` (sync) fails immediately with 503 instead of queueing.
    pub async fn reserve(&self, bytes: u64, wait: bool) -> Result<MemPermit, AppError> {
        let kib = Self::kib(bytes);
        if kib > self.total_kib {
            return Err(self.too_big(kib));
        }
        let permit = if wait {
            self.mem.clone().acquire_many_owned(kib as u32).await.map_err(|_| AppError::internal("memory gate closed"))?
        } else {
            match self.mem.clone().try_acquire_many_owned(kib as u32) {
                Ok(p) => p,
                Err(TryAcquireError::NoPermits) => return Err(self.busy()),
                Err(TryAcquireError::Closed) => return Err(AppError::internal("memory gate closed")),
            }
        };
        self.metrics.memory_permits_used_kib.fetch_add(kib as i64, Relaxed);
        Ok(MemPermit { permit: Some(permit), metrics: self.metrics.clone() })
    }

    /// Re-size a reservation after sniffing the real size/format.
    pub async fn adjust(&self, p: &mut MemPermit, bytes: u64, wait: bool) -> Result<(), AppError> {
        let want = Self::kib(bytes);
        let have = p.kib();
        if want > self.total_kib {
            return Err(self.too_big(want));
        }
        match want.cmp(&have) {
            std::cmp::Ordering::Less => {
                if let Some(extra) = p.permit.as_mut().and_then(|x| x.split(have - want)) {
                    self.metrics.memory_permits_used_kib.fetch_sub(extra.num_permits() as i64, Relaxed);
                }
            }
            std::cmp::Ordering::Greater => {
                let mut more = self.reserve(((want - have) * 1024) as u64, wait).await?;
                if let (Some(mine), Some(theirs)) = (p.permit.as_mut(), more.permit.take()) {
                    mine.merge(theirs);
                }
            }
            std::cmp::Ordering::Equal => {}
        }
        Ok(())
    }

    pub fn queue_len(&self) -> usize {
        (self.normal.max_capacity() - self.normal.capacity()) + (self.low.max_capacity() - self.low.capacity())
    }

    pub fn update_queue_metrics(&self) {
        self.metrics.queue_normal.store((self.normal.max_capacity() - self.normal.capacity()) as i64, Relaxed);
        self.metrics.queue_low.store((self.low.max_capacity() - self.low.capacity()) as i64, Relaxed);
    }

    pub fn estimated_start_ms(&self, position: usize, free_slots: usize) -> u64 {
        if position <= free_slots {
            return 0;
        }
        let rate = self.drain.lock().unwrap().rate.max(0.1);
        ((position - free_slots) as f64 / rate * 1000.0) as u64
    }

    pub fn job_finished(&self) {
        let mut d = self.drain.lock().unwrap();
        d.count += 1;
        let el = d.window_start.elapsed().as_secs_f64();
        if el >= 5.0 {
            let inst = d.count as f64 / el;
            d.rate = if d.rate == 0.0 { inst } else { 0.5 * d.rate + 0.5 * inst };
            d.count = 0;
            d.window_start = Instant::now();
        }
    }

    pub fn memory_in_use_kib(&self) -> usize {
        self.total_kib - self.mem.available_permits()
    }

    pub fn total_kib(&self) -> usize {
        self.total_kib
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adm(budget: u64) -> Admission {
        let m = Multipliers { pdf: 1.4, ooxml: 4.0, xlsx: 8.0, text: 3.0, image: 1.4 };
        Admission::new(budget, 4, m, Arc::new(Metrics::default())).0
    }

    #[tokio::test]
    async fn memory_gate() {
        let a = adm(10 << 20);
        assert_eq!(a.reserve(20 << 20, true).await.err().unwrap().status, 413);
        let mut p = a.reserve(6 << 20, false).await.unwrap();
        assert_eq!(a.reserve(6 << 20, false).await.err().unwrap().status, 503);
        a.adjust(&mut p, 2 << 20, false).await.unwrap();
        assert_eq!(p.kib(), 2048);
        let q = a.reserve(6 << 20, false).await.unwrap();
        assert_eq!(a.memory_in_use_kib(), 8192);
        drop((p, q));
        assert_eq!(a.memory_in_use_kib(), 0);
    }
}
