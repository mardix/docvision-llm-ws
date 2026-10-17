//! Embedded image extraction (`extract_images`): extractors leave a placeholder where an image
//! sits; [`resolve`] replaces it with a Markdown reference and collects the image bytes once
//! (named by content hash, so repeats and identical images are stored a single time).

use super::{Extracted, Package, XResult};
use crate::markdown::Segment;
use std::collections::HashMap;

/// Images smaller than this on both sides (icons, bullets, spacers) are skipped.
const MIN_SIDE: u32 = 32;
const MIN_BYTES: usize = 256;
/// Per-document caps.
pub const MAX_IMAGES: usize = 500;
pub const MAX_TOTAL_BYTES: u64 = 64 << 20;

const OPEN: char = '\u{E000}';
const CLOSE: char = '\u{E001}';
const ALT: char = '\u{E002}';

/// Placeholder for an image at this point of the text. `key` is a package path, or `#N` for
/// the N-th entry of [`Extracted::inline_images`].
pub fn marker(key: &str, alt: Option<&str>) -> String {
    let alt = alt.map(clean_alt).unwrap_or_default();
    format!("{OPEN}{key}{ALT}{alt}{CLOSE}")
}

fn clean_alt(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(*c, '[' | ']' | OPEN | CLOSE | ALT))
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_string()
}

/// An extracted image, still in memory.
#[derive(Debug, Clone)]
pub struct Image {
    pub id: String,
    pub ext: &'static str,
    pub bytes: Vec<u8>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub location: Option<String>,
    pub alt: Option<String>,
}

impl Image {
    pub fn file_name(&self) -> String {
        format!("{}.{}", self.id, self.ext)
    }
}

/// File extension for an image from its bytes (`None`: not an image we keep).
pub fn sniff(b: &[u8]) -> Option<&'static str> {
    let head = &b[..b.len().min(512)];
    let text = String::from_utf8_lossy(head);
    Some(match b {
        [0x89, b'P', b'N', b'G', ..] => "png",
        [0xFF, 0xD8, 0xFF, ..] => "jpg",
        [b'G', b'I', b'F', b'8', ..] => "gif",
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => "webp",
        [b'B', b'M', ..] => "bmp",
        [b'I', b'I', 0x2A, 0x00, ..] | [b'M', b'M', 0x00, 0x2A, ..] => "tif",
        [0xD7, 0xCD, 0xC6, 0x9A, ..] => "wmf",
        _ if b.len() > 44 && &b[40..44] == b" EMF" => "emf",
        _ if text.contains("<svg") => "svg",
        _ => return None,
    })
}

#[derive(Debug, Default)]
pub struct Images {
    pub enabled: bool,
    pub items: Vec<Image>,
    by_id: HashMap<String, usize>,
    total: u64,
    skipped_small: usize,
    skipped_cap: usize,
    unreadable: usize,
}

impl Images {
    pub fn new(enabled: bool) -> Images {
        Images { enabled, ..Default::default() }
    }

    /// Keep an image; returns the Markdown reference, or `None` when it is skipped.
    pub fn add(&mut self, bytes: Vec<u8>, location: Option<String>, alt: Option<&str>) -> Option<String> {
        let Some(ext) = sniff(&bytes) else {
            self.unreadable += 1;
            return None;
        };
        let size = imagesize::blob_size(&bytes).ok();
        let (width, height) = (size.map(|s| s.width as u32), size.map(|s| s.height as u32));
        if bytes.len() < MIN_BYTES || matches!((width, height), (Some(w), Some(h)) if w < MIN_SIDE && h < MIN_SIDE) {
            self.skipped_small += 1;
            return None;
        }
        let id = blake3::hash(&bytes).to_hex()[..16].to_string();
        let alt = alt.map(clean_alt).filter(|a| !a.is_empty());
        let reference = format!("![{}](images/{id}.{ext})", alt.as_deref().unwrap_or("image"));
        if self.by_id.contains_key(&id) {
            return Some(reference);
        }
        if self.items.len() >= MAX_IMAGES || self.total + bytes.len() as u64 > MAX_TOTAL_BYTES {
            self.skipped_cap += 1;
            return None;
        }
        self.total += bytes.len() as u64;
        self.by_id.insert(id.clone(), self.items.len());
        self.items.push(Image { id, ext, bytes, width, height, location, alt });
        Some(reference)
    }

    pub fn warnings(&self) -> Vec<String> {
        let mut w = Vec::new();
        if self.skipped_cap > 0 {
            w.push(format!(
                "{} image(s) were not extracted: the per-document limit ({MAX_IMAGES} images or {} MiB) was reached",
                self.skipped_cap,
                MAX_TOTAL_BYTES >> 20
            ));
        }
        if self.unreadable > 0 {
            w.push(format!("{} embedded object(s) were not recognised as images and were skipped", self.unreadable));
        }
        w
    }
}

