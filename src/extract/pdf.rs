//! PDF: exact local page counting (core, no PDF library) and native per-page text
//! extraction with quality judgement (`pdf-native`).

use std::collections::HashMap;
use std::io::Read;

fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\n' | b'\r' | b'\t' | b'\x0c' | 0)
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && is_ws(b[i]) {
        i += 1;
    }
    i
}

fn read_uint(b: &[u8], i: usize) -> Option<(u64, usize)> {
    let mut j = i;
    while j < b.len() && b[j].is_ascii_digit() {
        j += 1;
    }
    if j == i || j - i > 12 {
        return None;
    }
    std::str::from_utf8(&b[i..j]).ok()?.parse().ok().map(|n| (n, j))
}

/// After `key`, parse `N G R`; returns N.
fn ref_after(b: &[u8], at: usize) -> Option<u64> {
    let (n, i) = read_uint(b, skip_ws(b, at))?;
    let (_, i) = read_uint(b, skip_ws(b, i))?;
    let i = skip_ws(b, i);
    (b.get(i) == Some(&b'R')).then_some(n)
}

fn find_all<'a>(hay: &'a [u8], needle: &'a [u8]) -> impl Iterator<Item = usize> + 'a {
    let mut start = 0;
    std::iter::from_fn(move || {
        if needle.is_empty() || start >= hay.len() {
            return None;
        }
        let pos = hay[start..].windows(needle.len()).position(|w| w == needle)? + start;
        start = pos + 1;
        Some(pos)
    })
}

/// First `key` (as a whole name) followed by an indirect reference.
fn find_ref(b: &[u8], key: &[u8]) -> Option<u64> {
    find_all(b, key).filter(|&p| b.get(p + key.len()).is_none_or(|c| !c.is_ascii_alphanumeric())).find_map(|p| ref_after(b, p + key.len()))
}

fn find_last_ref(b: &[u8], key: &[u8]) -> Option<u64> {
    find_all(b, key)
        .filter(|&p| b.get(p + key.len()).is_none_or(|c| !c.is_ascii_alphanumeric()))
        .filter_map(|p| ref_after(b, p + key.len()))
        .last()
}

fn find_int(b: &[u8], key: &[u8]) -> Option<u64> {
    find_all(b, key)
        .filter(|&p| b.get(p + key.len()).is_none_or(|c| !c.is_ascii_alphanumeric()))
        .find_map(|p| read_uint(b, skip_ws(b, p + key.len())).map(|(n, _)| n))
}

/// Index of `N G obj` -> body slice (up to the next `endobj`). Later definitions win.
fn index_objects(b: &[u8]) -> HashMap<u64, (usize, usize)> {
    let mut map = HashMap::new();
    for p in find_all(b, b"obj") {
        // Must be `<num> <gen> obj` and not `endobj`.
        if p >= 3 && &b[p - 3..p] == b"end" {
            continue;
        }
        let mut i = p;
        while i > 0 && is_ws(b[i - 1]) {
            i -= 1;
        }
        let gen_end = i;
        while i > 0 && b[i - 1].is_ascii_digit() {
            i -= 1;
        }
        if i == gen_end {
            continue;
        }
        let gen_start = i;
        while i > 0 && is_ws(b[i - 1]) {
            i -= 1;
        }
        let num_end = i;
        while i > 0 && b[i - 1].is_ascii_digit() {
            i -= 1;
        }
        if i == num_end || num_end == gen_start {
            continue;
        }
        let Some((num, _)) = read_uint(b, i) else { continue };
        let body_start = p + 3;
        let body_end = find_all(&b[body_start..], b"endobj").next().map(|e| body_start + e).unwrap_or(b.len());
        map.insert(num, (body_start, body_end));
    }
    map
}

