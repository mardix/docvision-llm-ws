//! Minimal Prometheus registry: atomics and label maps, rendered as text format.

use dashmap::DashMap;
use std::fmt::Write;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering::Relaxed};

const BUCKETS_MS: [f64; 12] = [5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0, 10000.0, 30000.0];

#[derive(Default)]
pub struct Histogram {
    buckets: [AtomicU64; 12],
    count: AtomicU64,
    sum_ms: AtomicU64,
}

impl Histogram {
    pub fn observe_ms(&self, ms: f64) {
        for (i, b) in BUCKETS_MS.iter().enumerate() {
            if ms <= *b {
                self.buckets[i].fetch_add(1, Relaxed);
            }
        }
        self.count.fetch_add(1, Relaxed);
        self.sum_ms.fetch_add(ms as u64, Relaxed);
    }
    fn render(&self, out: &mut String, name: &str, labels: &str) {
        let sep = if labels.is_empty() { "" } else { "," };
        for (i, b) in BUCKETS_MS.iter().enumerate() {
            let _ = writeln!(out, "{name}_bucket{{{labels}{sep}le=\"{b}\"}} {}", self.buckets[i].load(Relaxed));
        }
        let c = self.count.load(Relaxed);
        let _ = writeln!(out, "{name}_bucket{{{labels}{sep}le=\"+Inf\"}} {c}");
        let _ = writeln!(out, "{name}_sum{{{labels}}} {}", self.sum_ms.load(Relaxed));
        let _ = writeln!(out, "{name}_count{{{labels}}} {c}");
    }
}

#[derive(Default)]
pub struct Metrics {
    pub queue_normal: AtomicI64,
    pub queue_low: AtomicI64,
    pub memory_permits_used_kib: AtomicI64,
    pub cpu_busy: AtomicI64,
    pub writer_batches: AtomicU64,
    pub writer_ops: AtomicU64,
    pub writer_last_batch: AtomicU64,
    pub writer_lag_ms: AtomicU64,
    pub writer_dropped: AtomicU64,
    pub cache_hits: AtomicU64,
    pub cache_misses: AtomicU64,
    pub singleflight_joins: AtomicU64,
    pub bytes_fetched: AtomicU64,
    pub rpc_total: DashMap<(String, u16), AtomicU64>,
    pub jobs_by_state: DashMap<&'static str, AtomicI64>,
    pub conversion_ms: DashMap<String, Histogram>,
    pub provider_inflight: DashMap<String, AtomicI64>,
    pub provider_ceiling: DashMap<String, AtomicI64>,
    pub provider_429: DashMap<String, AtomicU64>,
    pub provider_breaker_open: DashMap<String, AtomicI64>,
    pub provider_latency: DashMap<String, Histogram>,
}

pub fn rss_bytes() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    let kb: u64 = s.lines().find(|l| l.starts_with("VmRSS:"))?.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

impl Metrics {
    pub fn rpc(&self, op: &str, status: u16) {
        if let Some(c) = self.rpc_total.get(&(op.to_string(), status)) {
            c.fetch_add(1, Relaxed);
            return;
        }
        self.rpc_total.entry((op.to_string(), status)).or_default().fetch_add(1, Relaxed);
    }
    pub fn job_state(&self, from: Option<&'static str>, to: &'static str) {
        if let Some(f) = from {
            self.jobs_by_state.entry(f).or_default().fetch_sub(1, Relaxed);
        }
        self.jobs_by_state.entry(to).or_default().fetch_add(1, Relaxed);
    }
    pub fn set(map: &DashMap<String, AtomicI64>, key: &str, v: i64) {
        match map.get(key) {
            Some(a) => a.store(v, Relaxed),
            None => map.entry(key.to_string()).or_default().store(v, Relaxed),
        }
    }
    pub fn add(map: &DashMap<String, AtomicU64>, key: &str) {
        map.entry(key.to_string()).or_default().fetch_add(1, Relaxed);
    }
    pub fn observe_conversion(&self, format: &str, ms: f64) {
        Self::observe(&self.conversion_ms, format, ms);
    }
    pub fn observe(map: &DashMap<String, Histogram>, key: &str, ms: f64) {
        match map.get(key) {
            Some(h) => h.observe_ms(ms),
            None => map.entry(key.to_string()).or_default().observe_ms(ms),
        }
    }

    pub fn render(&self) -> String {
        let mut o = String::with_capacity(4096);
        let g = |o: &mut String, n: &str, v: i64| {
            let _ = writeln!(o, "{n} {v}");
        };
        g(&mut o, "docvision_queue_depth{lane=\"normal\"}", self.queue_normal.load(Relaxed));
        g(&mut o, "docvision_queue_depth{lane=\"low\"}", self.queue_low.load(Relaxed));
        g(&mut o, "docvision_memory_permits_used_kib", self.memory_permits_used_kib.load(Relaxed));
        g(&mut o, "docvision_cpu_pool_busy", self.cpu_busy.load(Relaxed));
        g(&mut o, "docvision_db_writer_batches_total", self.writer_batches.load(Relaxed) as i64);
        g(&mut o, "docvision_db_writer_ops_total", self.writer_ops.load(Relaxed) as i64);
        g(&mut o, "docvision_db_writer_last_batch_size", self.writer_last_batch.load(Relaxed) as i64);
        g(&mut o, "docvision_db_writer_lag_ms", self.writer_lag_ms.load(Relaxed) as i64);
        g(&mut o, "docvision_db_writer_dropped_total", self.writer_dropped.load(Relaxed) as i64);
        g(&mut o, "docvision_cache_hits_total", self.cache_hits.load(Relaxed) as i64);
        g(&mut o, "docvision_cache_misses_total", self.cache_misses.load(Relaxed) as i64);
        g(&mut o, "docvision_singleflight_joins_total", self.singleflight_joins.load(Relaxed) as i64);
        g(&mut o, "docvision_bytes_fetched_total", self.bytes_fetched.load(Relaxed) as i64);
        if let Some(rss) = rss_bytes() {
            g(&mut o, "docvision_process_rss_bytes", rss as i64);
        }
        for e in self.rpc_total.iter() {
            let _ = writeln!(o, "docvision_rpc_total{{operation=\"{}\",status=\"{}\"}} {}", e.key().0, e.key().1, e.value().load(Relaxed));
        }
        for e in self.jobs_by_state.iter() {
            let _ = writeln!(o, "docvision_jobs{{state=\"{}\"}} {}", e.key(), e.value().load(Relaxed));
        }
        for (name, map) in [
            ("docvision_provider_inflight", &self.provider_inflight),
            ("docvision_provider_aimd_ceiling", &self.provider_ceiling),
            ("docvision_provider_breaker_open", &self.provider_breaker_open),
        ] {
            for e in map.iter() {
                let _ = writeln!(o, "{name}{{provider=\"{}\"}} {}", e.key(), e.value().load(Relaxed));
            }
        }
        for e in self.provider_429.iter() {
            let _ = writeln!(o, "docvision_provider_429_total{{provider=\"{}\"}} {}", e.key(), e.value().load(Relaxed));
        }
        for e in self.provider_latency.iter() {
            e.value().render(&mut o, "docvision_provider_latency_ms", &format!("provider=\"{}\"", e.key()));
        }
        for e in self.conversion_ms.iter() {
            e.value().render(&mut o, "docvision_conversion_duration_ms", &format!("format=\"{}\"", e.key()));
        }
        o
    }
}
