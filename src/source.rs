//! Source references: local paths, HTTPS URLs, S3 URIs and credential-bearing S3 DSNs.
//! Every fetch streams to a staging file while hashing with blake3; nothing is buffered whole.

use crate::config::Secret;
use crate::rpc::AppError;
use futures_util::StreamExt;
use serde::Serialize;
use std::io::Read;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    Markdown,
    Text,
    Html,
    Docx,
    Xlsx,
    Pptx,
    Odt,
    Ods,
    Odp,
    Epub,
    Pdf,
    Png,
    Jpeg,
    Webp,
}

impl Format {
    pub fn name(self) -> &'static str {
        match self {
            Format::Markdown => "markdown",
            Format::Text => "text",
            Format::Html => "html",
            Format::Docx => "docx",
            Format::Xlsx => "xlsx",
            Format::Pptx => "pptx",
            Format::Odt => "odt",
            Format::Ods => "ods",
            Format::Odp => "odp",
            Format::Epub => "epub",
            Format::Pdf => "pdf",
            Format::Png => "png",
            Format::Jpeg => "jpeg",
            Format::Webp => "webp",
        }
    }
    pub fn is_image(self) -> bool {
        matches!(self, Format::Png | Format::Jpeg | Format::Webp)
    }
    pub fn is_text(self) -> bool {
        matches!(self, Format::Markdown | Format::Text)
    }
    pub fn mime(self) -> &'static str {
        match self {
            Format::Pdf => "application/pdf",
            Format::Png => "image/png",
            Format::Jpeg => "image/jpeg",
            Format::Webp => "image/webp",
            _ => "application/octet-stream",
        }
    }
}

#[derive(Debug, Clone)]
pub struct S3Loc {
    pub bucket: String,
    pub key: String,
    /// `?region=` from the URL. When absent: DSNs use `us-east-1`; plain `s3://` uses
    /// the server's `AWS_REGION` / `AWS_DEFAULT_REGION` (then `us-east-1`).
    pub region: Option<String>,
    pub access_key: Option<Secret<String>>,
    pub secret_key: Option<Secret<String>>,
    pub session_token: Option<Secret<String>>,
}

#[derive(Debug, Clone)]
pub enum Source {
    Local(PathBuf),
    Https(url::Url),
    S3(S3Loc),
}

fn decode(s: &str) -> Result<String, String> {
    percent_encoding::percent_decode_str(s).decode_utf8().map(|c| c.into_owned()).map_err(|_| "invalid percent-encoding".to_string())
}

/// Parse `s3://bucket/key` or `s3://access:secret@bucket/key?region=..&session_token=..`.
pub fn parse_s3(s: &str) -> Result<S3Loc, String> {
    // Reserved characters in credentials must be percent-encoded; a raw '/', '?' or '#'
    // before the '@' would otherwise be ambiguous.
    let rest = s.strip_prefix("s3://").ok_or("S3 location must start with s3://")?;
    if let Some(at) = rest.find('@') {
        let creds = &rest[..at];
        if creds.contains(['/', '?', '#']) || creds.matches(':').count() != 1 {
            return Err("S3 DSN credentials must be `access-key:secret-key` with reserved characters percent-encoded".into());
        }
    }
    let u = url::Url::parse(s).map_err(|e| format!("invalid S3 location: {e}"))?;
    let bucket = u.host_str().filter(|b| !b.is_empty()).ok_or("S3 location is missing a bucket")?.to_string();
    let key = decode(u.path().trim_start_matches('/'))?;
    if key.is_empty() {
        return Err("S3 location is missing an object key".into());
    }
    let mut region = None;
    let mut session_token = None;
    for (k, v) in u.query_pairs() {
        match k.as_ref() {
            "region" => region = Some(v.into_owned()),
            "session_token" => session_token = Some(Secret(v.into_owned())),
            other => return Err(format!("unknown S3 parameter `{other}`")),
        }
    }
    let (access_key, secret_key) = if u.username().is_empty() {
        if u.password().is_some() {
            return Err("S3 DSN is missing the access key".into());
        }
        (None, None)
    } else {
        let secret = u.password().ok_or("S3 DSN is missing the secret key")?;
        (Some(Secret(decode(u.username())?)), Some(Secret(decode(secret)?)))
    };
    Ok(S3Loc { bucket, key, region, access_key, secret_key, session_token })
}

