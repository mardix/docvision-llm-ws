//! Native extraction (CPU lane). Each parser streams XML from a size-limited ZIP reader
//! and writes Markdown into one pre-sized `String`.

#[cfg(feature = "odf-epub")]
pub mod epub;
pub mod html;
pub mod image;
#[cfg(feature = "odf-epub")]
pub mod odf;
pub mod ooxml;
pub mod pdf;
pub mod text;

use crate::markdown::Segment;
use crate::source::Format;
use quick_xml::events::BytesStart;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

pub const EXTRACTOR_VERSION: &str = "1";

#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct PageCount {
    pub total: u32,
    pub method: &'static str,
    pub exact: bool,
}

#[derive(Debug, Default)]
pub struct Extracted {
    pub content: String,
    pub title_hint: Option<String>,
    pub segments: Vec<Segment>,
    pub pages: Option<PageCount>,
    pub sheet_count: Option<u32>,
    pub slide_count: Option<u32>,
    pub warnings: Vec<String>,
    pub partial: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_archive_ratio: u64,
    pub max_archive_bytes: u64,
    pub max_pdf_pages: u32,
    pub max_decoded_pixels: u64,
}

#[derive(Debug)]
pub enum ExtractError {
    Corrupt(String),
    TooLarge(String),
    Unusable(String),
}

impl From<ExtractError> for crate::rpc::AppError {
    fn from(e: ExtractError) -> Self {
        match e {
            ExtractError::Corrupt(m) => crate::rpc::AppError::new(422, "corrupt_document", m),
            ExtractError::TooLarge(m) => crate::rpc::AppError::new(413, "archive_too_large", m),
            ExtractError::Unusable(m) => crate::rpc::AppError::new(422, "unusable_document", m),
        }
        .stage("extraction")
    }
}

impl From<quick_xml::Error> for ExtractError {
    fn from(e: quick_xml::Error) -> Self {
        // A zip-bomb limit surfaces as an I/O error from inside the XML reader.
        if let quick_xml::Error::Io(io) = &e {
            if io.kind() == std::io::ErrorKind::FileTooLarge {
                return ExtractError::TooLarge(io.to_string());
            }
        }
        ExtractError::Corrupt(format!("malformed XML: {e}"))
    }
}

pub type XResult<T> = Result<T, ExtractError>;

/// Estimate output capacity so the Markdown buffer rarely reallocates.
pub fn capacity_hint(path: &Path, mult: usize) -> usize {
    std::fs::metadata(path).map(|m| (m.len() as usize).saturating_mul(mult)).unwrap_or(4096).min(64 << 20)
}

/// Run the native extractor for `format`. Blocking: call on the CPU lane.
pub fn extract_native(path: &Path, format: Format, limits: &Limits) -> XResult<Extracted> {
    match format {
        Format::Markdown | Format::Text => text::extract(path, format),
        Format::Html => html::extract_file(path),
        Format::Docx => ooxml::docx(path, limits),
        Format::Xlsx => ooxml::xlsx(path, limits),
        Format::Pptx => ooxml::pptx(path, limits),
        #[cfg(feature = "odf-epub")]
        Format::Odt | Format::Ods | Format::Odp => odf::extract(path, format, limits),
        #[cfg(feature = "odf-epub")]
        Format::Epub => epub::extract(path, limits),
        _ => Err(ExtractError::Unusable(format!("{} has no native extractor", format.name()))),
    }
}

/// A ZIP package with zip-bomb protection: per-read accounting against an absolute byte cap
/// and an expansion ratio relative to the archive size. Entries are read lazily.
pub struct Package {
    zip: zip::ZipArchive<BufReader<File>>,
    budget: u64,
}

