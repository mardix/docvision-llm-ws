//! ODT / ODS / ODP (`odf-epub` feature): streaming `content.xml`.

use super::{Extracted, Limits, Package, XResult, attr, capacity_hint, read_dc_title, write_table, xml_reader};
use crate::markdown::{Segment, SpanKind};
use crate::source::Format;
use quick_xml::events::Event;
use std::path::Path;

const MAX_REPEAT: usize = 256;

/// (rows, open cell (text, column repeat), row repeat)
type Table = (Vec<Vec<String>>, Option<(String, usize)>, usize);

pub fn extract(path: &Path, format: Format, limits: &Limits) -> XResult<Extracted> {
    let mut pkg = Package::open(path, limits)?;
    let mut ex = Extracted { title_hint: read_dc_title(&mut pkg, "meta.xml"), ..Default::default() };
    let mut out = String::with_capacity(capacity_hint(path, 2));
    let mut sheets = 0u32;
    let mut slides = 0u32;
    let mut images = 0usize;
    let found = pkg.with_entry("content.xml", |r| {
        let mut x = xml_reader(r);
        let mut buf = Vec::new();
        let mut para: Option<(Option<usize>, String)> = None; // (heading level, text)
        let mut list_depth = 0usize;
        let mut tables: Vec<Table> = Vec::new();
        let mut link: Option<(Option<String>, String)> = None;
        loop {
            let ev = x.read_event_into(&mut buf)?;
            let empty = matches!(ev, Event::Empty(_));
            match ev {
                Event::Start(ref e) | Event::Empty(ref e) => match e.local_name().as_ref() {
                    b"h" if !empty => {
                        para = Some((Some(attr(e, b"outline-level").and_then(|v| v.parse().ok()).unwrap_or(1).clamp(1, 6)), String::new()))
                    }
                    b"p" if !empty => para = Some((None, String::new())),
                    b"s" => {
                        if let Some((_, t)) = para.as_mut() {
                            let n = attr(e, b"c").and_then(|v| v.parse().ok()).unwrap_or(1usize).min(64);
                            t.push_str(&" ".repeat(n));
                        }
                    }
                    b"tab" => {
                        if let Some((_, t)) = para.as_mut() {
                            t.push(' ');
                        }
                    }
                    b"line-break" => {
                        if let Some((_, t)) = para.as_mut() {
                            t.push('\n');
                        }
                    }
                    b"a" if !empty => link = Some((attr(e, b"href"), String::new())),
                    b"list" if !empty => list_depth += 1,
                    b"image" => images += 1,
                    b"page" if !empty && format == Format::Odp => {
                        slides += 1;
                        ex.segments.push(Segment { start: out.len(), kind: SpanKind::Slide, from: slides, to: slides });
                        let name = attr(e, b"name").unwrap_or_default();
                        out.push_str(&format!("## Slide {slides}"));
                        if !name.is_empty() && !name.starts_with("page") {
                            out.push_str(": ");
                            out.push_str(&name);
                        }
                        out.push_str("\n\n");
                    }
                    b"table" if !empty => {
                        if format == Format::Ods && tables.is_empty() {
                            sheets += 1;
                            ex.segments.push(Segment { start: out.len(), kind: SpanKind::Sheet, from: sheets, to: sheets });
                            out.push_str("## ");
                            out.push_str(&attr(e, b"name").unwrap_or_else(|| format!("Sheet {sheets}")));
                            out.push_str("\n\n");
                        }
                        tables.push((Vec::new(), None, 1));
                    }
                    b"table-row" => {
                        if let Some(t) = tables.last_mut() {
                            t.0.push(Vec::new());
                            t.2 = attr(e, b"number-rows-repeated").and_then(|v| v.parse().ok()).unwrap_or(1usize).min(MAX_REPEAT);
                        }
                    }
                    b"table-cell" | b"covered-table-cell" => {
                        if let Some(t) = tables.last_mut() {
                            let rep = attr(e, b"number-columns-repeated").and_then(|v| v.parse().ok()).unwrap_or(1usize).min(MAX_REPEAT);
                            if empty {
                                if let Some(row) = t.0.last_mut() {
                                    row.extend(std::iter::repeat_n(String::new(), rep));
                                }
                            } else {
                                t.1 = Some((String::new(), rep));
                            }
                        }
                    }
                    _ => {}
                },
                ev @ (Event::Text(_) | Event::GeneralRef(_)) => {
                    let s = super::xml_text(&ev);
                    if let Some((_, lt)) = link.as_mut() {
                        lt.push_str(&s);
                    } else if let Some((_, p)) = para.as_mut() {
                        p.push_str(&s);
                    }
                }
                Event::End(e) => match e.local_name().as_ref() {
                    b"a" => {
                        if let (Some((url, text)), Some((_, p))) = (link.take(), para.as_mut()) {
                            match url {
                                Some(u) if !text.trim().is_empty() => p.push_str(&format!("[{text}]({u})")),
                                _ => p.push_str(&text),
                            }
                        }
                    }
                    b"h" | b"p" => {
                        if let Some((level, text)) = para.take() {
                            let text = text.trim();
                            if let Some((c, _)) = tables.last_mut().and_then(|t| t.1.as_mut()) {
                                if !c.is_empty() && !text.is_empty() {
                                    c.push('\n');
                                }
                                c.push_str(text);
                            } else if !text.is_empty() {
                                match level {
                                    Some(l) => out.push_str(&format!("{} {}\n\n", "#".repeat(l), text.replace('\n', " "))),
                                    None if list_depth > 0 => {
                                        out.push_str(&"   ".repeat(list_depth - 1));
                                        out.push_str("- ");
                                        out.push_str(&text.replace('\n', " "));
                                        out.push('\n');
                                    }
                                    None => {
                                        out.push_str(&text.replace('\n', "  \n"));
                                        out.push_str("\n\n");
                                    }
                                }
                            }
                        }
                    }
                    b"list" => {
                        list_depth = list_depth.saturating_sub(1);
                        if list_depth == 0 {
                            out.push('\n');
                        }
                    }
                    b"table-cell" | b"covered-table-cell" => {
                        if let Some(t) = tables.last_mut() {
                            if let (Some((c, rep)), Some(row)) = (t.1.take(), t.0.last_mut()) {
                                let n = if c.is_empty() { rep } else { rep.min(MAX_REPEAT) };
                                row.extend(std::iter::repeat_n(c, n));
                            }
                        }
                    }
                    b"table-row" => {
                        if let Some(t) = tables.last_mut() {
                            if let Some(row) = t.0.last_mut() {
                                while row.last().is_some_and(|c| c.trim().is_empty()) {
                                    row.pop();
                                }
                            }
                            if t.0.last().is_some_and(|r| r.is_empty()) {
                                t.0.pop();
                            } else if t.2 > 1 {
                                let r = t.0.last().cloned().unwrap_or_default();
                                for _ in 1..t.2 {
                                    t.0.push(r.clone());
                                }
                            }
                        }
                    }
                    b"table" => {
                        if let Some((rows, _, _)) = tables.pop() {
                            match tables.last_mut().and_then(|p| p.1.as_mut()) {
                                Some((c, _)) => {
                                    for r in rows {
                                        c.push_str(&r.join(" / "));
                                        c.push('\n');
                                    }
                                }
                                None if rows.is_empty() => out.push_str("_(empty table)_\n\n"),
                                None => write_table(&mut out, &rows),
                            }
                        }
                    }
                    _ => {}
                },
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }
        Ok(())
    })?;
    if found.is_none() {
        return Err(super::ExtractError::Corrupt("ODF package is missing content.xml".into()));
    }
    if images > 0 {
        ex.warnings.push(format!("{images} image(s) were not transcribed"));
    }
    match format {
        Format::Ods => ex.sheet_count = Some(sheets),
        Format::Odp => {
            ex.slide_count = Some(slides);
            ex.pages = Some(super::PageCount { total: slides, method: "slide_count", exact: true });
        }
        _ => {}
    }
    ex.content = out;
    Ok(ex)
}