/// Web locations (sources, webhooks): `https://`, or `http://` to a loopback host for local development.
pub fn web_url_allowed(u: &url::Url) -> bool {
    match (u.scheme(), u.host()) {
        ("https", _) => true,
        ("http", Some(url::Host::Domain(d))) => d.eq_ignore_ascii_case("localhost"),
        ("http", Some(url::Host::Ipv4(ip))) => ip.is_loopback(),
        ("http", Some(url::Host::Ipv6(ip))) => ip.is_loopback(),
        _ => false,
    }
}

pub fn parse(s: &str) -> Result<Source, AppError> {
    let bad = |m: String| AppError::bad_request("invalid_source", m);
    if s.starts_with("data:") {
        return Err(AppError::new(415, "upload_not_supported", "data: URLs are not accepted; reference a path, HTTPS URL or S3 object"));
    }
    if s.starts_with('/') {
        return Ok(Source::Local(PathBuf::from(s)));
    }
    if let Some(p) = s.strip_prefix("file://") {
        if !p.starts_with('/') {
            return Err(bad("file:// URLs must be absolute (file:///path)".into()));
        }
        return Ok(Source::Local(PathBuf::from(decode(p).map_err(bad)?)));
    }
    if s.starts_with("https://") || s.starts_with("http://") {
        let u = url::Url::parse(s).map_err(|e| bad(format!("invalid URL: {e}")))?;
        if !web_url_allowed(&u) {
            return Err(bad("source URLs must be https:// (http:// only for localhost)".into()));
        }
        return Ok(Source::Https(u));
    }
    if s.starts_with("s3://") {
        if !cfg!(feature = "s3") {
            return Err(AppError::feature_not_compiled("s3"));
        }
        return parse_s3(s).map(Source::S3).map_err(bad);
    }
    Err(bad("source must be an absolute path, file:/// URL, https:// URL or s3:// location".into()))
}

/// Location safe for logs and responses: no credentials, HTTPS query strings masked.
pub fn sanitize(s: &str) -> String {
    if let Ok(mut u) = url::Url::parse(s) {
        match u.scheme() {
            "s3" => {
                let _ = u.set_username("");
                let _ = u.set_password(None);
                let region = u.query_pairs().find(|(k, _)| k == "region").map(|(_, v)| v.into_owned());
                u.set_query(None);
                if let Some(r) = region {
                    u.query_pairs_mut().append_pair("region", &r);
                }
                return u.to_string();
            }
            "http" | "https" => {
                let _ = u.set_username("");
                let _ = u.set_password(None);
                if u.query().is_some() {
                    u.set_query(Some("***"));
                }
                u.set_fragment(None);
                return u.to_string();
            }
            _ => {}
        }
    }
    s.to_string()
}

pub fn file_name(s: &str) -> String {
    let path = url::Url::parse(s).map(|u| u.path().to_string()).unwrap_or_else(|_| s.to_string());
    let name = path.rsplit('/').next().unwrap_or("");
    decode(name).unwrap_or_else(|_| name.to_string())
}

/// A source staged on local disk. Service-owned staging files are removed on drop.
#[derive(Debug)]
pub struct Staged {
    pub path: PathBuf,
    pub size: u64,
    pub hash: blake3::Hash,
    pub owned: bool,
}

impl Drop for Staged {
    fn drop(&mut self) {
        if self.owned {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Use only the bundled webpki roots (identical on every platform, no OS trust store needed).
/// The native-roots toggle exists only when `object_store` (s3) enables reqwest's native roots.
pub fn webpki_only(b: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    #[cfg(feature = "s3")]
    {
        b.tls_built_in_native_certs(false)
    }
    #[cfg(not(feature = "s3"))]
    {
        b
    }
}

/// Client for sources and webhooks, built on first use (TLS root loading is not paid at startup).
pub fn http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| build_http_client(std::time::Duration::from_secs(10)))
}

fn build_http_client(connect_timeout: std::time::Duration) -> reqwest::Client {
    webpki_only(reqwest::Client::builder())
        .use_rustls_tls()
        .connect_timeout(connect_timeout)
        .read_timeout(std::time::Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::custom(|a| {
            if a.previous().len() >= 5 {
                a.error("too many redirects")
            } else if !web_url_allowed(a.url()) {
                a.error("redirect left HTTPS")
            } else {
                a.follow()
            }
        }))
        .build()
        .expect("http client")
}

/// Best-effort size before fetching, for memory admission.
pub async fn size_hint(src: &Source) -> Option<u64> {
    match src {
        Source::Local(p) => tokio::fs::metadata(p).await.ok().map(|m| m.len()),
        Source::Https(_) => None,
        #[cfg(feature = "s3")]
        Source::S3(loc) => {
            let (store, path) = crate::result::s3_store(loc).ok()?;
            use object_store::ObjectStore;
            store.head(&path).await.ok().map(|m| m.size as u64)
        }
        #[cfg(not(feature = "s3"))]
        Source::S3(_) => None,
    }
}

pub struct FetchCtx<'a> {
    pub http: &'a reqwest::Client,
    pub staging: &'a Path,
    pub max_bytes: u64,
}

