//! Environment + `.env` configuration, validated once at startup.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

/// A value that never prints itself (Debug/Display/Serialize all emit `***`).
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Secret<T>(pub T);

impl<T> Secret<T> {
    pub fn expose(&self) -> &T {
        &self.0
    }
}
impl<T> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}
impl<T> fmt::Display for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}
impl<T> serde::Serialize for Secret<T> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("***")
    }
}
impl<'de, T: serde::Deserialize<'de>> serde::Deserialize<'de> for Secret<T> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        T::deserialize(d).map(Secret)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Openai,
    Gemini,
    Compatible,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct ProviderConfig {
    pub name: String,
    pub kind: ProviderKind,
    #[serde(skip)]
    pub api_key: Secret<String>,
    #[serde(skip)]
    pub base_url: String,
    pub model: String,
    pub max_concurrency: u32,
    pub timeout_ms: u64,
    pub max_retries: u32,
    pub max_output_tokens: u32,
    pub max_input_tokens: u32,
    pub tokens_per_page: u32,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct Multipliers {
    pub pdf: f64,
    pub ooxml: f64,
    pub xlsx: f64,
    pub text: f64,
    pub image: f64,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub token: Secret<String>,
    pub bind: String,
    pub data_dir: PathBuf,
    pub database_url: Secret<String>,
    pub encryption_key: Secret<[u8; 32]>,
    pub memory_budget: u64,
    pub queue_depth: usize,
    pub max_concurrent_jobs: usize,
    pub max_input_bytes: u64,
    pub max_result_bytes: u64,
    pub max_inline_text_bytes: usize,
    pub max_body_bytes: usize,
    pub max_pdf_pages: u32,
    pub max_decoded_pixels: u64,
    pub max_archive_ratio: u64,
    pub max_archive_bytes: u64,
    pub job_timeout: Duration,
    pub shutdown_grace: Duration,
    pub llm_per_request_concurrency: usize,
    pub providers: BTreeMap<String, ProviderConfig>,
    pub default_provider: Option<String>,
    pub provider_upload_threshold: u64,
    pub cache_ttl: Duration,
    pub result_retention: Duration,
    pub history_retention: Duration,
    pub reject_log_per_sec: u32,
    pub log_level: String,
    pub log_format: String,
    pub multipliers: Multipliers,
    pub webhook_retry_base: Duration,
}

/// Variables the configuration is read from (the process environment in production).
pub type Vars = BTreeMap<String, String>;

fn var(vars: &Vars, name: &str) -> Option<String> {
    vars.get(&format!("DOCVISION_{name}")).map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: f64 = num.parse().map_err(|_| format!("invalid duration `{s}`"))?;
    let secs = match unit.trim() {
        "ms" => n / 1000.0,
        "" | "s" => n,
        "m" => n * 60.0,
        "h" => n * 3600.0,
        "d" => n * 86400.0,
        _ => return Err(format!("invalid duration unit in `{s}`")),
    };
    Ok(Duration::from_secs_f64(secs))
}

pub fn parse_bytes(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: f64 = num.parse().map_err(|_| format!("invalid size `{s}`"))?;
    let mul: f64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "kb" => 1e3,
        "kib" | "k" => 1024.0,
        "mb" => 1e6,
        "mib" | "m" => 1024.0 * 1024.0,
        "gb" => 1e9,
        "gib" | "g" => 1024.0 * 1024.0 * 1024.0,
        _ => return Err(format!("invalid size unit in `{s}`")),
    };
    Ok((n * mul) as u64)
}

fn get<T>(vars: &Vars, name: &str, default: T, parse: impl Fn(&str) -> Result<T, String>) -> Result<T, String> {
    match var(vars, name) {
        Some(v) => parse(&v).map_err(|e| format!("DOCVISION_{name}: {e}")),
        None => Ok(default),
    }
}

fn num<T: std::str::FromStr>(s: &str) -> Result<T, String> {
    s.parse().map_err(|_| format!("invalid number `{s}`"))
}

/// Memory limit of the container (cgroup v2/v1), falling back to physical memory.
pub fn memory_limit() -> u64 {
    let read = |p: &str| std::fs::read_to_string(p).ok().and_then(|s| s.trim().parse::<u64>().ok());
    if let Some(v) = read("/sys/fs/cgroup/memory.max") {
        return v;
    }
    if let Some(v) = read("/sys/fs/cgroup/memory/memory.limit_in_bytes").filter(|v| *v < (1 << 60)) {
        return v;
    }
    if let Ok(m) = std::fs::read_to_string("/proc/meminfo") {
        if let Some(kb) =
            m.lines().find(|l| l.starts_with("MemTotal:")).and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse::<u64>().ok())
        {
            return kb * 1024;
        }
    }
    1 << 30
}

