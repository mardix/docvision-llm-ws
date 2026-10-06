//! Test harness: in-process service on an ephemeral port with a mock provider.

#![allow(dead_code)]

pub mod fixtures;
pub mod mock;

use docvision_llm_ws::config::{Config, Vars};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;

pub const TOKEN: &str = "test-token-0123456789abcdef";

pub struct TestServer {
    pub url: String,
    pub client: reqwest::Client,
    pub dir: PathBuf,
    pub app: Arc<docvision_llm_ws::App>,
    pub mock: Arc<mock::MockState>,
    pub mock_url: String,
    pub shutdown: tokio::sync::watch::Sender<bool>,
}

pub fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("docvision-test-{tag}-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

pub fn base_vars(dir: &std::path::Path, mock_url: &str) -> Vars {
    let mut v = Vars::new();
    for (k, val) in [
        ("DOCVISION_TOKEN", TOKEN),
        ("DOCVISION_DATA_DIR", dir.to_str().unwrap()),
        ("DOCVISION_LLM_PROVIDER", "openai"),
        ("DOCVISION_LLM_BASE_URL", mock_url),
        ("DOCVISION_LLM_MODEL", "mock-model"),
        ("DOCVISION_LLM_API_KEY", "sk-mock-secret"),
        ("DOCVISION_LLM_MAX_OUTPUT_TOKENS", "8000"),
        ("DOCVISION_MEMORY_BUDGET", "512MiB"),
        ("DOCVISION_LOG_FORMAT", "text"),
    ] {
        v.insert(k.to_string(), val.to_string());
    }
    v
}

pub async fn server_with(dir: Option<PathBuf>, mock: Option<(Arc<mock::MockState>, String)>, tweak: impl FnOnce(&mut Vars)) -> TestServer {
    server_full(dir, mock, tweak, |_| {}).await
}

/// Like `server_with`, plus direct changes to internal (non-env) limits.
pub async fn server_cfg(tweak_cfg: impl FnOnce(&mut Config)) -> TestServer {
    server_full(None, None, |_| {}, tweak_cfg).await
}

pub async fn server_full(
    dir: Option<PathBuf>,
    mock: Option<(Arc<mock::MockState>, String)>,
    tweak: impl FnOnce(&mut Vars),
    tweak_cfg: impl FnOnce(&mut Config),
) -> TestServer {
    let (mock, mock_url) = match mock {
        Some(m) => m,
        None => {
            let st = Arc::new(mock::MockState::default());
            let u = mock::spawn(st.clone()).await;
            (st, u)
        }
    };
    let dir = dir.unwrap_or_else(|| temp_dir("srv"));
    let mut vars = base_vars(&dir, &mock_url);
    tweak(&mut vars);
    let mut cfg = Config::from_vars(&vars).unwrap();
    tweak_cfg(&mut cfg);
    let running = docvision_llm_ws::start(cfg).await.unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let router = docvision_llm_ws::router(running.app.clone());
    tokio::spawn(async move { axum::serve(l, router).await.unwrap() });
    TestServer {
        url: format!("http://{addr}"),
        client: reqwest::Client::new(),
        dir,
        app: running.app,
        mock,
        mock_url,
        shutdown: running.shutdown,
    }
}

pub async fn server() -> TestServer {
    server_with(None, None, |_| {}).await
}

impl TestServer {
    pub async fn rpc(&self, op: &str, payload: Value, options: Value) -> (u16, Value) {
        self.rpc_raw(json!({"operation": op, "payload": payload, "options": options})).await
    }

    pub async fn rpc_raw(&self, body: Value) -> (u16, Value) {
        let r = self.client.post(format!("{}/rpc", self.url)).header("x-access-token", TOKEN).json(&body).send().await.unwrap();
        let s = r.status().as_u16();
        let v = r.json::<Value>().await.unwrap_or(Value::Null);
        (s, v)
    }

    pub fn write(&self, name: &str, bytes: &[u8]) -> String {
        let p = self.dir.join("inputs");
        std::fs::create_dir_all(&p).unwrap();
        let f = p.join(name);
        std::fs::write(&f, bytes).unwrap();
        f.to_str().unwrap().to_string()
    }

    pub async fn convert_sync(&self, path: &str, options: Value) -> (u16, Value) {
        let mut o = options;
        o["execution"] = json!("sync");
        self.rpc("convert", json!({"source": path}), o).await
    }

    /// Wait for an async job to finish and return `job.get` data.
    pub async fn wait_job(&self, job_id: &str) -> Value {
        for _ in 0..200 {
            let (_, w) = self.rpc("job.wait", json!({"job_id": job_id, "timeout_ms": 5000}), json!({})).await;
            let st = w["data"]["status"].as_str().unwrap_or("");
            if matches!(st, "completed" | "partial" | "failed") {
                let (_, g) = self.rpc("job.get", json!({"job_id": job_id}), json!({})).await;
                return g["data"].clone();
            }
        }
        panic!("job {job_id} did not finish");
    }

    /// Wait until the DB writer has committed everything queued so far.
    pub async fn flush(&self) {
        self.app.writer.flush().await;
    }
}