/// Decompress `/Type /ObjStm` streams and index their objects.
fn object_streams(b: &[u8], objs: &HashMap<u64, (usize, usize)>) -> HashMap<u64, Vec<u8>> {
    let mut out = HashMap::new();
    for &(s, e) in objs.values() {
        let body = &b[s..e];
        let Some(dict_end) = find_all(body, b"stream").next() else { continue };
        let dict = &body[..dict_end];
        let is_objstm = find_all(dict, b"/ObjStm").next().is_some();
        if !is_objstm {
            continue;
        }
        let (Some(n), Some(first)) = (find_int(dict, b"/N"), find_int(dict, b"/First")) else { continue };
        let mut ds = dict_end + 6;
        if body.get(ds) == Some(&b'\r') {
            ds += 1;
        }
        if body.get(ds) == Some(&b'\n') {
            ds += 1;
        }
        let de = find_all(&body[ds..], b"endstream").next().map(|x| ds + x).unwrap_or(body.len());
        let raw = &body[ds..de];
        let data = if find_all(dict, b"/FlateDecode").next().is_some() {
            let mut v = Vec::new();
            if flate2::read::ZlibDecoder::new(raw).take(64 << 20).read_to_end(&mut v).is_err() {
                continue;
            }
            v
        } else {
            raw.to_vec()
        };
        let mut pairs = Vec::new();
        let mut i = 0;
        for _ in 0..n.min(100_000) {
            let Some((num, j)) = read_uint(&data, skip_ws(&data, i)) else { break };
            let Some((off, j)) = read_uint(&data, skip_ws(&data, j)) else { break };
            pairs.push((num, off as usize));
            i = j;
        }
        for (k, &(num, off)) in pairs.iter().enumerate() {
            let start = first as usize + off;
            let end = pairs.get(k + 1).map(|p| first as usize + p.1).unwrap_or(data.len());
            if start <= end && end <= data.len() {
                out.entry(num).or_insert_with(|| data[start..end].to_vec());
            }
        }
    }
    out
}

/// Exact page count from the page tree root's `/Count`, via trailer/xref-stream `/Root`.
/// Falls back to counting `/Type /Page` objects (reported as not exact).
pub fn page_count(b: &[u8]) -> Option<(u32, bool)> {
    if !b.starts_with(b"%PDF-") {
        return None;
    }
    let objs = index_objects(b);
    let streams = object_streams(b, &objs);
    let get = |n: u64| -> Option<&[u8]> { objs.get(&n).map(|&(s, e)| &b[s..e]).or_else(|| streams.get(&n).map(Vec::as_slice)) };
    let exact = (|| {
        let root = find_last_ref(b, b"/Root")?;
        let pages = find_ref(get(root)?, b"/Pages")?;
        let count = find_int(get(pages)?, b"/Count")?;
        u32::try_from(count).ok()
    })();
    if let Some(c) = exact {
        return Some((c, true));
    }
    let is_page = |body: &[u8]| {
        find_all(body, b"/Type").any(|p| {
            let i = skip_ws(body, p + 5);
            body[i..].starts_with(b"/Page") && body.get(i + 5).is_none_or(|c| !c.is_ascii_alphanumeric())
        })
    };
    let n = objs.values().filter(|&&(s, e)| is_page(&b[s..e])).count() + streams.values().filter(|v| is_page(v)).count();
    (n > 0).then_some((n as u32, false))
}

/// Text quality judgement for one page: usable text vs. needs OCR.
#[derive(Debug, Clone, Copy)]
pub struct PageQuality {
    pub non_ws: usize,
    pub alnum_ratio: f32,
    pub garbage_ratio: f32,
    pub images: usize,
}

impl PageQuality {
    pub fn of(text: &str, images: usize) -> PageQuality {
        let mut non_ws = 0usize;
        let mut alnum = 0usize;
        let mut garbage = 0usize;
        for c in text.chars() {
            if c.is_whitespace() {
                continue;
            }
            non_ws += 1;
            if c.is_alphanumeric() {
                alnum += 1;
            }
            if c == '\u{fffd}' || (c.is_control()) || ('\u{e000}'..='\u{f8ff}').contains(&c) {
                garbage += 1;
            }
        }
        let d = non_ws.max(1) as f32;
        PageQuality { non_ws, alnum_ratio: alnum as f32 / d, garbage_ratio: garbage as f32 / d, images }
    }

    /// Text is usable when there is enough of it, it is mostly letters/digits, it is not
    /// mojibake, and the page is not dominated by images with only a caption of text.
    pub fn needs_ocr(&self) -> bool {
        let text_ok = self.non_ws >= 20 && self.alnum_ratio >= 0.5 && self.garbage_ratio < 0.05;
        let image_dominated = self.images > 0 && self.non_ws < 200;
        !text_ok || image_dominated
    }
}