/// The default LLM, from `DOCVISION_LLM_*` (same names as the request's `llm_*` options).
/// `DOCVISION_LLM_PROVIDER` defaults to `openai`; `openai` and `gemini` use their public
/// endpoints, any other name is an OpenAI-compatible API and needs `DOCVISION_LLM_BASE_URL`.
/// The LLM is configured once `DOCVISION_LLM_MODEL` has a value.
fn load_providers(vars: &Vars) -> Result<BTreeMap<String, ProviderConfig>, String> {
    let p = |f: &str| var(vars, &format!("LLM_{f}"));
    let mut out = BTreeMap::new();
    let Some(model) = p("MODEL") else { return Ok(out) };
    let name = p("PROVIDER").unwrap_or_else(|| "openai".into()).to_ascii_lowercase();
    let kind = match name.as_str() {
        "openai" => ProviderKind::Openai,
        "gemini" => ProviderKind::Gemini,
        _ => ProviderKind::Compatible,
    };
    let base_url = p("BASE_URL").unwrap_or_else(|| match kind {
        ProviderKind::Openai => "https://api.openai.com/v1".into(),
        ProviderKind::Gemini => "https://generativelanguage.googleapis.com/v1beta".into(),
        ProviderKind::Compatible => String::new(),
    });
    if base_url.is_empty() {
        return Err(format!("DOCVISION_LLM_BASE_URL is required for provider `{name}` (only openai and gemini have a built-in endpoint)"));
    }
    let n32 = |f: &str, d: u32| -> Result<u32, String> {
        p(f).map(|v| v.parse().map_err(|_| format!("DOCVISION_LLM_{f}: invalid number"))).unwrap_or(Ok(d))
    };
    out.insert(
        name.clone(),
        ProviderConfig {
            name,
            kind,
            api_key: Secret(p("API_KEY").unwrap_or_default()),
            base_url: base_url.trim_end_matches('/').to_string(),
            model,
            max_concurrency: n32("MAX_CONCURRENCY", 64)?.max(1),
            timeout_ms: 300_000,
            max_retries: n32("MAX_RETRIES", 3)?,
            max_output_tokens: n32("MAX_OUTPUT_TOKENS", 16384)?,
            max_input_tokens: 128_000,
            tokens_per_page: 800,
        },
    );
    Ok(out)
}

impl Config {
    /// Load `.env` (real env vars win) then validate everything.
    pub fn from_env() -> Result<Config, String> {
        let _ = dotenvy::dotenv();
        Self::from_current_env()
    }

    pub fn from_current_env() -> Result<Config, String> {
        Self::from_vars(&std::env::vars().collect())
    }

