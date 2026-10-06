//! LLM gateway: owns all provider traffic. Per lane (provider + model + key): priority slot
//! queue with AIMD concurrency, circuit breaker, jittered retries,
//! and per-attempt accounting (including cancelled attempts).

use crate::llm::{self, Accounting, CallError, CallRecord, Endpoint, LlmRequest, LlmResponse};
use crate::metrics::Metrics;
use dashmap::DashMap;
use std::collections::VecDeque;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{Semaphore, oneshot};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prio {
    Sync = 0,
    Extract = 1,
    Enrich = 2,
    Low = 3,
}

struct Slots {
    limit: usize,
    inflight: usize,
    successes: u32,
    waiters: [VecDeque<oneshot::Sender<()>>; 4],
}

#[derive(Default)]
struct Breaker {
    failures: u32,
    open_until: Option<Instant>,
}

pub struct Lane {
    key: String,
    max: usize,
    slots: Mutex<Slots>,
    breaker: Mutex<Breaker>,
    metrics: Arc<Metrics>,
}

const BREAKER_THRESHOLD: u32 = 5;
const BREAKER_COOLDOWN: Duration = Duration::from_secs(30);

impl Lane {
    fn grant(&self, s: &mut Slots) {
        while s.inflight < s.limit {
            let Some(tx) = s.waiters.iter_mut().find_map(|q| q.pop_front()) else { break };
            if tx.send(()).is_ok() {
                s.inflight += 1;
            }
        }
        Metrics::set(&self.metrics.provider_inflight, &self.key, s.inflight as i64);
    }

    async fn acquire(self: &Arc<Self>, prio: Prio) -> SlotGuard {
        let rx = {
            let mut s = self.slots.lock().unwrap();
            let ahead = s.waiters[..=prio as usize].iter().any(|q| !q.is_empty());
            if s.inflight < s.limit && !ahead {
                s.inflight += 1;
                Metrics::set(&self.metrics.provider_inflight, &self.key, s.inflight as i64);
                return SlotGuard { lane: self.clone() };
            }
            let (tx, rx) = oneshot::channel();
            s.waiters[prio as usize].push_back(tx);
            // Hand out any free slots (queued senders of cancelled waiters are skipped).
            self.grant(&mut s);
            rx
        };
        let mut waiter = Waiter { lane: self.clone(), rx: Some(rx) };
        let rx = waiter.rx.as_mut().unwrap();
        let _ = rx.await;
        waiter.rx = None;
        SlotGuard { lane: self.clone() }
    }

    fn release(&self) {
        let mut s = self.slots.lock().unwrap();
        s.inflight = s.inflight.saturating_sub(1);
        self.grant(&mut s);
    }

    fn on_success(&self) {
        {
            let mut s = self.slots.lock().unwrap();
            s.successes += 1;
            if s.successes >= 20 {
                s.successes = 0;
                if s.limit < self.max {
                    s.limit += 1;
                    Metrics::set(&self.metrics.provider_ceiling, &self.key, s.limit as i64);
                    self.grant(&mut s);
                }
            }
        }
        let mut b = self.breaker.lock().unwrap();
        b.failures = 0;
        b.open_until = None;
        Metrics::set(&self.metrics.provider_breaker_open, &self.key, 0);
    }

    fn on_throttle(&self) {
        let mut s = self.slots.lock().unwrap();
        s.limit = (s.limit / 2).max(1);
        s.successes = 0;
        Metrics::set(&self.metrics.provider_ceiling, &self.key, s.limit as i64);
    }

    fn on_failure(&self) {
        let mut b = self.breaker.lock().unwrap();
        b.failures += 1;
        if b.failures >= BREAKER_THRESHOLD {
            b.open_until = Some(Instant::now() + BREAKER_COOLDOWN);
            Metrics::set(&self.metrics.provider_breaker_open, &self.key, 1);
        }
    }

    /// Open: fail fast. After the cooldown the breaker is half-open: calls pass, and one more
    /// failure re-opens it immediately (failures stay above threshold until a success).
    fn is_open(&self) -> bool {
        let b = self.breaker.lock().unwrap();
        b.open_until.is_some_and(|t| Instant::now() < t)
    }

    pub fn state(&self) -> &'static str {
        let b = self.breaker.lock().unwrap();
        match b.open_until {
            Some(t) if Instant::now() < t => "open",
            Some(_) => "half_open",
            None => "closed",
        }
    }
}

struct SlotGuard {
    lane: Arc<Lane>,
}
impl Drop for SlotGuard {
    fn drop(&mut self) {
        self.lane.release();
    }
}