/// Human location of a text offset, from the extractor's segments ("slide 3", "sheet 2", …).
fn location(segments: &[Segment], pos: usize) -> Option<String> {
    let s = segments.iter().rev().find(|s| s.start <= pos)?;
    let kind = format!("{:?}", s.kind).to_lowercase();
    Some(if s.from == s.to { format!("{kind} {}", s.from) } else { format!("{kind}s {}-{}", s.from, s.to) })
}

/// Replace every placeholder in `ex.content` with its Markdown reference (or nothing, when the
/// image is skipped), reading package images from `pkg`. Segment offsets are kept correct.
pub fn resolve(ex: &mut Extracted, mut pkg: Option<&mut Package>, images: &mut Images) -> XResult<()> {
    if !ex.content.contains(OPEN) {
        return Ok(());
    }
    let inline = std::mem::take(&mut ex.inline_images);
    let mut inline: Vec<Option<Vec<u8>>> = inline.into_iter().map(Some).collect();
    let src = std::mem::take(&mut ex.content);
    let mut out = String::with_capacity(src.len());
    // (original offset, change in length) for every replaced placeholder, in order.
    let mut shifts: Vec<(usize, isize)> = Vec::new();
    let mut rest = src.as_str();
    let mut consumed = 0usize;
    while let Some(start) = rest.find(OPEN) {
        let Some(end_rel) = rest[start..].find(CLOSE) else { break };
        let end = start + end_rel + CLOSE.len_utf8();
        out.push_str(&rest[..start]);
        let inner = &rest[start + OPEN.len_utf8()..start + end_rel];
        let (key, alt) = inner.split_once(ALT).unwrap_or((inner, ""));
        let bytes = match key.strip_prefix('#') {
            Some(n) => n.parse::<usize>().ok().and_then(|i| inline.get_mut(i)).and_then(Option::take),
            None => match pkg.as_deref_mut() {
                Some(p) => p.read_bytes(key)?,
                None => None,
            },
        };
        let pos = consumed + start;
        let reference =
            bytes.and_then(|b| images.add(b, location(&ex.segments, pos), Some(alt).filter(|a| !a.is_empty()))).unwrap_or_default();
        out.push_str(&reference);
        shifts.push((pos, reference.len() as isize - (end - start) as isize));
        consumed += end;
        rest = &rest[end..];
    }
    out.push_str(rest);
    for seg in &mut ex.segments {
        let delta: isize = shifts.iter().take_while(|(p, _)| *p < seg.start).map(|(_, d)| d).sum();
        seg.start = (seg.start as isize + delta).max(0) as usize;
    }
    ex.content = out;
    Ok(())
}

impl Images {
    /// Hand the collected images and any warnings to the extraction result.
    pub fn finish(mut self, ex: &mut Extracted) {
        ex.warnings.extend(self.warnings());
        ex.images = std::mem::take(&mut self.items);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::SpanKind;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut b = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13, b'I', b'H', b'D', b'R'];
        b.extend(w.to_be_bytes());
        b.extend(h.to_be_bytes());
        b.extend([8, 2, 0, 0, 0, 0, 0, 0, 0]);
        b.resize(400, 0);
        b
    }

    #[test]
    fn placeholders_become_references_and_offsets_follow() {
        let mut ex = Extracted {
            content: format!(
                "## Slide 1\n\nA {}\n\n## Slide 2\n\nB {} {}\n",
                marker("#0", Some("Chart [Q3]")),
                marker("#1", None),
                marker("#2", None)
            ),
            segments: vec![
                Segment { start: 0, kind: SpanKind::Slide, from: 1, to: 1 },
                Segment { start: 0, kind: SpanKind::Slide, from: 2, to: 2 },
            ],
            inline_images: vec![png(100, 50), png(100, 50), png(8, 8)],
            ..Default::default()
        };
        ex.segments[1].start = ex.content.find("## Slide 2").unwrap();
        let mut images = Images::new(true);
        resolve(&mut ex, None, &mut images).unwrap();
        images.finish(&mut ex);
        let id = &ex.images[0].id;
        assert!(ex.content.contains(&format!("A ![Chart Q3](images/{id}.png)")), "{}", ex.content);
        assert!(
            ex.content.contains(&format!("B ![image](images/{id}.png) \n")),
            "a repeat references the same file; the tiny one is dropped: {}",
            ex.content
        );
        assert_eq!(ex.images.len(), 1, "identical images are kept once");
        assert_eq!((ex.images[0].width, ex.images[0].height), (Some(100), Some(50)));
        assert_eq!(ex.images[0].location.as_deref(), Some("slide 1"));
        assert_eq!(&ex.content[ex.segments[1].start..ex.segments[1].start + 10], "## Slide 2", "segment offsets are shifted");
    }

    #[test]
    fn sniffs_types() {
        assert_eq!(sniff(&png(1, 1)), Some("png"));
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("jpg"));
        assert_eq!(sniff(b"<?xml version=\"1.0\"?><svg xmlns=\"x\"/>"), Some("svg"));
        assert_eq!(sniff(b"hello"), None);
    }
}