impl Package {
    pub fn open(path: &Path, limits: &Limits) -> XResult<Package> {
        let f = File::open(path).map_err(|e| ExtractError::Corrupt(e.to_string()))?;
        let size = f.metadata().map(|m| m.len()).unwrap_or(0).max(1);
        let zip = zip::ZipArchive::new(BufReader::new(f)).map_err(|e| ExtractError::Corrupt(format!("invalid ZIP: {e}")))?;
        let budget = limits.max_archive_bytes.min(size.saturating_mul(limits.max_archive_ratio).max(1 << 20));
        Ok(Package { zip, budget })
    }

    pub fn has(&self, name: &str) -> bool {
        self.zip.index_for_name(name).is_some()
    }

    /// Stream an entry through `f`; `None` if the entry does not exist.
    pub fn with_entry<T>(&mut self, name: &str, f: impl FnOnce(&mut dyn std::io::BufRead) -> XResult<T>) -> XResult<Option<T>> {
        let name = name.trim_start_matches('/');
        let Some(idx) = self.zip.index_for_name(name) else { return Ok(None) };
        let entry = self.zip.by_index(idx).map_err(|e| ExtractError::Corrupt(format!("bad ZIP entry {name}: {e}")))?;
        let mut limited = Limited { inner: entry, remaining: self.budget, used: 0 };
        let mut reader = BufReader::with_capacity(64 * 1024, &mut limited);
        let out = f(&mut reader);
        drop(reader);
        self.budget = self.budget.saturating_sub(limited.used);
        out.map(Some)
    }

    pub fn read_string(&mut self, name: &str) -> XResult<Option<String>> {
        self.with_entry(name, |r| {
            let mut s = String::new();
            r.read_to_string(&mut s).map_err(io_err)?;
            Ok(s)
        })
    }
}

pub fn io_err(e: std::io::Error) -> ExtractError {
    if e.kind() == std::io::ErrorKind::FileTooLarge { ExtractError::TooLarge(e.to_string()) } else { ExtractError::Corrupt(e.to_string()) }
}

struct Limited<R> {
    inner: R,
    remaining: u64,
    used: u64,
}

impl<R: Read> Read for Limited<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.used += n as u64;
        if self.used > self.remaining {
            return Err(std::io::Error::new(std::io::ErrorKind::FileTooLarge, "archive expansion exceeds DOCVISION_MAX_ARCHIVE_EXPANSION"));
        }
        Ok(n)
    }
}

pub fn xml_reader<R: std::io::BufRead>(r: R) -> quick_xml::Reader<R> {
    let mut x = quick_xml::Reader::from_reader(r);
    x.config_mut().trim_text(false);
    x.config_mut().check_end_names = false;
    x
}

/// Attribute value by local name (ignores namespace prefix).
pub fn attr(e: &BytesStart, local: &[u8]) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == local)
        .and_then(|a| a.normalized_value(quick_xml::XmlVersion::Implicit1_0).ok().map(|v| v.into_owned()))
}

/// Attribute value by exact qualified name (e.g. `r:id`).
pub fn attr_q(e: &BytesStart, q: &[u8]) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == q)
        .and_then(|a| a.normalized_value(quick_xml::XmlVersion::Implicit1_0).ok().map(|v| v.into_owned()))
}

/// Text carried by a text event or an entity reference. quick-xml reports `&amp;`, `&#233;` …
/// as separate events; predefined and numeric references are resolved, unknown ones kept as written.
pub(crate) fn xml_text<'a>(ev: &'a quick_xml::events::Event<'a>) -> std::borrow::Cow<'a, str> {
    use quick_xml::events::Event;
    use std::borrow::Cow;
    match ev {
        Event::Text(t) => t.xml10_content().unwrap_or_default(),
        Event::GeneralRef(r) if r.is_char_ref() => {
            r.resolve_char_ref().ok().flatten().map(|c| Cow::Owned(c.to_string())).unwrap_or_default()
        }
        Event::GeneralRef(r) => {
            let name = r.decode().unwrap_or_default();
            match quick_xml::escape::resolve_predefined_entity(&name) {
                Some(s) => Cow::Borrowed(s),
                None => Cow::Owned(format!("&{name};")),
            }
        }
        _ => Cow::Borrowed(""),
    }
}