fn too_large(max: u64) -> AppError {
    AppError::new(413, "input_too_large", format!("source exceeds DOCVISION_MAX_INPUT_BYTES ({max} bytes)")).stage("fetch")
}

/// Pseudo-filesystems expose the process environment (secrets), kernel settings and devices:
/// never read or write there. Checked on the real path, after symlinks are resolved.
pub fn forbidden_path(p: &Path) -> bool {
    ["/proc", "/sys", "/dev"].iter().any(|x| p.starts_with(x))
}

pub async fn fetch(ctx: &FetchCtx<'_>, src: &Source) -> Result<Staged, AppError> {
    match src {
        Source::Local(p) => {
            let real = tokio::fs::canonicalize(p)
                .await
                .map_err(|e| AppError::new(404, "source_not_found", format!("cannot read source: {}", e.kind())).stage("fetch"))?;
            if forbidden_path(p) || forbidden_path(&real) {
                return Err(AppError::bad_request("invalid_source", "this location can't be used as a source").stage("fetch"));
            }
            let p = &real;
            let meta = tokio::fs::metadata(p)
                .await
                .map_err(|e| AppError::new(404, "source_not_found", format!("cannot read source: {}", e.kind())).stage("fetch"))?;
            if !meta.is_file() {
                return Err(AppError::bad_request("invalid_source", "source is not a regular file").stage("fetch"));
            }
            if meta.len() > ctx.max_bytes {
                return Err(too_large(ctx.max_bytes));
            }
            let path = p.clone();
            let hash = tokio::task::spawn_blocking(move || -> std::io::Result<blake3::Hash> {
                let mut h = blake3::Hasher::new();
                let mut f = std::fs::File::open(&path)?;
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    let n = f.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    h.update(&buf[..n]);
                }
                Ok(h.finalize())
            })
            .await
            .map_err(|_| AppError::internal("hash task failed"))?
            .map_err(|e| AppError::new(502, "source_read_failed", format!("cannot read source: {}", e.kind())).stage("fetch"))?;
            Ok(Staged { path: p.clone(), size: meta.len(), hash, owned: false })
        }
        Source::Https(u) => {
            let resp = ctx.http.get(u.clone()).send().await.map_err(|e| upstream_err(&e))?;
            let status = resp.status();
            if !status.is_success() {
                let code = if status.as_u16() == 404 { 404 } else { 502 };
                return Err(AppError::new(code, "source_fetch_failed", format!("source returned HTTP {}", status.as_u16())).stage("fetch"));
            }
            if resp.content_length().is_some_and(|l| l > ctx.max_bytes) {
                return Err(too_large(ctx.max_bytes));
            }
            stream_to_staging(ctx, resp.bytes_stream().map(|r| r.map_err(|e| upstream_err(&e)))).await
        }
        #[cfg(feature = "s3")]
        Source::S3(loc) => {
            use object_store::ObjectStore;
            let (store, path) = crate::result::s3_store(loc).map_err(|e| AppError::bad_request("invalid_source", e))?;
            let got = store.get(&path).await.map_err(|e| s3_err(&e))?;
            if got.meta.size as u64 > ctx.max_bytes {
                return Err(too_large(ctx.max_bytes));
            }
            stream_to_staging(ctx, got.into_stream().map(|r| r.map_err(|e| s3_err(&e)))).await
        }
        #[cfg(not(feature = "s3"))]
        Source::S3(_) => Err(AppError::feature_not_compiled("s3")),
    }
}

#[cfg(feature = "s3")]
fn s3_err(e: &object_store::Error) -> AppError {
    match e {
        object_store::Error::NotFound { .. } => AppError::new(404, "source_not_found", "S3 object not found").stage("fetch"),
        _ => AppError::new(502, "source_fetch_failed", "S3 request failed").stage("fetch"),
    }
}

fn upstream_err(e: &reqwest::Error) -> AppError {
    if e.is_timeout() {
        AppError::new(504, "source_timeout", "source fetch timed out").stage("fetch")
    } else {
        AppError::new(502, "source_fetch_failed", "source fetch failed").stage("fetch")
    }
}