/// Native per-page text: (page number, markdown text, needs OCR).
#[cfg(feature = "pdf-native")]
pub fn native_pages(bytes: &[u8], max_pages: u32) -> Result<Vec<(u32, String, bool)>, super::ExtractError> {
    let doc = lopdf::Document::load_mem(bytes).map_err(|e| super::ExtractError::Corrupt(format!("unreadable PDF: {e}")))?;
    if doc.is_encrypted() {
        return Err(super::ExtractError::Unusable("encrypted PDFs are not supported for native extraction".into()));
    }
    let pages = doc.get_pages();
    if pages.len() as u32 > max_pages {
        return Err(super::ExtractError::TooLarge(format!("PDF has {} pages; exceeds DOCVISION_MAX_PDF_PAGES ({max_pages})", pages.len())));
    }
    let mut out = Vec::with_capacity(pages.len());
    for (num, id) in pages {
        let text = doc.extract_text(&[num]).unwrap_or_default();
        let images = doc.get_page_images(id).map(|v| v.len()).unwrap_or(0);
        let q = PageQuality::of(&text, images);
        out.push((num, text_to_markdown(&text), q.needs_ocr()));
    }
    Ok(out)
}

/// Join wrapped lines into paragraphs; blank lines separate paragraphs.
pub fn text_to_markdown(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut para = String::new();
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() {
            if !para.is_empty() {
                out.push_str(&para);
                out.push_str("\n\n");
                para.clear();
            }
            continue;
        }
        if !para.is_empty() {
            if para.ends_with('-') && !para.ends_with(" -") {
                para.pop();
            } else {
                para.push(' ');
            }
        }
        para.push_str(l);
    }
    if !para.is_empty() {
        out.push_str(&para);
        out.push_str("\n\n");
    }
    out
}

// ---------------------------------------------------------------- LLM routing

use crate::App;
use crate::gateway::{CallSpec, Prio};
use crate::llm::{Accounting, DocRef, Endpoint, LlmRequest};
use crate::markdown::{Segment, SpanKind};
use crate::rpc::{AppError, Ocr, Options};
use crate::source::Staged;
use futures_util::FutureExt;
use futures_util::future::{BoxFuture, try_join_all};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Semaphore;

const SYSTEM_TRANSCRIBE: &str = "You are a precise document transcription engine. Convert the requested pages of the attached PDF to clean GitHub-flavored Markdown. Preserve headings, paragraphs, lists, tables (as GFM tables), links and reading order. Transcribe text in scanned or image-only pages too. Do not summarize, translate or add commentary, and do not repeat page headers/footers as headings. Output only the Markdown.";

struct Llm<'a> {
    app: &'a App,
    ep: &'a Endpoint,
    acct: &'a Accounting,
    sem: &'a Semaphore,
    prio: Prio,
    total: u32,
}

/// The PDF attached to a transcription call: the whole document, or a piece cut from it that
/// holds only original pages `first..=last`. Piece files and uploads are removed on drop.
struct Att {
    path: PathBuf,
    doc: DocRef,
    first: u32,
    last: u32,
    owned: bool,
    upload: Option<(reqwest::Client, Endpoint)>,
}

impl Drop for Att {
    fn drop(&mut self) {
        if self.owned {
            let _ = std::fs::remove_file(&self.path);
        }
        if let Some((client, ep)) = self.upload.take() {
            let doc = self.doc.clone();
            if let Ok(rt) = tokio::runtime::Handle::try_current() {
                rt.spawn(async move { crate::llm::delete_upload(&client, &ep, &doc).await });
            }
        }
    }
}

/// Attach a PDF file: inline when small, uploaded once to the provider when large.
async fn attach(l: &Llm<'_>, path: PathBuf, first: u32, last: u32, owned: bool) -> Result<Arc<Att>, AppError> {
    let size = tokio::fs::metadata(&path).await.map(|m| m.len()).unwrap_or(0);
    let mut att = Att {
        doc: DocRef::Inline { path: path.clone(), mime: "application/pdf", file_name: format!("pages-{first}-{last}.pdf") },
        path,
        first,
        last,
        owned,
        upload: None,
    };
    if size > l.app.cfg.provider_upload_threshold {
        match crate::llm::upload(l.app.gateway.http(), l.ep, att.path.clone(), "application/pdf", size).await {
            Ok(Some(d)) => {
                att.doc = d;
                att.upload = Some((l.app.gateway.http().clone(), l.ep.clone()));
            }
            Ok(None) => {}
            Err(e) => return Err(e.to_app("extraction")),
        }
    }
    Ok(Arc::new(att))
}

