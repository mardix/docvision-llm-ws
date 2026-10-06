//! Standalone mock OpenAI-compatible provider for load tests.
//!
//! MOCK_ADDR=127.0.0.1:9099 MOCK_LATENCY_MS=2000-8000 MOCK_P429=0 MOCK_P5XX=0 MOCK_MAX_INFLIGHT=0 \
//!   cargo run --release --example mock_provider
//! Point the service at it: DOCVISION_LLM_BASE_URL=http://127.0.0.1:9099/v1 DOCVISION_LLM_MODEL=mock-model.
//! Counters: GET /stats.

#[path = "../tests/common/mock.rs"]
mod mock;

use std::sync::Arc;
use std::sync::atomic::Ordering;

#[tokio::main]
async fn main() {
    let st = Arc::new(mock::MockState::default());
    let env = |k: &str| std::env::var(k).ok();
    if let Some(l) = env("MOCK_LATENCY_MS") {
        let (a, b) = l.split_once('-').unwrap_or((&l, &l));
        st.set_latency(a.parse().unwrap(), b.parse().unwrap());
    }
    if let Some(p) = env("MOCK_P429") {
        *st.p429.lock().unwrap() = p.parse().unwrap();
    }
    if let Some(p) = env("MOCK_P5XX") {
        *st.p5xx.lock().unwrap() = p.parse().unwrap();
    }
    if let Some(m) = env("MOCK_MAX_INFLIGHT") {
        st.max_inflight.store(m.parse().unwrap(), Ordering::SeqCst);
    }
    let addr = env("MOCK_ADDR").unwrap_or_else(|| "127.0.0.1:9099".into());
    let l = tokio::net::TcpListener::bind(&addr).await.unwrap();
    eprintln!("mock provider on http://{addr}/v1 (stats: http://{addr}/stats)");
    axum::serve(l, mock::router(st)).await.unwrap();
}
