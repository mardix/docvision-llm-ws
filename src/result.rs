//! Result schema and streaming serialization.
//!
//! A result file is one JSON object written as three fragments:
//! header (per request: ids, status, locations, metadata), body (content-derived, cacheable)
//! and trailer (error, LLM accounting, timing). Cache hits copy the body byte range verbatim.

use crate::llm::LlmSection;
use crate::markdown::{ChunkRange, ChunksSer};
use crate::rpc::{AppError, ErrorBody};
use crate::source::Format;
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize)]
pub struct FileRef {
    pub kind: &'static str,
    pub location: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Header<'a> {
    pub schema_version: u32,
    pub request_id: &'a str,
    pub job_id: Option<&'a str>,
    pub status: &'a str,
    pub cache_hit: bool,
    pub src_file: &'a str,
    pub dest_file: Option<&'a str>,
    pub files: &'a [FileRef],
    pub metadata: Option<&'a serde_json::Map<String, serde_json::Value>>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Statistics {
    pub original_total_pages: Option<u32>,
    pub original_page_count_method: Option<&'static str>,
    pub original_page_count_exact: bool,
    pub markdown_estimated_total_pages: u64,
    pub markdown_words_per_page: u32,
    pub total_words: u64,
    pub word_count_method: &'static str,
    pub total_characters: u64,
    pub content_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sheet_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slide_count: Option<u32>,
    pub chunk_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct FeatureStatus {
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl FeatureStatus {
    pub fn disabled() -> FeatureStatus {
        FeatureStatus { status: "disabled", method: None, error: None }
    }
    pub fn done(method: &'static str) -> FeatureStatus {
        FeatureStatus { status: "completed", method: Some(method), error: None }
    }
    pub fn failed(method: &'static str, e: String) -> FeatureStatus {
        FeatureStatus { status: "failed", method: Some(method), error: Some(e) }
    }
    /// The LLM did not deliver; the local method was used instead (`error` says why).
    pub fn fallback(e: String) -> FeatureStatus {
        FeatureStatus { status: "completed", method: Some("local_fallback"), error: Some(e) }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LanguageDetection {
    pub code: String,
    pub method: &'static str,
    pub confidence: f64,
}

/// Content-derived part of a result (shared by cache hits and single-flight joiners).
#[derive(Debug, Clone, Default)]
pub struct Body {
    pub format: Option<Format>,
    pub content: String,
    pub title: Option<String>,
    pub summary: Option<String>,
    /// Structured data matching `extract_schema` (`None` when not requested or failed).
    pub extracted: Option<serde_json::Value>,
    pub chunks: Vec<ChunkRange>,
    pub language: Option<String>,
    pub language_detection: Option<LanguageDetection>,
    pub translated_content: Option<String>,
    pub translated_language: Option<String>,
    pub statistics: Statistics,
    pub translated_statistics: Option<Statistics>,
    pub feature_status: BTreeMap<&'static str, FeatureStatus>,
    pub warnings: Vec<String>,
    pub partial: bool,
    /// Stage durations of the conversion that produced this body (not serialized).
    pub stages: Stages,
}

impl Serialize for Body {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct View<'a> {
            format: Option<Format>,
            content: &'a str,
            title: &'a Option<String>,
            summary: &'a Option<String>,
            extracted: &'a Option<serde_json::Value>,
            chunks: ChunksSer<'a>,
            language: &'a Option<String>,
            language_detection: &'a Option<LanguageDetection>,
            translated_content: &'a Option<String>,
            translated_language: &'a Option<String>,
            statistics: &'a Statistics,
            translated_statistics: &'a Option<Statistics>,
            feature_status: &'a BTreeMap<&'static str, FeatureStatus>,
            warnings: &'a [String],
        }
        View {
            format: self.format,
            content: &self.content,
            title: &self.title,
            summary: &self.summary,
            extracted: &self.extracted,
            chunks: ChunksSer { content: &self.content, chunks: &self.chunks },
            language: &self.language,
            language_detection: &self.language_detection,
            translated_content: &self.translated_content,
            translated_language: &self.translated_language,
            statistics: &self.statistics,
            translated_statistics: &self.translated_statistics,
            feature_status: &self.feature_status,
            warnings: &self.warnings,
        }
        .serialize(s)
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Stages {
    pub fetch_ms: u64,
    pub extraction_ms: u64,
    pub enrichment_ms: u64,
    pub translation_ms: u64,
    pub storage_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Timing {
    pub queue_ms: u64,
    pub processing_ms: u64,
    pub total_ms: u64,
    pub stages: Stages,
    pub llm_wall_ms: u64,
    pub llm_call_sum_ms: u64,
    pub llm_retry_wait_ms: u64,
}

#[derive(Serialize)]
pub struct Trailer<'a> {
    pub error: Option<&'a ErrorBody>,
    pub llm: &'a LlmSection,
    pub timing: &'a Timing,
}

pub enum BodySrc<'a> {
    Body(&'a Body),
    /// A body fragment copied from a cached result file.
    Raw(&'a [u8]),
}

/// Writes `{fields}` of a serialized struct without the surrounding braces.
struct Fragment<'w, W: Write> {
    inner: &'w mut W,
    first: bool,
    held: Option<u8>,
}

impl<W: Write> Write for Fragment<'_, W> {
    fn write(&mut self, mut buf: &[u8]) -> io::Result<usize> {
        let n = buf.len();
        if self.first && !buf.is_empty() {
            self.first = false;
            buf = &buf[1..];
        }
        if buf.is_empty() {
            return Ok(n);
        }
        if let Some(h) = self.held.take() {
            self.inner.write_all(&[h])?;
        }
        self.inner.write_all(&buf[..buf.len() - 1])?;
        self.held = Some(buf[buf.len() - 1]);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn write_fields<W: Write>(w: &mut W, v: &impl Serialize) -> io::Result<()> {
    let mut f = Fragment { inner: w, first: true, held: None };
    serde_json::to_writer(&mut f, v).map_err(io::Error::other)
}

/// Counts bytes and enforces `DOCVISION_MAX_RESULT_BYTES`.
pub struct Counting<W> {
    pub inner: W,
    pub n: u64,
    pub limit: u64,
}

impl<W: Write> Write for Counting<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.n += buf.len() as u64;
        if self.n > self.limit {
            return Err(io::Error::new(io::ErrorKind::FileTooLarge, "result exceeds DOCVISION_MAX_RESULT_BYTES"));
        }
        self.inner.write_all(buf)?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Serialize a full result; returns the body byte range within the output.
pub fn write_result<W: Write>(w: W, limit: u64, h: &Header, body: BodySrc, t: &Trailer) -> io::Result<(u64, u64, W)> {
    let mut c = Counting { inner: w, n: 0, limit };
    c.write_all(b"{")?;
    write_fields(&mut c, h)?;
    c.write_all(b",")?;
    let start = c.n;
    match body {
        BodySrc::Body(b) => write_fields(&mut c, b)?,
        BodySrc::Raw(r) => c.write_all(r)?,
    }
    let end = c.n;
    c.write_all(b",")?;
    write_fields(&mut c, t)?;
    c.write_all(b"}")?;
    c.flush()?;
    Ok((start, end, c.inner))
}

pub fn result_bytes(limit: u64, h: &Header, body: BodySrc, t: &Trailer) -> Result<Vec<u8>, AppError> {
    write_result(Vec::new(), limit, h, body, t).map(|(_, _, v)| v).map_err(map_io)
}

pub fn map_io(e: io::Error) -> AppError {
    if e.kind() == io::ErrorKind::FileTooLarge {
        AppError::new(413, "result_too_large", e.to_string()).stage("storage")
    } else {
        AppError::internal(format!("result serialization failed: {}", e.kind())).stage("storage")
    }
}

/// Size of the body fragment when serialized (used to compute self-referential file sizes).
pub fn body_len(body: BodySrc) -> io::Result<u64> {
    match body {
        BodySrc::Raw(r) => Ok(r.len() as u64),
        BodySrc::Body(b) => {
            let mut c = Counting { inner: io::sink(), n: 0, limit: u64::MAX };
            write_fields(&mut c, b)?;
            Ok(c.n)
        }
    }
}

/// Write a result file atomically (temp + rename). Blocking: call on the CPU lane.
pub fn write_file_atomic(path: &Path, limit: u64, h: &Header, body: BodySrc, t: &Trailer) -> io::Result<(u64, u64, u64)> {
    let tmp = path.with_extension("tmp");
    let f = std::fs::File::create(&tmp)?;
    let res = write_result(io::BufWriter::with_capacity(64 * 1024, f), limit, h, body, t).and_then(|(s, e, w)| {
        let f = w.into_inner().map_err(|e| e.into_error())?;
        f.sync_data()?;
        Ok((s, e, f.metadata()?.len()))
    });
    match res {
        Ok(r) => {
            std::fs::rename(&tmp, path)?;
            Ok(r)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

// ---------------------------------------------------------------- destinations

#[derive(Debug, Clone)]
pub enum Dest {
    Local(PathBuf),
    #[cfg_attr(not(feature = "s3"), allow(dead_code))]
    S3(crate::source::S3Loc, String),
}

pub fn parse_dest(s: &str) -> Result<Dest, AppError> {
    let bad = |m: &str| AppError::bad_request("invalid_destination", m.to_string());
    let key_or_path = if s.starts_with("s3://") { s.split('?').next().unwrap_or(s) } else { s };
    if !key_or_path.ends_with(".md") || key_or_path.ends_with("/.md") {
        return Err(bad("destination must name a .md file (the JSON result is written next to it as <name>.docv.json)"));
    }
    if s.starts_with('/') {
        return Ok(Dest::Local(PathBuf::from(s)));
    }
    if let Some(p) = s.strip_prefix("file://") {
        return Ok(Dest::Local(PathBuf::from(p)));
    }
    if s.starts_with("s3://") {
        if !cfg!(feature = "s3") {
            return Err(AppError::feature_not_compiled("s3"));
        }
        return crate::source::parse_s3(s).map(|l| Dest::S3(l, crate::source::sanitize(s))).map_err(|e| bad(&e));
    }
    Err(bad("destination must be an absolute path, file:/// URL or s3:// location"))
}

/// `report.md` -> `report.docv.json`: where the full JSON result is written.
pub fn json_sibling(md: &str) -> String {
    format!("{}.docv.json", md.strip_suffix(".md").unwrap_or(md))
}

impl Dest {
    pub fn display(&self) -> String {
        match self {
            Dest::Local(p) => p.display().to_string(),
            Dest::S3(_, s) => s.clone(),
        }
    }
    pub fn json_display(&self) -> String {
        match self {
            Dest::Local(p) => json_sibling(&p.display().to_string()),
            Dest::S3(_, s) => {
                let (base, q) = s.split_once('?').map(|(a, b)| (a, Some(b))).unwrap_or((s, None));
                let j = json_sibling(base);
                q.map(|q| format!("{j}?{q}")).unwrap_or(j)
            }
        }
    }
}

fn exists_err(what: &str) -> AppError {
    AppError::new(409, "destination_exists", format!("{what} already exists; set options.overwrite=true to replace it")).stage("storage")
}

fn local_put(src: Option<&Path>, bytes: Option<&[u8]>, dest: &Path, overwrite: bool) -> Result<(), AppError> {
    let io =
        |e: io::Error| AppError::new(502, "destination_write_failed", format!("cannot write destination: {}", e.kind())).stage("storage");
    let dir = dest.parent().ok_or_else(|| AppError::bad_request("invalid_destination", "destination has no parent directory"))?;
    let real_dir = std::fs::canonicalize(dir).map_err(io)?;
    if crate::source::forbidden_path(dest) || crate::source::forbidden_path(&real_dir) {
        return Err(AppError::bad_request("invalid_destination", "this location can't be used as a destination"));
    }
    let tmp = dir.join(format!(".docvision-{}.tmp", uuid::Uuid::now_v7()));
    match (src, bytes) {
        (Some(s), _) => std::fs::copy(s, &tmp).map(|_| ()).map_err(io)?,
        (_, Some(b)) => std::fs::write(&tmp, b).map_err(io)?,
        _ => return Ok(()),
    }
    let res = if overwrite {
        std::fs::rename(&tmp, dest).map_err(io)
    } else {
        // hard_link fails if the destination exists: atomic create-if-absent.
        match std::fs::hard_link(&tmp, dest) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(exists_err(&dest.display().to_string())),
            Err(e) => Err(io(e)),
        }
    };
    let _ = std::fs::remove_file(&tmp);
    res
}

#[cfg(feature = "s3")]
pub fn s3_store(loc: &crate::source::S3Loc) -> Result<(object_store::aws::AmazonS3, object_store::path::Path), String> {
    use object_store::aws::AmazonS3Builder;
    // DSN credentials apply only to this store; never to process-global AWS settings.
    let mut b = match (&loc.access_key, &loc.secret_key) {
        (Some(a), Some(s)) => {
            let mut b = AmazonS3Builder::new().with_access_key_id(a.expose()).with_secret_access_key(s.expose());
            if let Some(t) = &loc.session_token {
                b = b.with_token(t.expose());
            }
            b
        }
        _ => AmazonS3Builder::from_env(),
    };
    b = b.with_bucket_name(&loc.bucket);
    match (&loc.region, &loc.access_key) {
        (Some(r), _) => b = b.with_region(r),
        // DSN without a region: us-east-1. Plain s3:// keeps the region from the environment.
        (None, Some(_)) => b = b.with_region("us-east-1"),
        (None, None) => {}
    }
    if let Ok(ep) = std::env::var("DOCVISION_S3_ENDPOINT") {
        b = b.with_endpoint(ep).with_allow_http(true).with_virtual_hosted_style_request(false);
    }
    let store = b.build().map_err(|e| format!("invalid S3 configuration: {e}"))?;
    Ok((store, object_store::path::Path::from(loc.key.as_str())))
}

#[cfg(feature = "s3")]
async fn s3_put(loc: &crate::source::S3Loc, key: &str, data: bytes::Bytes, overwrite: bool, shown: &str) -> Result<(), AppError> {
    use object_store::{ObjectStore, PutMode, PutOptions};
    let mut l = loc.clone();
    l.key = key.to_string();
    let (store, path) = s3_store(&l).map_err(|e| AppError::bad_request("invalid_destination", e))?;
    let mode = if overwrite { PutMode::Overwrite } else { PutMode::Create };
    match store.put_opts(&path, data.into(), PutOptions { mode, ..Default::default() }).await {
        Ok(_) => Ok(()),
        Err(object_store::Error::AlreadyExists { .. }) | Err(object_store::Error::Precondition { .. }) => Err(exists_err(shown)),
        Err(_) => Err(AppError::new(502, "destination_write_failed", format!("cannot write {shown}")).stage("storage")),
    }
}

/// Publish the Markdown to the destination and the JSON result next to it. Both are written
/// before the caller marks completion; any failure is returned, never hidden.
pub async fn publish(dest: &Dest, result_file: &Path, content: &str, overwrite: bool) -> Result<(), AppError> {
    match dest {
        Dest::Local(p) => {
            let (p, src, md) = (p.clone(), result_file.to_path_buf(), content.as_bytes().to_vec());
            tokio::task::spawn_blocking(move || {
                local_put(Some(&src), None, Path::new(&json_sibling(&p.display().to_string())), overwrite)?;
                local_put(None, Some(&md), &p, overwrite)
            })
            .await
            .map_err(|_| AppError::internal("publish task failed"))?
        }
        #[cfg(feature = "s3")]
        Dest::S3(loc, shown) => {
            let data = tokio::fs::read(result_file).await.map_err(|_| AppError::internal("cannot read result file"))?;
            s3_put(loc, &json_sibling(&loc.key), data.into(), overwrite, &dest.json_display()).await?;
            s3_put(loc, &loc.key, bytes::Bytes::copy_from_slice(content.as_bytes()), overwrite, shown).await
        }
        #[cfg(not(feature = "s3"))]
        Dest::S3(..) => Err(AppError::feature_not_compiled("s3")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragments_compose_valid_json() {
        let body =
            Body { content: "# Hi\n\ntext\n".into(), chunks: crate::markdown::analyze("# Hi\n\ntext\n", 100, &[]).1, ..Default::default() };
        let llm = LlmSection::default();
        let timing = Timing::default();
        let h = Header {
            schema_version: 1,
            request_id: "r",
            job_id: Some("j"),
            status: "completed",
            cache_hit: false,
            src_file: "/a.md",
            dest_file: None,
            files: &[],
            metadata: None,
        };
        let t = Trailer { error: None, llm: &llm, timing: &timing };
        let (s, e, out) = write_result(Vec::new(), u64::MAX, &h, BodySrc::Body(&body), &t).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["content"], "# Hi\n\ntext\n");
        assert_eq!(v["chunks"][0]["headings"][0], "Hi");
        assert_eq!(v["job_id"], "j");
        assert_eq!(body_len(BodySrc::Body(&body)).unwrap(), e - s);
        // Reuse the body bytes with a different header (cache hit).
        let raw = out[s as usize..e as usize].to_vec();
        let h2 = Header { request_id: "r2", cache_hit: true, ..h };
        let (_, _, out2) = write_result(Vec::new(), u64::MAX, &h2, BodySrc::Raw(&raw), &t).unwrap();
        let v2: serde_json::Value = serde_json::from_slice(&out2).unwrap();
        assert_eq!(v2["request_id"], "r2");
        assert_eq!(v2["content"], v["content"]);
        assert!(write_result(Vec::new(), 10, &h, BodySrc::Body(&body), &t).is_err());
    }

    #[test]
    fn siblings() {
        assert_eq!(json_sibling("s3/report.md"), "s3/report.docv.json");
        assert_eq!(json_sibling("/x/a.md"), "/x/a.docv.json");
        #[cfg(feature = "s3")]
        {
            let d = parse_dest("s3://b/k/report.md?region=eu-west-1").unwrap();
            assert_eq!(d.json_display(), "s3://b/k/report.docv.json?region=eu-west-1");
        }
        assert!(parse_dest("/x/a.txt").is_err());
        assert!(parse_dest("/x/a.json").is_err());
    }
}