async fn stream_to_staging<S>(ctx: &FetchCtx<'_>, mut stream: S) -> Result<Staged, AppError>
where
    S: futures_util::Stream<Item = Result<bytes::Bytes, AppError>> + Unpin,
{
    let path = ctx.staging.join(uuid::Uuid::now_v7().to_string());
    let mut file = tokio::fs::File::create(&path).await.map_err(|_| AppError::internal("cannot create staging file"))?;
    // From here on the staging file is owned and removed on any error.
    let mut staged = Staged { path, size: 0, hash: blake3::Hash::from_bytes([0; 32]), owned: true };
    let mut hasher = blake3::Hasher::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        staged.size += chunk.len() as u64;
        if staged.size > ctx.max_bytes {
            return Err(too_large(ctx.max_bytes));
        }
        hasher.update(&chunk);
        file.write_all(&chunk).await.map_err(|_| AppError::internal("staging write failed"))?;
    }
    file.flush().await.map_err(|_| AppError::internal("staging write failed"))?;
    staged.hash = hasher.finalize();
    Ok(staged)
}

const CFB: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];

/// Sniff the format from signatures and package manifests (never the extension, except to
/// distinguish Markdown from plain text, which have no signature).
pub fn sniff(path: &Path, name_hint: &str) -> Result<Format, AppError> {
    let mut f = std::fs::File::open(path).map_err(|_| AppError::internal("cannot open staged file"))?;
    let mut head = [0u8; 512];
    let n = read_up_to(&mut f, &mut head);
    sniff_bytes(
        &head[..n],
        name_hint,
        || {
            let file = std::fs::File::open(path).ok()?;
            let zip = zip::ZipArchive::new(std::io::BufReader::new(file)).ok()?;
            Some(zip.file_names().map(str::to_string).collect::<Vec<_>>())
        },
        || {
            let file = std::fs::File::open(path).ok()?;
            let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file)).ok()?;
            let mut m = String::new();
            zip.by_name("mimetype").ok()?.take(128).read_to_string(&mut m).ok()?;
            Some(m)
        },
    )
}

fn read_up_to(r: &mut impl Read, buf: &mut [u8]) -> usize {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) | Err(_) => break,
            Ok(k) => n += k,
        }
    }
    n
}