/// Cut `att` into one piece per range (original page numbers). `None` when the PDF can't be
/// split (encrypted, malformed, or `pdf-native` not compiled): callers then keep `att`.
async fn split(l: &Llm<'_>, att: &Att, ranges: &[(u32, u32)]) -> Option<Vec<Arc<Att>>> {
    #[cfg(feature = "pdf-native")]
    {
        let (src, first, dir, rs) = (att.path.clone(), att.first, l.app.staging_dir(), ranges.to_vec());
        let paths = match l.app.cpu.run(move || split_pdf(&src, first, &rs, &dir)).await {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => {
                tracing::debug!(target: "docvision_llm_ws::pdf", error = %e, "PDF not split; sending it whole");
                return None;
            }
            Err(_) => return None,
        };
        let mut out = Vec::with_capacity(paths.len());
        let mut paths = paths.into_iter();
        for &(a, b) in ranges {
            let p = paths.next()?;
            match attach(l, p.clone(), a, b, true).await {
                Ok(x) => out.push(x),
                Err(_) => {
                    let _ = std::fs::remove_file(&p);
                    for rest in paths {
                        let _ = std::fs::remove_file(rest);
                    }
                    return None;
                }
            }
        }
        Some(out)
    }
    #[cfg(not(feature = "pdf-native"))]
    {
        let _ = (l, att, ranges);
        None
    }
}