/// A queued slot request; if dropped after being granted, the slot is returned.
struct Waiter {
    lane: Arc<Lane>,
    rx: Option<oneshot::Receiver<()>>,
}
impl Drop for Waiter {
    fn drop(&mut self) {
        if let Some(mut rx) = self.rx.take() {
            rx.close();
            if rx.try_recv().is_ok() {
                self.lane.release();
            }
        }
    }
}

/// Records an attempt as `cancelled` if its future is dropped before completion.
struct AttemptGuard<'a> {
    acct: &'a Accounting,
    rec: Option<CallRecord>,
    started: Instant,
}
impl Drop for AttemptGuard<'_> {
    fn drop(&mut self) {
        if let Some(mut r) = self.rec.take() {
            r.status = "cancelled".into();
            r.duration_ms = self.started.elapsed().as_millis() as u64;
            r.error = Some("request cancelled before the call completed".into());
            self.acct.record(r, self.started, Instant::now());
        }
    }
}

pub struct CallSpec<'a> {
    pub stage: &'static str,
    pub purpose: &'a str,
    pub span: Option<String>,
    pub prio: Prio,
}

pub struct Gateway {
    lanes: DashMap<String, Arc<Lane>>,
    http: std::sync::OnceLock<reqwest::Client>,
    metrics: Arc<Metrics>,
}

impl Gateway {
    pub fn new(metrics: Arc<Metrics>) -> Gateway {
        Gateway { lanes: DashMap::new(), http: std::sync::OnceLock::new(), metrics }
    }

    /// Shared provider client (rustls, HTTP/2, pooled keep-alive), built on first use.
    pub fn http(&self) -> &reqwest::Client {
        self.http.get_or_init(|| {
            crate::source::webpki_only(reqwest::Client::builder())
                .use_rustls_tls()
                .pool_max_idle_per_host(64)
                .tcp_keepalive(Duration::from_secs(60))
                .connect_timeout(Duration::from_secs(10))
                .build()
                .expect("provider http client")
        })
    }

    pub fn lane(&self, ep: &Endpoint) -> Arc<Lane> {
        if let Some(l) = self.lanes.get(&ep.lane_key) {
            return l.clone();
        }
        self.lanes
            .entry(ep.lane_key.clone())
            .or_insert_with(|| {
                let max = ep.cfg.max_concurrency as usize;
                Metrics::set(&self.metrics.provider_ceiling, &ep.lane_key, max as i64);
                Arc::new(Lane {
                    key: ep.lane_key.clone(),
                    max,
                    slots: Mutex::new(Slots { limit: max, inflight: 0, successes: 0, waiters: Default::default() }),
                    breaker: Mutex::new(Breaker::default()),
                    metrics: self.metrics.clone(),
                })
            })
            .clone()
    }

    pub fn breaker_states(&self) -> Vec<(String, &'static str)> {
        self.lanes.iter().map(|l| (l.key().clone(), l.value().state())).collect()
    }

    pub fn any_open(&self) -> bool {
        self.lanes.iter().any(|l| l.value().is_open())
    }