/// Parse a `.rels` part into (Id -> (Target, external)).
pub fn read_rels(pkg: &mut Package, name: &str) -> XResult<std::collections::HashMap<String, (String, bool)>> {
    use quick_xml::events::Event;
    let mut map = std::collections::HashMap::new();
    pkg.with_entry(name, |r| {
        let mut x = xml_reader(r);
        let mut buf = Vec::new();
        loop {
            match x.read_event_into(&mut buf)? {
                Event::Start(e) | Event::Empty(e) if e.local_name().as_ref() == b"Relationship" => {
                    if let (Some(id), Some(t)) = (attr(&e, b"Id"), attr(&e, b"Target")) {
                        let ext = attr(&e, b"TargetMode").is_some_and(|m| m == "External");
                        map.insert(id, (t, ext));
                    }
                }
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }
        Ok(())
    })?;
    Ok(map)
}

/// Resolve a relationship target relative to the part's directory.
pub fn resolve(base_dir: &str, target: &str) -> String {
    if let Some(abs) = target.strip_prefix('/') {
        return abs.to_string();
    }
    let mut parts: Vec<&str> = base_dir.split('/').filter(|s| !s.is_empty()).collect();
    for seg in target.split('/') {
        match seg {
            ".." => {
                parts.pop();
            }
            "." | "" => {}
            s => parts.push(s),
        }
    }
    parts.join("/")
}

/// Escape a table cell for GFM.
pub fn cell(s: &str) -> String {
    s.trim().replace('|', "\\|").replace('\n', "<br>")
}

/// Append one GFM table row with at least `cols` cells (extra cells are kept, never dropped).
pub fn write_row(out: &mut String, row: &[String], cols: usize) {
    out.push('|');
    for c in 0..cols.max(row.len()) {
        out.push(' ');
        out.push_str(&cell(row.get(c).map(String::as_str).unwrap_or("")));
        out.push_str(" |");
    }
    out.push('\n');
}

pub fn write_separator(out: &mut String, cols: usize) {
    out.push('|');
    for _ in 0..cols {
        out.push_str(" --- |");
    }
    out.push('\n');
}

/// Write a GFM table; the first row is the header. Pads ragged rows so no cell is dropped.
pub fn write_table(out: &mut String, rows: &[Vec<String>]) {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    if cols == 0 {
        return;
    }
    for (i, row) in rows.iter().enumerate() {
        write_row(out, row, cols);
        if i == 0 {
            write_separator(out, cols);
        }
    }
    out.push('\n');
}

/// Read Dublin Core title from an OOXML/ODF metadata part.
pub fn read_dc_title(pkg: &mut Package, name: &str) -> Option<String> {
    use quick_xml::events::Event;
    pkg.with_entry(name, |r| {
        let mut x = xml_reader(r);
        let mut buf = Vec::new();
        let mut in_title = false;
        let mut title = String::new();
        loop {
            match x.read_event_into(&mut buf)? {
                Event::Start(e) if e.local_name().as_ref() == b"title" => in_title = true,
                Event::End(e) if e.local_name().as_ref() == b"title" => break,
                ev @ (Event::Text(_) | Event::GeneralRef(_)) if in_title => title.push_str(&xml_text(&ev)),
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }
        Ok(title)
    })
    .ok()
    .flatten()
    .map(|t| t.trim().to_string())
    .filter(|t| !t.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_entities_are_resolved() {
        let mut r = quick_xml::Reader::from_str("<t>A &amp; B &lt;c&gt; &#233;&#x41; &quot;q&quot; &custom;</t>");
        let mut out = String::new();
        loop {
            match r.read_event().unwrap() {
                quick_xml::events::Event::Eof => break,
                ev => out.push_str(&xml_text(&ev)),
            }
        }
        assert_eq!(out, "A & B <c> éA \"q\" &custom;");
    }
}