/// Write one small PDF per range of original page numbers (`first` is the original number of
/// `src`'s first page). Each piece keeps only its pages; inherited page attributes are copied
/// down and document-level references to other pages (outline, structure tree, …) are dropped.
#[cfg(feature = "pdf-native")]
fn split_pdf(src: &std::path::Path, first: u32, ranges: &[(u32, u32)], dir: &std::path::Path) -> Result<Vec<PathBuf>, String> {
    use lopdf::{Document, Object, ObjectId};
    let doc = Document::load(src).map_err(|e| e.to_string())?;
    if doc.is_encrypted() {
        return Err("encrypted PDF".into());
    }
    let pages = doc.get_pages();
    let root = doc.catalog().and_then(|c| c.get(b"Pages")).and_then(Object::as_reference).map_err(|e| e.to_string())?;
    let inherited = |page: ObjectId| -> Vec<(&'static [u8], Object)> {
        let mut out: Vec<(&'static [u8], Object)> = Vec::new();
        let mut cur = doc.get_dictionary(page).ok().and_then(|d| d.get(b"Parent").and_then(Object::as_reference).ok());
        for _ in 0..64 {
            let Some(id) = cur else { break };
            let Ok(d) = doc.get_dictionary(id) else { break };
            for k in [b"Resources".as_slice(), b"MediaBox", b"CropBox", b"Rotate"] {
                if !out.iter().any(|(x, _)| *x == k) {
                    if let Ok(v) = d.get(k) {
                        out.push((k, v.clone()));
                    }
                }
            }
            cur = d.get(b"Parent").and_then(Object::as_reference).ok();
        }
        out
    };
    let mut written = Vec::with_capacity(ranges.len());
    let result = (|| {
        for &(a, b) in ranges {
            let mut d = doc.clone();
            let mut kids = Vec::new();
            for n in a..=b {
                let id = *pages.get(&(n + 1 - first)).ok_or_else(|| format!("page {n} not found"))?;
                let attrs = inherited(id);
                let page = d.get_dictionary_mut(id).map_err(|e| e.to_string())?;
                for (k, v) in attrs {
                    if !page.has(k) {
                        page.set(k, v);
                    }
                }
                page.set("Parent", Object::Reference(root));
                kids.push(Object::Reference(id));
            }
            let count = kids.len() as i64;
            let tree = d.get_dictionary_mut(root).map_err(|e| e.to_string())?;
            tree.set("Kids", kids);
            tree.set("Count", count);
            tree.remove(b"Parent");
            if let Ok(cat) = d.catalog_mut() {
                for k in [b"Outlines".as_slice(), b"Dests", b"StructTreeRoot", b"OpenAction", b"Names", b"AcroForm", b"PageLabels"] {
                    cat.remove(k);
                }
            }
            d.prune_objects();
            d.compress();
            let path = dir.join(format!("{}-p{a}-{b}.pdf", uuid::Uuid::now_v7()));
            written.push(path.clone());
            d.save(&path).map_err(|e| e.to_string())?;
        }
        Ok(())
    })();
    match result {
        Ok(()) => Ok(written),
        Err(e) => {
            for p in written {
                let _ = std::fs::remove_file(p);
            }
            Err(e)
        }
    }
}

/// Transcribe pages a..=b from `att`; on truncation split the range in half (cutting the PDF
/// again when possible) and retry, so output is never silently incomplete. A single truncated
/// page is reported as partial. (first page, last page, Markdown, truncated)
type Parts = Vec<(u32, u32, String, bool)>;

fn transcribe<'a>(l: &'a Llm<'a>, a: u32, b: u32, att: Arc<Att>) -> BoxFuture<'a, Result<Parts, AppError>> {
    async move {
        let n = l.total;
        let user = match (a == b, att.first == 1 && att.last == n, att.first == a && att.last == b) {
            (true, true, _) => format!("Transcribe page {a} (of {n}) of the attached PDF. Output only that page."),
            (true, _, true) => format!("Transcribe page {a} (of {n}). The attached PDF contains only this page."),
            (true, _, _) => format!(
                "Transcribe page {a} (of {n}). The attached PDF starts at page {} of the document, so this is its page {}. Output only that page.",
                att.first,
                a + 1 - att.first
            ),
            (false, true, _) => {
                format!("Transcribe pages {a} through {b} (of {n}) of the attached PDF, in order. Output only those pages.")
            }
            (false, _, true) => {
                format!("Transcribe pages {a} through {b} (of {n}). The attached PDF contains exactly these pages; transcribe all of it, in order.")
            }
            (false, _, _) => format!(
                "Transcribe pages {a} through {b} (of {n}). The attached PDF contains pages {}-{} of the document, so these are its pages {}-{}. Output only those pages, in order.",
                att.first,
                att.last,
                a + 1 - att.first,
                b + 1 - att.first
            ),
        };
        let req = LlmRequest {
            system: SYSTEM_TRANSCRIBE.into(),
            user,
            doc: Some(att.doc.clone()),
            max_output_tokens: l.ep.cfg.max_output_tokens,
            json: false,
            schema: None,
        };
        let spec = CallSpec { stage: "extraction", purpose: "pdf_transcription", span: Some(format!("pages {a}-{b}")), prio: l.prio };
        let r = l.app.gateway.call(l.ep, &req, &spec, l.sem, l.acct).await.map_err(|e| e.to_app("extraction"))?;
        if r.truncated && b > a {
            let mid = a + (b - a) / 2;
            let halves = split(l, &att, &[(a, mid), (mid + 1, b)]).await;
            let (left, right) = match halves {
                Some(mut v) if v.len() == 2 => {
                    let right = v.pop().unwrap_or_else(|| att.clone());
                    (v.pop().unwrap_or_else(|| att.clone()), right)
                }
                _ => (att.clone(), att.clone()),
            };
            drop(att);
            let (x, y) = tokio::try_join!(transcribe(l, a, mid, left), transcribe(l, mid + 1, b, right))?;
            return Ok(x.into_iter().chain(y).collect());
        }
        Ok(vec![(a, b, crate::markdown::strip_llm_wrapper(&r.text), r.truncated)])
    }
    .boxed()
}

#[allow(clippy::too_many_arguments)]
async fn llm_ranges(
    app: &App,
    staged: &Staged,
    ep: &Endpoint,
    acct: &Accounting,
    sem: &Semaphore,
    prio: Prio,
    total: u32,
    ranges: &[(u32, u32)],
) -> Result<Parts, AppError> {
    let per = ((ep.cfg.max_output_tokens as f64 / ep.cfg.tokens_per_page as f64) * 0.8).floor().max(1.0) as u32;
    let mut batches = Vec::new();
    for &(a, b) in ranges {
        let mut s = a;
        while s <= b {
            let e = (s + per - 1).min(b);
            batches.push((s, e));
            s = e + 1;
        }
    }
    let l = Llm { app, ep, acct, sem, prio, total };
    let whole = || Att {
        path: staged.path.clone(),
        doc: DocRef::Inline { path: staged.path.clone(), mime: "application/pdf", file_name: "document.pdf".into() },
        first: 1,
        last: total,
        owned: false,
        upload: None,
    };
    // Each batch gets a PDF holding only its pages, so a call pays input tokens for those pages
    // alone and provider per-file limits apply per piece. One batch covering the whole document
    // (or a PDF that can't be split) sends the original file, uploaded once when large.
    let pieces = if batches == [(1, total)] { None } else { split(&l, &whole(), &batches).await };
    let atts = match pieces {
        Some(p) => p,
        None => {
            let a = attach(&l, staged.path.clone(), 1, total, false).await?;
            vec![a; batches.len()]
        }
    };
    // Batches run concurrently; the first failure cancels the siblings (dropping their pieces).
    let res = try_join_all(batches.iter().zip(atts).map(|(&(a, b), att)| transcribe(&l, a, b, att))).await;
    let mut out: Vec<_> = res?.into_iter().flatten().collect();
    out.sort_by_key(|x| x.0);
    Ok(out)
}

#[cfg_attr(not(feature = "pdf-native"), allow(dead_code))]
fn ranges_of(pages: &[u32]) -> Vec<(u32, u32)> {
    let mut out: Vec<(u32, u32)> = Vec::new();
    for &p in pages {
        match out.last_mut() {
            Some((_, b)) if *b + 1 == p => *b = p,
            _ => out.push((p, p)),
        }
    }
    out
}

pub async fn run(
    app: &App,
    staged: &Staged,
    opts: &Options,
    ep: Option<&Endpoint>,
    acct: &Accounting,
    sem: &Semaphore,
    prio: Prio,
) -> Result<super::Extracted, AppError> {
    let path = staged.path.clone();
    let max_pages = app.cfg.max_pdf_pages;
    let (bytes, count) = app
        .cpu
        .run(move || {
            let b = std::fs::read(&path).ok()?;
            let c = page_count(&b);
            Some((b, c))
        })
        .await
        .map_err(|_| AppError::new(422, "corrupt_document", "unreadable PDF").stage("extraction"))?
        .ok_or_else(|| AppError::internal("cannot read staged PDF"))?;
    let (total, exact) =
        count.ok_or_else(|| AppError::new(422, "corrupt_document", "not a readable PDF (no page tree)").stage("extraction"))?;
    if total > max_pages {
        return Err(AppError::new(413, "too_many_pages", format!("PDF has {total} pages; exceeds DOCVISION_MAX_PDF_PAGES ({max_pages})"))
            .stage("extraction"));
    }
    let mut ex = super::Extracted {
        pages: Some(super::PageCount { total, method: if exact { "pdf_page_tree" } else { "pdf_page_objects" }, exact }),
        ..Default::default()
    };
    let mut out = String::new();
    let push_llm = |ex: &mut super::Extracted, out: &mut String, parts: Vec<(u32, u32, String, bool)>| {
        for (a, b, text, truncated) in parts {
            ex.segments.push(Segment { start: out.len(), kind: SpanKind::Page, from: a, to: b });
            out.push_str(text.trim());
            out.push_str("\n\n");
            if truncated {
                ex.partial = true;
                ex.warnings.push(format!("page {a} transcription was truncated at the model output limit"));
            }
        }
    };
    match opts.ocr {
        Ocr::On => {
            drop(bytes);
            let ep = ep.ok_or_else(|| AppError::bad_request("provider_not_configured", "PDF transcription needs an LLM provider"))?;
            if !opts.skip_local() {
                #[cfg(feature = "pdf-render")]
                {
                    let parts = render::transcribe_rendered(app, staged, ep, acct, sem, prio, total).await?;
                    push_llm(&mut ex, &mut out, parts);
                }
                #[cfg(not(feature = "pdf-render"))]
                return Err(AppError::feature_not_compiled("pdf-render"));
            } else if total > 0 {
                let parts = llm_ranges(app, staged, ep, acct, sem, prio, total, &[(1, total)]).await?;
                push_llm(&mut ex, &mut out, parts);
            }
        }
        Ocr::Auto | Ocr::Off => {
            #[cfg(not(feature = "pdf-native"))]
            {
                let _ = (bytes, &mut out, push_llm, ep, acct, sem, prio);
                return Err(AppError::feature_not_compiled("pdf-native"));
            }
            #[cfg(feature = "pdf-native")]
            {
                let pages = app
                    .cpu
                    .run(move || native_pages(&bytes, max_pages))
                    .await
                    .map_err(|_| AppError::new(422, "corrupt_document", "PDF parser failed").stage("extraction"))??;
                let scanned: Vec<u32> = pages.iter().filter(|p| p.2).map(|p| p.0).collect();
                let mut llm_parts = std::collections::BTreeMap::new();
                if opts.ocr == Ocr::Auto && !scanned.is_empty() {
                    let ep =
                        ep.ok_or_else(|| AppError::bad_request("provider_not_configured", "scanned pages need an LLM provider for OCR"))?;
                    for part in llm_ranges(app, staged, ep, acct, sem, prio, total, &ranges_of(&scanned)).await? {
                        llm_parts.insert(part.0, part);
                    }
                } else if opts.ocr == Ocr::Off && !scanned.is_empty() {
                    if !opts.allow_partial {
                        return Err(AppError::new(
                            422,
                            "scanned_pages",
                            format!("{} page(s) have no usable text and ocr=off (pages {:?})", scanned.len(), ranges_of(&scanned)),
                        )
                        .stage("extraction"));
                    }
                    ex.partial = true;
                    ex.warnings.push(format!("pages {:?} have no usable text and were not transcribed (ocr=off)", ranges_of(&scanned)));
                }
                let mut covered_to = 0;
                for (num, md, needs) in pages {
                    if num <= covered_to {
                        continue;
                    }
                    if let Some(part) = llm_parts.remove(&num) {
                        covered_to = part.1;
                        push_llm(&mut ex, &mut out, vec![part]);
                        continue;
                    }
                    if needs && opts.ocr == Ocr::Auto {
                        continue;
                    }
                    ex.segments.push(Segment { start: out.len(), kind: SpanKind::Page, from: num, to: num });
                    out.push_str(&md);
                }
            }
        }
    }
    ex.content = out;
    Ok(ex)
}

#[cfg(feature = "pdf-render")]
mod render {
    use super::*;
    use pdfium_render::prelude::*;

    fn render_pages(path: &std::path::Path, dir: &std::path::Path) -> Result<Vec<std::path::PathBuf>, String> {
        let lib = std::env::var("PDFIUM_DYNAMIC_LIB_PATH").unwrap_or_else(|_| "./".into());
        let bindings = Pdfium::bind_to_library(Pdfium::pdfium_platform_library_name_at_path(&lib))
            .or_else(|_| Pdfium::bind_to_system_library())
            .map_err(|e| format!("Pdfium unavailable: {e}"))?;
        let pdfium = Pdfium::new(bindings);
        let doc = pdfium.load_pdf_from_file(path, None).map_err(|e| format!("Pdfium cannot open PDF: {e}"))?;
        let cfg = PdfRenderConfig::new().set_target_width(1600).set_maximum_height(2400);
        let mut out = Vec::new();
        for (i, page) in doc.pages().iter().enumerate() {
            let p = dir.join(format!("{}-p{}.jpg", uuid::Uuid::now_v7(), i + 1));
            page.render_with_config(&cfg)
                .map_err(|e| e.to_string())?
                .as_image()
                .into_rgb8()
                .save_with_format(&p, image::ImageFormat::Jpeg)
                .map_err(|e| e.to_string())?;
            out.push(p);
        }
        Ok(out)
    }

    pub async fn transcribe_rendered(
        app: &App,
        staged: &Staged,
        ep: &Endpoint,
        acct: &Accounting,
        sem: &Semaphore,
        prio: Prio,
        _total: u32,
    ) -> Result<Vec<(u32, u32, String, bool)>, AppError> {
        let (path, dir) = (staged.path.clone(), app.staging_dir());
        let images = app
            .cpu
            .run(move || render_pages(&path, &dir))
            .await
            .map_err(|_| AppError::new(422, "corrupt_document", "PDF rendering failed").stage("extraction"))?
            .map_err(|e| AppError::new(422, "render_failed", e).stage("extraction"))?;
        let futs = images.iter().enumerate().map(|(i, p)| async move {
            let req = LlmRequest {
                system: SYSTEM_TRANSCRIBE.into(),
                user: "Transcribe this page image to Markdown.".into(),
                doc: Some(DocRef::Inline { path: p.clone(), mime: "image/jpeg", file_name: "page.jpg".into() }),
                max_output_tokens: ep.cfg.max_output_tokens,
                json: false,
                schema: None,
            };
            let n = i as u32 + 1;
            let spec = CallSpec { stage: "extraction", purpose: "pdf_page_image_transcription", span: Some(format!("page {n}")), prio };
            let r = app.gateway.call(ep, &req, &spec, sem, acct).await.map_err(|e| e.to_app("extraction"))?;
            Ok::<_, AppError>((n, n, crate::markdown::strip_llm_wrapper(&r.text), r.truncated))
        });
        let res = try_join_all(futs).await;
        for p in &images {
            let _ = std::fs::remove_file(p);
        }
        res
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "pdf-native")]
    #[test]
    fn splits_pdf_into_valid_pieces() {
        use lopdf::{Document, Object, dictionary};
        // Nested page tree; MediaBox and Resources are inherited from the intermediate node.
        let mut doc = Document::with_version("1.5");
        let root = doc.new_object_id();
        let mid = doc.new_object_id();
        let font = doc.add_object(dictionary! {"Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica"});
        let mut kids = Vec::new();
        for _ in 0..5 {
            kids.push(Object::Reference(doc.add_object(dictionary! {"Type" => "Page", "Parent" => mid})));
        }
        doc.objects.insert(
            mid,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Parent" => root, "Kids" => kids, "Count" => 5,
                "MediaBox" => vec![0.into(), 0.into(), 300.into(), 400.into()],
                "Resources" => dictionary! {"Font" => dictionary! {"F1" => font}},
            }),
        );
        doc.objects.insert(root, Object::Dictionary(dictionary! {"Type" => "Pages", "Kids" => vec![mid.into()], "Count" => 5}));
        let outline = doc.add_object(dictionary! {"Type" => "Outlines"});
        let catalog = doc.add_object(dictionary! {"Type" => "Catalog", "Pages" => root, "Outlines" => outline});
        doc.trailer.set("Root", catalog);
        let dir = std::env::temp_dir().join(format!("docvision-split-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.pdf");
        doc.save(&src).unwrap();

        let pieces = split_pdf(&src, 1, &[(1, 2), (3, 5)], &dir).unwrap();
        let counts: Vec<usize> = pieces.iter().map(|p| Document::load(p).unwrap().get_pages().len()).collect();
        assert_eq!(counts, vec![2, 3]);
        let piece = Document::load(&pieces[1]).unwrap();
        for (_, id) in piece.get_pages() {
            let page = piece.get_dictionary(id).unwrap();
            assert!(page.has(b"MediaBox") && page.has(b"Resources"), "inherited attributes copied down");
        }
        assert!(!piece.catalog().unwrap().has(b"Outlines"));
        // Splitting a piece again uses original page numbers.
        let again = split_pdf(&pieces[1], 3, &[(4, 4)], &dir).unwrap();
        assert_eq!(Document::load(&again[0]).unwrap().get_pages().len(), 1);
        // Out-of-range pages fail cleanly, leaving nothing behind.
        assert!(split_pdf(&src, 1, &[(1, 2), (6, 7)], &dir).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    use super::*;

    pub fn minimal_pdf(pages: usize) -> Vec<u8> {
        let mut s = String::from("%PDF-1.4\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n");
        let kids: Vec<String> = (0..pages).map(|i| format!("{} 0 R", i + 3)).collect();
        s.push_str(&format!("2 0 obj << /Type /Pages /Kids [{}] /Count {} >> endobj\n", kids.join(" "), pages));
        for i in 0..pages {
            s.push_str(&format!("{} 0 obj << /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >> endobj\n", i + 3));
        }
        s.push_str("trailer << /Size 10 /Root 1 0 R >>\n%%EOF\n");
        s.into_bytes()
    }

    #[test]
    fn counts_pages_exactly() {
        assert_eq!(page_count(&minimal_pdf(7)), Some((7, true)));
        assert_eq!(page_count(&minimal_pdf(1)), Some((1, true)));
        assert_eq!(page_count(b"not a pdf"), None);
    }

    #[test]
    fn counts_pages_in_object_streams() {
        use std::io::Write;
        let objs = "<< /Type /Catalog /Pages 2 0 R >> << /Type /Pages /Kids [] /Count 42 >>";
        let header = "1 0 2 34 ";
        let data = format!("{header}{objs}");
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(data.as_bytes()).unwrap();
        let z = enc.finish().unwrap();
        let mut pdf = b"%PDF-1.5\n5 0 obj << /Type /ObjStm /N 2 /First 9 /Filter /FlateDecode /Length ".to_vec();
        pdf.extend_from_slice(format!("{} >>\nstream\n", z.len()).as_bytes());
        pdf.extend_from_slice(&z);
        pdf.extend_from_slice(b"\nendstream\nendobj\n6 0 obj << /Type /XRef /Root 1 0 R /Size 7 >> stream\nendstream\nendobj\n%%EOF");
        assert_eq!(page_count(&pdf), Some((42, true)));
    }

    #[test]
    fn quality() {
        assert!(!PageQuality::of("This is a perfectly normal page of text with words.", 0).needs_ocr());
        assert!(PageQuality::of("", 1).needs_ocr());
        assert!(PageQuality::of("Figure 1", 1).needs_ocr());
        assert!(PageQuality::of("\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}abc", 0).needs_ocr());
    }

    #[test]
    fn joins_lines() {
        assert_eq!(text_to_markdown("Hello\nwor-\nld\n\nNext"), "Hello world\n\nNext\n\n");
    }
}