    pub async fn call(
        &self,
        ep: &Endpoint,
        req: &LlmRequest,
        spec: &CallSpec<'_>,
        req_sem: &Semaphore,
        acct: &Accounting,
    ) -> Result<LlmResponse, CallError> {
        let lane = self.lane(ep);
        let max_attempts = ep.cfg.max_retries + 1;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let base = CallRecord {
                call_id: uuid::Uuid::now_v7().to_string(),
                stage: spec.stage,
                purpose: spec.purpose.to_string(),
                provider: ep.provider.clone(),
                endpoint: ep.sanitized_endpoint(),
                model: ep.model.clone(),
                provider_request_id: None,
                status: "pending".into(),
                attempt,
                source_pages_or_chunks: spec.span.clone(),
                started_at: crate::db::rfc3339(crate::db::now_ms()),
                duration_ms: 0,
                input_tokens: None,
                output_tokens: None,
                total_tokens: None,
                cached_tokens: None,
                reasoning_tokens: None,
                usage_source: "missing",
                request_summary: format!("{} via {} ({})", spec.purpose, ep.model, spec.span.as_deref().unwrap_or("whole input")),
                response_summary: None,
                error: None,
            };
            if lane.is_open() {
                let now = Instant::now();
                acct.record(CallRecord { status: "breaker_open".into(), error: Some(CallError::BreakerOpen.message()), ..base }, now, now);
                return Err(CallError::BreakerOpen);
            }
            let _permit = req_sem.acquire().await.map_err(|_| CallError::Cancelled)?;
            let slot = lane.acquire(spec.prio).await;
            let started = Instant::now();
            let mut guard = AttemptGuard { acct, rec: Some(base), started };
            let res = llm::send(self.http(), ep, req).await;
            let ended = Instant::now();
            drop(slot);
            let mut rec = guard.rec.take().unwrap();
            rec.duration_ms = ended.duration_since(started).as_millis() as u64;
            Metrics::observe(&self.metrics.provider_latency, &ep.provider, rec.duration_ms as f64);
            match res {
                Ok(r) => {
                    lane.on_success();
                    rec.status = if r.truncated { "truncated".into() } else { "ok".into() };
                    rec.provider_request_id = r.provider_request_id.clone();
                    rec.input_tokens = r.usage.input_tokens;
                    rec.output_tokens = r.usage.output_tokens;
                    rec.total_tokens = r.usage.total_tokens;
                    rec.cached_tokens = r.usage.cached_tokens;
                    rec.reasoning_tokens = r.usage.reasoning_tokens;
                    rec.usage_source = r.usage.source();
                    rec.response_summary = Some(format!(
                        "{} output characters{}",
                        r.text.chars().count(),
                        if r.truncated { ", truncated at output limit" } else { "" }
                    ));
                    acct.record(rec, started, ended);
                    return Ok(r);
                }
                Err(e) => {
                    rec.status = e.status_label().into();
                    rec.error = Some(e.message());
                    acct.record(rec, started, ended);
                    let mut retry_after = None;
                    match &e {
                        CallError::Http { status, retry_after: ra, .. } if *status == 429 || *status == 503 => {
                            lane.on_throttle();
                            Metrics::add(&self.metrics.provider_429, &ep.provider);
                            retry_after = *ra;
                            if *status == 503 {
                                lane.on_failure();
                            }
                        }
                        // The breaker counts 5xx and timeouts; connection errors are retried only.
                        CallError::Http { status, .. } if *status >= 500 => lane.on_failure(),
                        CallError::Timeout => lane.on_failure(),
                        _ => {}
                    }
                    if !e.retryable() || attempt >= max_attempts {
                        return Err(e);
                    }
                    // Jittered exponential backoff; the slot and permit are released while waiting.
                    let backoff = Duration::from_millis((250u64 << (attempt - 1).min(6)).min(30_000));
                    let jitter = Duration::from_millis(fastrand::u64(0..=backoff.as_millis() as u64 / 2));
                    let wait = retry_after.unwrap_or(Duration::ZERO).max(backoff + jitter).min(Duration::from_secs(60));
                    acct.retry_wait_ms.fetch_add(wait.as_millis() as u64, Relaxed);
                    drop(_permit);
                    tokio::time::sleep(wait).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lane(max: usize) -> Arc<Lane> {
        Arc::new(Lane {
            key: "t".into(),
            max,
            slots: Mutex::new(Slots { limit: max, inflight: 0, successes: 0, waiters: Default::default() }),
            breaker: Mutex::new(Breaker::default()),
            metrics: Arc::new(Metrics::default()),
        })
    }

    #[test]
    fn aimd() {
        let l = lane(64);
        l.on_throttle();
        assert_eq!(l.slots.lock().unwrap().limit, 32);
        for _ in 0..20 {
            l.on_success();
        }
        assert_eq!(l.slots.lock().unwrap().limit, 33);
        for _ in 0..10 {
            l.on_throttle();
        }
        assert_eq!(l.slots.lock().unwrap().limit, 1);
    }

    #[test]
    fn breaker_transitions() {
        let l = lane(4);
        for _ in 0..4 {
            l.on_failure();
        }
        assert_eq!(l.state(), "closed");
        l.on_failure();
        assert_eq!(l.state(), "open");
        l.breaker.lock().unwrap().open_until = Some(Instant::now() - Duration::from_secs(1));
        assert_eq!(l.state(), "half_open");
        assert!(!l.is_open());
        l.on_failure();
        assert_eq!(l.state(), "open");
        l.breaker.lock().unwrap().open_until = Some(Instant::now() - Duration::from_secs(1));
        l.on_success();
        assert_eq!(l.state(), "closed");
    }

    #[tokio::test]
    async fn priority_order_and_cancel_safety() {
        let l = lane(1);
        let held = l.acquire(Prio::Sync).await;
        let l2 = l.clone();
        let low = tokio::spawn(async move {
            let _g = l2.acquire(Prio::Low).await;
            Instant::now()
        });
        tokio::task::yield_now().await;
        let l3 = l.clone();
        let high = tokio::spawn(async move {
            let _g = l3.acquire(Prio::Sync).await;
            Instant::now()
        });
        // A cancelled waiter must not leak a slot.
        let l4 = l.clone();
        let cancelled = tokio::spawn(async move {
            let _g = l4.acquire(Prio::Extract).await;
        });
        tokio::task::yield_now().await;
        cancelled.abort();
        tokio::time::sleep(Duration::from_millis(10)).await;
        drop(held);
        let (h, lo) = (high.await.unwrap(), low.await.unwrap());
        assert!(h <= lo);
        assert_eq!(l.slots.lock().unwrap().inflight, 0);
    }
}
