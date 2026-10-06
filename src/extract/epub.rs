//! EPUB (`odf-epub` feature): container.xml -> OPF -> spine order -> XHTML via the HTML converter.

use super::{Extracted, Limits, Package, XResult, attr, resolve, xml_reader};
use quick_xml::events::Event;
use std::collections::HashMap;
use std::path::Path;

pub fn extract(path: &Path, limits: &Limits) -> XResult<Extracted> {
    let mut pkg = Package::open(path, limits)?;
    let mut opf_path = None;
    pkg.with_entry("META-INF/container.xml", |r| {
        let mut x = xml_reader(r);
        let mut buf = Vec::new();
        loop {
            match x.read_event_into(&mut buf)? {
                Event::Start(e) | Event::Empty(e) if e.local_name().as_ref() == b"rootfile" => {
                    opf_path = attr(&e, b"full-path");
                    break;
                }
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }
        Ok(())
    })?;
    let opf_path = opf_path.ok_or_else(|| super::ExtractError::Corrupt("EPUB has no rootfile".into()))?;
    let opf_dir = opf_path.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default();
    let mut manifest: HashMap<String, (String, String)> = HashMap::new();
    let mut spine: Vec<String> = Vec::new();
    let mut title = String::new();
    pkg.with_entry(&opf_path, |r| {
        let mut x = xml_reader(r);
        let mut buf = Vec::new();
        let mut in_title = false;
        loop {
            match x.read_event_into(&mut buf)? {
                Event::Start(e) | Event::Empty(e) => match e.local_name().as_ref() {
                    b"item" => {
                        if let (Some(id), Some(href)) = (attr(&e, b"id"), attr(&e, b"href")) {
                            manifest.insert(id, (href, attr(&e, b"media-type").unwrap_or_default()));
                        }
                    }
                    b"itemref" => spine.extend(attr(&e, b"idref")),
                    b"title" if title.is_empty() => in_title = true,
                    _ => {}
                },
                ev @ (Event::Text(_) | Event::GeneralRef(_)) if in_title => title.push_str(&super::xml_text(&ev)),
                Event::End(e) if e.local_name().as_ref() == b"title" => in_title = false,
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }
        Ok(())
    })?
    .ok_or_else(|| super::ExtractError::Corrupt("EPUB package document missing".into()))?;

    let mut ex = Extracted { title_hint: Some(title.trim().to_string()).filter(|t| !t.is_empty()), ..Default::default() };
    let mut out = String::new();
    for idref in spine {
        let Some((href, media)) = manifest.get(&idref) else { continue };
        if !media.contains("html") {
            continue;
        }
        let part = resolve(&opf_dir, &percent_encoding::percent_decode_str(href).decode_utf8_lossy());
        if let Some(html) = pkg.read_string(&part)? {
            let c = super::html::convert(&html);
            out.push_str(c.content.trim());
            out.push_str("\n\n");
            ex.warnings.extend(c.warnings);
        }
    }
    ex.content = out;
    Ok(ex)
}