pub fn sniff_bytes(
    head: &[u8],
    name_hint: &str,
    zip_names: impl FnOnce() -> Option<Vec<String>>,
    zip_mimetype: impl FnOnce() -> Option<String>,
) -> Result<Format, AppError> {
    let unsupported = |m: &str| AppError::new(415, "unsupported_format", m.to_string()).stage("sniff");
    if head.starts_with(&CFB) {
        return Err(unsupported(
            "legacy binary Office formats (.doc, .xls, .ppt and other OLE/CFB files) are not supported; convert to the modern equivalent (.docx, .xlsx, .pptx)",
        ));
    }
    if head.starts_with(b"%PDF-") {
        return Ok(Format::Pdf);
    }
    if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Ok(Format::Png);
    }
    if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Ok(Format::Jpeg);
    }
    if head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        return Ok(Format::Webp);
    }
    if head.starts_with(b"PK\x03\x04") {
        let names = zip_names().ok_or_else(|| AppError::new(422, "corrupt_document", "unreadable ZIP container").stage("sniff"))?;
        let has = |n: &str| names.iter().any(|x| x == n);
        if has("word/document.xml") {
            return Ok(Format::Docx);
        }
        if has("xl/workbook.xml") {
            return Ok(Format::Xlsx);
        }
        if has("ppt/presentation.xml") {
            return Ok(Format::Pptx);
        }
        if has("mimetype") {
            let m = zip_mimetype().unwrap_or_default();
            let f = match m.trim() {
                "application/vnd.oasis.opendocument.text" => Some(Format::Odt),
                "application/vnd.oasis.opendocument.spreadsheet" => Some(Format::Ods),
                "application/vnd.oasis.opendocument.presentation" => Some(Format::Odp),
                "application/epub+zip" => Some(Format::Epub),
                _ => None,
            };
            if let Some(f) = f {
                if !cfg!(feature = "odf-epub") {
                    return Err(AppError::feature_not_compiled("odf-epub"));
                }
                return Ok(f);
            }
        }
        return Err(unsupported("unsupported ZIP-based format"));
    }
    // Text: must be UTF-8 (allowing a truncated trailing sequence in the sniff window).
    let text = match std::str::from_utf8(head) {
        Ok(t) => t,
        Err(e) if e.error_len().is_none() => std::str::from_utf8(&head[..e.valid_up_to()]).unwrap_or(""),
        Err(_) => return Err(unsupported("unsupported binary format")),
    };
    if head.contains(&0) {
        return Err(unsupported("unsupported binary format"));
    }
    let t = text.trim_start_matches('\u{feff}').trim_start().to_ascii_lowercase();
    if t.starts_with("<!doctype html") || t.starts_with("<html") || (t.starts_with('<') && (t.contains("<body") || t.contains("<head"))) {
        return Ok(Format::Html);
    }
    if t.starts_with("<?xml") && t.contains("<html") {
        return Ok(Format::Html);
    }
    let lower = name_hint.to_ascii_lowercase();
    if lower.ends_with(".md") || lower.ends_with(".markdown") {
        return Ok(Format::Markdown);
    }
    if lower.ends_with(".html") || lower.ends_with(".htm") {
        return Ok(Format::Html);
    }
    Ok(Format::Text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dsn_parsing() {
        let l = parse_s3("s3://AKIA:se%2Fcr%40et@bucket/dir/my%20file.pdf?region=eu-west-1&session_token=tok").unwrap();
        assert_eq!(l.bucket, "bucket");
        assert_eq!(l.key, "dir/my file.pdf");
        assert_eq!(l.region.as_deref(), Some("eu-west-1"));
        assert_eq!(l.access_key.unwrap().0, "AKIA");
        assert_eq!(l.secret_key.unwrap().0, "se/cr@et");
        assert_eq!(l.session_token.unwrap().0, "tok");
        let l = parse_s3("s3://bucket/key.pdf").unwrap();
        assert_eq!(l.region, None);
        assert!(l.access_key.is_none());
        assert!(parse_s3("s3://ak:se/cret@bucket/key").is_err());
        assert!(parse_s3("s3://bucket/").is_err());
        assert!(parse_s3("s3://bucket/k?foo=1").is_err());
    }

    #[test]
    fn sanitizes() {
        assert_eq!(sanitize("s3://ak:sk@bucket/k.pdf?region=eu-west-1&session_token=x"), "s3://bucket/k.pdf?region=eu-west-1");
        assert_eq!(sanitize("https://h.com/a.pdf?X-Amz-Signature=abc"), "https://h.com/a.pdf?***");
        assert_eq!(sanitize("/docs/a.pdf"), "/docs/a.pdf");
    }

    fn sb(head: &[u8], name: &str) -> Result<Format, AppError> {
        sniff_bytes(head, name, || None, || None)
    }

    #[test]
    fn sniffs() {
        assert_eq!(sb(b"%PDF-1.7", "x").unwrap(), Format::Pdf);
        assert_eq!(sb(&CFB, "x.doc").unwrap_err().status, 415);
        assert_eq!(sb(b"<!DOCTYPE html><p>", "x").unwrap(), Format::Html);
        assert_eq!(sb(b"# hi", "a.md").unwrap(), Format::Markdown);
        assert_eq!(sb(b"hi", "a.pdf").unwrap(), Format::Text);
        assert_eq!(sb(b"\x00\x01\x02", "a").unwrap_err().status, 415);
        assert_eq!(sb(b"\x89PNG\r\n\x1a\nxxxx", "a").unwrap(), Format::Png);
        let names = || Some(vec!["[Content_Types].xml".to_string(), "word/document.xml".to_string()]);
        assert_eq!(sniff_bytes(b"PK\x03\x04rest", "a", names, || None).unwrap(), Format::Docx);
    }

    #[test]
    fn parses_sources() {
        assert!(matches!(parse("/a/b.pdf").unwrap(), Source::Local(_)));
        assert!(matches!(parse("file:///a/b%20c.pdf").unwrap(), Source::Local(p) if p == Path::new("/a/b c.pdf")));
        assert!(matches!(parse("https://x.com/a").unwrap(), Source::Https(_)));
        assert_eq!(parse("http://x.com/a").unwrap_err().status, 400);
        assert!(matches!(parse("http://127.0.0.1:8080/a").unwrap(), Source::Https(_)));
        assert!(matches!(parse("http://localhost/a").unwrap(), Source::Https(_)));
        assert_eq!(parse("data:application/pdf;base64,AAA").unwrap_err().status, 415);
        assert_eq!(parse("relative/path").unwrap_err().status, 400);
        assert!(forbidden_path(Path::new("/proc/self/environ")) && forbidden_path(Path::new("/dev/fd/0")));
        assert!(!forbidden_path(Path::new("/documents/procedures.pdf")));
    }
}
