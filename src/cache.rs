//! Content-addressed result cache key and single-flight of identical in-flight conversions.

use crate::result::Body;
use crate::rpc::{AppError, Options};
use dashmap::DashMap;
use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared, WeakShared};
use std::sync::Arc;

pub type FlightResult = Result<Arc<Body>, AppError>;
type Flight = Shared<BoxFuture<'static, FlightResult>>;
type Weak = WeakShared<BoxFuture<'static, FlightResult>>;

/// blake3(source) + normalized output-affecting options + provider + model + extractor version.
pub fn key(source_hash: &blake3::Hash, opts: &Options, provider: &str, model: &str) -> String {
    let mut v = opts.sanitized();
    if let Some(o) = v.as_object_mut() {
        // Delivery options and the provider/model (hashed separately) do not affect the output.
        for k in ["execution", "overwrite", "cache", "priority", "llm_provider", "llm_model"] {
            o.remove(k);
        }
        o.insert("pdf_skip_local_processing".into(), serde_json::Value::Bool(opts.skip_local()));
    }
    let mut h = blake3::Hasher::new();
    h.update(source_hash.as_bytes());
    h.update(serde_json::to_string(&v).unwrap_or_default().as_bytes());
    h.update(provider.as_bytes());
    h.update(b"\0");
    h.update(model.as_bytes());
    h.update(b"\0");
    h.update(crate::extract::EXTRACTOR_VERSION.as_bytes());
    h.finalize().to_hex().to_string()
}

/// Holds only weak references: when every waiter is gone (e.g. a cancelled sync request),
/// the conversion future is dropped, which cancels its provider calls.
#[derive(Default)]
pub struct Cache {
    flights: Arc<DashMap<String, (u64, Weak)>>,
}

/// Removes the map entry when the flight finishes or is dropped (only if it is still dead or ours).
struct Unregister {
    flights: Arc<DashMap<String, (u64, Weak)>>,
    key: String,
    id: u64,
}

impl Drop for Unregister {
    fn drop(&mut self) {
        self.flights.remove_if(&self.key, |_, (id, _)| *id == self.id);
    }
}

static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl Cache {
    /// Join an identical in-flight conversion or start one. Returns (is_leader, future).
    pub fn flight<F>(&self, key: &str, make: F) -> (bool, Flight)
    where
        F: FnOnce() -> BoxFuture<'static, FlightResult>,
    {
        if let Some(f) = self.flights.get(key).and_then(|w| w.1.upgrade()) {
            return (false, f);
        }
        let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let start = |make: F| {
            let guard = Unregister { flights: self.flights.clone(), key: key.to_string(), id };
            let inner = make();
            async move {
                let _guard = guard;
                inner.await
            }
            .boxed()
            .shared()
        };
        match self.flights.entry(key.to_string()) {
            dashmap::Entry::Occupied(mut o) => {
                if let Some(f) = o.get().1.upgrade() {
                    return (false, f);
                }
                let fut = start(make);
                o.insert((id, fut.downgrade().expect("fresh shared future")));
                (true, fut)
            }
            dashmap::Entry::Vacant(v) => {
                let fut = start(make);
                v.insert((id, fut.downgrade().expect("fresh shared future")));
                (true, fut)
            }
        }
    }

    pub fn in_flight(&self) -> usize {
        self.flights.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn single_flight_runs_once() {
        let c = Cache::default();
        let runs = Arc::new(AtomicUsize::new(0));
        let mut futs = Vec::new();
        let mut leaders = 0;
        for _ in 0..50 {
            let r = runs.clone();
            let (leader, f) = c.flight("k", move || {
                async move {
                    r.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    Ok(Arc::new(Body::default()))
                }
                .boxed()
            });
            leaders += leader as usize;
            futs.push(f);
        }
        for f in futs {
            assert!(f.await.is_ok());
        }
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert_eq!(leaders, 1);
        assert_eq!(c.in_flight(), 0);
    }

    #[test]
    fn keys_ignore_delivery_options() {
        let h = blake3::hash(b"doc");
        let a = Options::default();
        let b = Options { overwrite: true, execution: crate::rpc::Execution::Sync, ..Default::default() };
        assert_eq!(key(&h, &a, "p", "m"), key(&h, &b, "p", "m"));
        let c = Options { chunk_size: 500, ..Default::default() };
        assert_ne!(key(&h, &a, "p", "m"), key(&h, &c, "p", "m"));
        assert_ne!(key(&h, &a, "p", "m"), key(&h, &a, "p", "m2"));
    }
}