    pub fn from_vars(vars: &Vars) -> Result<Config, String> {
        let token = var(vars, "TOKEN").ok_or("DOCVISION_TOKEN is required (run scripts/setup-env.sh)")?;
        if token.len() < 16 {
            return Err("DOCVISION_TOKEN must be at least 16 characters".into());
        }
        let data_dir = PathBuf::from(var(vars, "DATA_DIR").unwrap_or_else(|| "./data".into()));
        let database_url =
            var(vars, "DATABASE_URL").unwrap_or_else(|| format!("sqlite://{}?mode=rwc", data_dir.join("docvision.sqlite").display()));
        let encryption_key = match var(vars, "ENCRYPTION_KEY") {
            Some(k) => {
                use base64::Engine;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(&k)
                    .ok()
                    .filter(|b| b.len() == 32)
                    .ok_or("DOCVISION_ENCRYPTION_KEY must be 32 bytes, base64-encoded")?;
                let mut a = [0u8; 32];
                a.copy_from_slice(&bytes);
                a
            }
            // Stable default derived from the token so restarts can decrypt.
            // Context string predates the rename; changing it would break decryption of stored data.
            None => blake3::derive_key("doc2md-llm-ws encryption key v1", token.as_bytes()),
        };
        let mem_default = memory_limit() * 6 / 10;
        let memory_budget = match var(vars, "MEMORY_BUDGET") {
            Some(v) if v.ends_with('%') => {
                let pct: f64 = num(v.trim_end_matches('%'))?;
                (memory_limit() as f64 * pct / 100.0) as u64
            }
            Some(v) => parse_bytes(&v).map_err(|e| format!("DOCVISION_MEMORY_BUDGET: {e}"))?,
            None => mem_default,
        };
        let max_input_bytes = get(vars, "MAX_INPUT_BYTES", 200 << 20, parse_bytes)?;
        let result_retention = get(vars, "RESULT_RETENTION", Duration::from_secs(86400), parse_duration)?;
        let providers = load_providers(vars)?;
        let default_provider = providers.keys().next().cloned();
        let cfg = Config {
            token: Secret(token),
            bind: var(vars, "BIND").unwrap_or_else(|| "0.0.0.0:4242".into()),
            data_dir,
            database_url: Secret(database_url),
            encryption_key: Secret(encryption_key),
            memory_budget,
            queue_depth: get(vars, "QUEUE_DEPTH", 1024, num)?,
            max_concurrent_jobs: get(vars, "MAX_CONCURRENT_JOBS", 512, num)?,
            max_input_bytes,
            job_timeout: get(vars, "JOB_TIMEOUT", Duration::from_secs(900), parse_duration)?,
            providers,
            default_provider,
            result_retention,
            history_retention: get(vars, "HISTORY_RETENTION", Duration::from_secs(30 * 86400), parse_duration)?,
            log_level: var(vars, "LOG_LEVEL").unwrap_or_else(|| "info".into()),
            log_format: var(vars, "LOG_FORMAT").unwrap_or_else(|| "json".into()),
            // Fixed internal limits: sensible for every deployment, not configurable.
            max_result_bytes: max_input_bytes.max(100 << 20),
            max_inline_text_bytes: 4 << 20,
            max_body_bytes: 256 << 10,
            max_pdf_pages: 2000,
            max_decoded_pixels: 100_000_000,
            max_archive_ratio: 100,
            max_archive_bytes: 1 << 30,
            shutdown_grace: Duration::from_secs(30),
            llm_per_request_concurrency: 16,
            provider_upload_threshold: 8 << 20,
            cache_ttl: result_retention,
            reject_log_per_sec: 50,
            multipliers: Multipliers { pdf: 2.5, ooxml: 4.0, xlsx: 8.0, text: 3.0, image: 1.4 },
            webhook_retry_base: Duration::from_secs(30),
        };
        if cfg.queue_depth == 0 || cfg.max_concurrent_jobs == 0 {
            return Err("DOCVISION_QUEUE_DEPTH and DOCVISION_MAX_CONCURRENT_JOBS must be > 0".into());
        }
        if cfg.memory_budget < (1 << 20) {
            return Err("DOCVISION_MEMORY_BUDGET must be at least 1 MiB".into());
        }
        Ok(cfg)
    }

    pub fn provider(&self, name: Option<&str>) -> Option<&ProviderConfig> {
        let name = name.map(|s| s.to_ascii_lowercase()).or_else(|| self.default_provider.clone())?;
        self.providers.get(&name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_units() {
        assert_eq!(parse_duration("10ms").unwrap(), Duration::from_millis(10));
        assert_eq!(parse_duration("24h").unwrap(), Duration::from_secs(86400));
        assert_eq!(parse_duration("30").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_bytes("4MiB").unwrap(), 4 << 20);
        assert_eq!(parse_bytes("8MB").unwrap(), 8_000_000);
        assert!(parse_bytes("8XB").is_err());
    }
    #[test]
    fn default_llm() {
        let mut v = Vars::new();
        v.insert("DOCVISION_TOKEN".into(), "0123456789abcdef0".into());
        v.insert("DOCVISION_LLM_API_KEY".into(), "k".into());
        assert!(Config::from_vars(&v).unwrap().providers.is_empty(), "no model -> no LLM");
        v.insert("DOCVISION_LLM_MODEL".into(), "m".into());
        let c = Config::from_vars(&v).unwrap();
        assert_eq!(c.default_provider.as_deref(), Some("openai"));
        assert_eq!(c.providers["openai"].base_url, "https://api.openai.com/v1");
        v.insert("DOCVISION_LLM_PROVIDER".into(), "ollama".into());
        assert!(Config::from_vars(&v).unwrap_err().contains("BASE_URL"));
        v.insert("DOCVISION_LLM_BASE_URL".into(), "http://x/v1".into());
        let c = Config::from_vars(&v).unwrap();
        assert_eq!(c.providers["ollama"].kind, ProviderKind::Compatible);
    }

    #[test]
    fn secret_redacts() {
        let s = Secret("hunter2".to_string());
        assert_eq!(format!("{s:?} {s}"), "*** ***");
        assert_eq!(serde_json::to_string(&s).unwrap(), "\"***\"");
    }
}
