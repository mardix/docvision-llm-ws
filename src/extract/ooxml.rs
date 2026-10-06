//! DOCX, XLSX and PPTX via streaming `quick-xml` over lazily read ZIP entries.
//! Macros and external relationships are never executed or fetched.

use super::{
    Extracted, Limits, Package, XResult, attr, attr_q, capacity_hint, read_dc_title, read_rels, resolve, write_row, write_separator,
    write_table, xml_reader,
};
use crate::markdown::{Segment, SpanKind};
use quick_xml::events::Event;
use std::collections::HashMap;
use std::path::Path;

// ---------------------------------------------------------------- DOCX

struct DocxStyles {
    heading: HashMap<String, usize>,
}

fn docx_styles(pkg: &mut Package) -> XResult<DocxStyles> {
    let mut heading = HashMap::new();
    pkg.with_entry("word/styles.xml", |r| {
        let mut x = xml_reader(r);
        let mut buf = Vec::new();
        let mut cur: Option<String> = None;
        loop {
            match x.read_event_into(&mut buf)? {
                Event::Start(e) if e.local_name().as_ref() == b"style" => cur = attr(&e, b"styleId"),
                Event::End(e) if e.local_name().as_ref() == b"style" => cur = None,
                Event::Start(e) | Event::Empty(e) => {
                    if let Some(id) = &cur {
                        match e.local_name().as_ref() {
                            b"name" => {
                                let n = attr(&e, b"val").unwrap_or_default().to_ascii_lowercase();
                                if n == "title" {
                                    heading.insert(id.clone(), 1);
                                } else if let Some(l) = n.strip_prefix("heading ").and_then(|l| l.trim().parse::<usize>().ok()) {
                                    heading.insert(id.clone(), l.clamp(1, 6));
                                }
                            }
                            b"outlineLvl" => {
                                if let Some(l) = attr(&e, b"val").and_then(|v| v.parse::<usize>().ok()) {
                                    if l < 9 {
                                        heading.insert(id.clone(), (l + 1).min(6));
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }
        Ok(())
    })?;
    Ok(DocxStyles { heading })
}

/// (numId, ilvl) -> ordered list?
fn docx_numbering(pkg: &mut Package) -> XResult<HashMap<(String, u32), bool>> {
    let mut abstract_fmt: HashMap<(String, u32), bool> = HashMap::new();
    let mut num_to_abs: HashMap<String, String> = HashMap::new();
    pkg.with_entry("word/numbering.xml", |r| {
        let mut x = xml_reader(r);
        let mut buf = Vec::new();
        let mut abs: Option<String> = None;
        let mut lvl: u32 = 0;
        let mut num: Option<String> = None;
        loop {
            match x.read_event_into(&mut buf)? {
                Event::Start(e) | Event::Empty(e) => match e.local_name().as_ref() {
                    b"abstractNum" => abs = attr(&e, b"abstractNumId"),
                    b"lvl" => lvl = attr(&e, b"ilvl").and_then(|v| v.parse().ok()).unwrap_or(0),
                    b"numFmt" => {
                        if let Some(a) = &abs {
                            let fmt = attr(&e, b"val").unwrap_or_default();
                            abstract_fmt.insert((a.clone(), lvl), !matches!(fmt.as_str(), "bullet" | "none" | ""));
                        }
                    }
                    b"num" => num = attr(&e, b"numId"),
                    b"abstractNumId" => {
                        if let (Some(n), Some(a)) = (&num, attr(&e, b"val")) {
                            num_to_abs.insert(n.clone(), a);
                        }
                    }
                    _ => {}
                },
                Event::End(e) if e.local_name().as_ref() == b"abstractNum" => abs = None,
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }
        Ok(())
    })?;
    let mut out = HashMap::new();
    for (num, abs) in num_to_abs {
        for lvl in 0..9 {
            if let Some(o) = abstract_fmt.get(&(abs.clone(), lvl)) {
                out.insert((num.clone(), lvl), *o);
            }
        }
    }
    Ok(out)
}

#[derive(Default)]
struct Para {
    text: String,
    style_heading: Option<usize>,
    outline: Option<usize>,
    num_id: Option<String>,
    ilvl: u32,
}

struct Table {
    rows: Vec<Vec<String>>,
    cell: Option<String>,
}

pub fn docx(path: &Path, limits: &Limits) -> XResult<Extracted> {
    let mut pkg = Package::open(path, limits)?;
    let styles = docx_styles(&mut pkg)?;
    let numbering = docx_numbering(&mut pkg)?;
    let rels = read_rels(&mut pkg, "word/_rels/document.xml.rels")?;
    let mut ex = Extracted { title_hint: read_dc_title(&mut pkg, "docProps/core.xml"), ..Default::default() };
    let mut out = String::with_capacity(capacity_hint(path, 2));
    let mut images = 0usize;
    let found = pkg.with_entry("word/document.xml", |r| {
        let mut x = xml_reader(r);
        let mut buf = Vec::new();
        let mut para: Option<Para> = None;
        let mut tables: Vec<Table> = Vec::new();
        let mut run_bold = false;
        let mut run_italic = false;
        let mut run_text = String::new();
        let mut in_rpr = false;
        let mut in_text = false;
        let mut link: Option<(Option<String>, String)> = None; // (url, text)
        let mut ordinals: HashMap<(String, u32), u32> = HashMap::new();
        loop {
            let ev = x.read_event_into(&mut buf)?;
            match ev {
                Event::Start(ref e) | Event::Empty(ref e) => {
                    let empty = matches!(ev, Event::Empty(_));
                    match e.local_name().as_ref() {
                        b"p" if !empty => para = Some(Para::default()),
                        b"pStyle" => {
                            if let (Some(p), Some(v)) = (para.as_mut(), attr(e, b"val")) {
                                p.style_heading = styles.heading.get(&v).copied();
                            }
                        }
                        b"outlineLvl" => {
                            if let (Some(p), Some(v)) = (para.as_mut(), attr(e, b"val").and_then(|v| v.parse::<usize>().ok())) {
                                if v < 9 {
                                    p.outline = Some((v + 1).min(6));
                                }
                            }
                        }
                        b"ilvl" => {
                            if let Some(p) = para.as_mut() {
                                p.ilvl = attr(e, b"val").and_then(|v| v.parse().ok()).unwrap_or(0);
                            }
                        }
                        b"numId" => {
                            if let Some(p) = para.as_mut() {
                                p.num_id = attr(e, b"val").filter(|v| v != "0");
                            }
                        }
                        b"r" if !empty => {
                            run_bold = false;
                            run_italic = false;
                            run_text.clear();
                        }
                        b"rPr" if !empty => in_rpr = true,
                        b"b" if in_rpr => run_bold = attr(e, b"val").is_none_or(|v| v != "0" && v != "false"),
                        b"i" if in_rpr => run_italic = attr(e, b"val").is_none_or(|v| v != "0" && v != "false"),
                        b"t" if !empty => in_text = true,
                        b"tab" if para.is_some() => run_text.push(' '),
                        b"br" | b"cr" if para.is_some() => run_text.push('\n'),
                        b"hyperlink" if !empty => {
                            let url = attr_q(e, b"r:id").and_then(|id| rels.get(&id).map(|(t, _)| t.clone()));
                            link = Some((url, String::new()));
                        }
                        b"drawing" | b"pict" => images += 1,
                        b"tbl" if !empty => tables.push(Table { rows: Vec::new(), cell: None }),
                        b"tr" if !empty => {
                            if let Some(t) = tables.last_mut() {
                                t.rows.push(Vec::new());
                            }
                        }
                        b"tc" if !empty => {
                            if let Some(t) = tables.last_mut() {
                                t.cell = Some(String::new());
                            }
                        }
                        _ => {}
                    }
                }
                ev @ (Event::Text(_) | Event::GeneralRef(_)) if in_text => run_text.push_str(&super::xml_text(&ev)),
                Event::End(e) => match e.local_name().as_ref() {
                    b"t" => in_text = false,
                    b"rPr" => in_rpr = false,
                    b"r" => {
                        let text = std::mem::take(&mut run_text);
                        let styled = if !text.trim().is_empty() && (run_bold || run_italic) {
                            let m = match (run_bold, run_italic) {
                                (true, true) => "***",
                                (true, false) => "**",
                                _ => "*",
                            };
                            let lead = &text[..text.len() - text.trim_start().len()];
                            let trail = &text[text.trim_end().len()..];
                            format!("{lead}{m}{}{m}{trail}", text.trim())
                        } else {
                            text
                        };
                        if let Some((_, lt)) = link.as_mut() {
                            lt.push_str(&styled);
                        } else if let Some(p) = para.as_mut() {
                            p.text.push_str(&styled);
                        }
                    }
                    b"hyperlink" => {
                        if let (Some((url, text)), Some(p)) = (link.take(), para.as_mut()) {
                            match url {
                                Some(u) if !text.trim().is_empty() => {
                                    p.text.push('[');
                                    p.text.push_str(&text);
                                    p.text.push_str("](");
                                    p.text.push_str(&u.replace(' ', "%20"));
                                    p.text.push(')');
                                }
                                _ => p.text.push_str(&text),
                            }
                        }
                    }
                    b"p" => {
                        if let Some(p) = para.take() {
                            let text = p.text.trim();
                            if let Some(t) = tables.last_mut().filter(|t| t.cell.is_some()) {
                                let c = t.cell.as_mut().unwrap();
                                if !c.is_empty() && !text.is_empty() {
                                    c.push('\n');
                                }
                                c.push_str(text);
                            } else if !text.is_empty() {
                                if let Some(level) = p.outline.or(p.style_heading) {
                                    out.push_str(&"#".repeat(level));
                                    out.push(' ');
                                    out.push_str(&text.replace('\n', " "));
                                    out.push_str("\n\n");
                                } else if let Some(n) = &p.num_id {
                                    let ordered = numbering.get(&(n.clone(), p.ilvl)).copied().unwrap_or(false);
                                    out.push_str(&"   ".repeat(p.ilvl as usize));
                                    if ordered {
                                        let k = ordinals.entry((n.clone(), p.ilvl)).or_insert(0);
                                        *k += 1;
                                        out.push_str(&format!("{k}. "));
                                    } else {
                                        out.push_str("- ");
                                    }
                                    out.push_str(&text.replace('\n', " "));
                                    out.push('\n');
                                } else {
                                    if out.ends_with('\n') && !out.ends_with("\n\n") && !out.is_empty() {
                                        out.push('\n'); // close a preceding list
                                    }
                                    out.push_str(&text.replace('\n', "  \n"));
                                    out.push_str("\n\n");
                                }
                            }
                        }
                    }
                    b"tc" => {
                        if let Some(t) = tables.last_mut() {
                            if let (Some(c), Some(row)) = (t.cell.take(), t.rows.last_mut()) {
                                row.push(c);
                            }
                        }
                    }
                    b"tbl" => {
                        if let Some(t) = tables.pop() {
                            if let Some(parent) = tables.last_mut().and_then(|p| p.cell.as_mut()) {
                                // Nested table: flatten into the parent cell.
                                for row in &t.rows {
                                    if !parent.is_empty() {
                                        parent.push('\n');
                                    }
                                    parent.push_str(&row.join(" / "));
                                }
                            } else {
                                if !out.is_empty() && !out.ends_with("\n\n") {
                                    out.push('\n');
                                }
                                write_table(&mut out, &t.rows);
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
        return Err(super::ExtractError::Corrupt("DOCX is missing word/document.xml".into()));
    }
    if images > 0 {
        ex.warnings.push(format!("{images} embedded image(s) or drawing(s) were not transcribed"));
    }
    if pkg.has("word/vbaProject.bin") {
        ex.warnings.push("document contains macros; they were ignored".into());
    }
    ex.content = out;
    Ok(ex)
}

// ---------------------------------------------------------------- XLSX

fn col_index(cell_ref: &str) -> Option<usize> {
    let mut n = 0usize;
    let mut any = false;
    for b in cell_ref.bytes() {
        if b.is_ascii_alphabetic() {
            n = n * 26 + (b.to_ascii_uppercase() - b'A' + 1) as usize;
            any = true;
        } else {
            break;
        }
    }
    any.then(|| n - 1)
}

/// All shared strings in one buffer with (start, end) offsets: no per-string allocation.
#[derive(Default)]
struct SharedStrings {
    buf: String,
    idx: Vec<(u32, u32)>,
}

impl SharedStrings {
    fn get(&self, i: usize) -> &str {
        self.idx.get(i).map(|&(a, b)| &self.buf[a as usize..b as usize]).unwrap_or("")
    }
}

fn shared_strings(pkg: &mut Package) -> XResult<SharedStrings> {
    let mut out = SharedStrings::default();
    pkg.with_entry("xl/sharedStrings.xml", |r| {
        let mut x = xml_reader(r);
        let mut buf = Vec::new();
        let mut start = 0usize;
        let mut in_t = false;
        let mut in_rph = false;
        loop {
            match x.read_event_into(&mut buf)? {
                Event::Start(e) => match e.local_name().as_ref() {
                    b"si" => start = out.buf.len(),
                    b"t" => in_t = true,
                    b"rPh" => in_rph = true,
                    _ => {}
                },
                Event::Empty(e) if e.local_name().as_ref() == b"si" => out.idx.push((out.buf.len() as u32, out.buf.len() as u32)),
                ev @ (Event::Text(_) | Event::GeneralRef(_)) if in_t && !in_rph => out.buf.push_str(&super::xml_text(&ev)),
                Event::End(e) => match e.local_name().as_ref() {
                    b"si" => out.idx.push((start as u32, out.buf.len() as u32)),
                    b"t" => in_t = false,
                    b"rPh" => in_rph = false,
                    _ => {}
                },
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }
        Ok(())
    })?;
    out.buf.shrink_to_fit();
    Ok(out)
}

pub fn xlsx(path: &Path, limits: &Limits) -> XResult<Extracted> {
    let mut pkg = Package::open(path, limits)?;
    let strings = shared_strings(&mut pkg)?;
    let rels = read_rels(&mut pkg, "xl/_rels/workbook.xml.rels")?;
    let mut sheets: Vec<(String, String)> = Vec::new();
    pkg.with_entry("xl/workbook.xml", |r| {
        let mut x = xml_reader(r);
        let mut buf = Vec::new();
        loop {
            match x.read_event_into(&mut buf)? {
                Event::Start(e) | Event::Empty(e) if e.local_name().as_ref() == b"sheet" => {
                    let name = attr(&e, b"name").unwrap_or_default();
                    if let Some((target, _)) = attr_q(&e, b"r:id").and_then(|id| rels.get(&id)) {
                        sheets.push((name, resolve("xl", target)));
                    }
                }
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }
        Ok(())
    })?
    .ok_or_else(|| super::ExtractError::Corrupt("XLSX is missing xl/workbook.xml".into()))?;

    let mut ex = Extracted { title_hint: read_dc_title(&mut pkg, "docProps/core.xml"), ..Default::default() };
    let mut out = String::with_capacity(capacity_hint(path, 3));
    let mut formulas_without_cache = 0usize;
    for (i, (name, part)) in sheets.iter().enumerate() {
        ex.segments.push(Segment { start: out.len(), kind: SpanKind::Sheet, from: i as u32 + 1, to: i as u32 + 1 });
        out.push_str("## ");
        out.push_str(if name.is_empty() { "Sheet" } else { name });
        out.push_str("\n\n");
        // Rows stream straight into the output when the sheet declares its <dimension>;
        // otherwise they are buffered to find the column count.
        let mut rows: Vec<Vec<String>> = Vec::new();
        let mut cols: Option<usize> = None;
        let mut wrote_header = false;
        pkg.with_entry(part, |r| {
            let mut x = xml_reader(r);
            let mut buf = Vec::new();
            let mut row: Vec<String> = Vec::new();
            let mut col = 0usize;
            let mut ty = String::new();
            let mut val = String::new();
            let mut in_v = false;
            let mut has_formula = false;
            loop {
                match x.read_event_into(&mut buf)? {
                    Event::Start(e) => match e.local_name().as_ref() {
                        b"row" => {
                            row.clear();
                            col = 0;
                        }
                        b"c" => {
                            col = attr(&e, b"r").and_then(|r| col_index(&r)).unwrap_or(col);
                            ty = attr(&e, b"t").unwrap_or_default();
                            val.clear();
                            has_formula = false;
                        }
                        b"v" | b"t" => in_v = true,
                        b"f" => has_formula = true,
                        _ => {}
                    },
                    Event::Empty(e) => match e.local_name().as_ref() {
                        b"dimension" => {
                            cols = attr(&e, b"ref")
                                .and_then(|r| r.rsplit(':').next().and_then(col_index))
                                .map(|c| c + 1)
                                .filter(|c| *c <= 16_384);
                        }
                        b"c" => col = attr(&e, b"r").and_then(|r| col_index(&r)).unwrap_or(col) + 1,
                        b"f" => has_formula = true,
                        _ => {}
                    },
                    ev @ (Event::Text(_) | Event::GeneralRef(_)) if in_v => val.push_str(&super::xml_text(&ev)),
                    Event::End(e) => match e.local_name().as_ref() {
                        b"v" | b"t" => in_v = false,
                        b"c" => {
                            let text = match ty.as_str() {
                                "s" => val.trim().parse::<usize>().map(|i| strings.get(i).to_string()).unwrap_or_default(),
                                "b" => {
                                    if val.trim() == "1" {
                                        "TRUE".into()
                                    } else {
                                        "FALSE".into()
                                    }
                                }
                                _ => std::mem::take(&mut val),
                            };
                            if has_formula && text.is_empty() {
                                formulas_without_cache += 1;
                            }
                            if col < 16_384 {
                                if row.len() <= col {
                                    row.resize(col + 1, String::new());
                                }
                                row[col] = text;
                            }
                            col += 1;
                        }
                        b"row" => {
                            if row.iter().any(|c| !c.trim().is_empty()) {
                                while row.last().is_some_and(|c| c.trim().is_empty()) {
                                    row.pop();
                                }
                                match cols {
                                    Some(n) => {
                                        write_row(&mut out, &row, n);
                                        if !wrote_header {
                                            write_separator(&mut out, n.max(row.len()));
                                            wrote_header = true;
                                        }
                                        row.clear();
                                    }
                                    None => rows.push(std::mem::take(&mut row)),
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
        if wrote_header {
            out.push('\n');
        } else if rows.is_empty() {
            out.push_str("_(empty sheet)_\n\n");
        } else {
            write_table(&mut out, &rows);
        }
    }
    if formulas_without_cache > 0 {
        ex.warnings.push(format!("{formulas_without_cache} formula cell(s) had no cached value"));
    }
    if pkg.has("xl/vbaProject.bin") {
        ex.warnings.push("workbook contains macros; they were ignored".into());
    }
    ex.sheet_count = Some(sheets.len() as u32);
    ex.content = out;
    Ok(ex)
}

// ---------------------------------------------------------------- PPTX

pub fn pptx(path: &Path, limits: &Limits) -> XResult<Extracted> {
    let mut pkg = Package::open(path, limits)?;
    let rels = read_rels(&mut pkg, "ppt/_rels/presentation.xml.rels")?;
    let mut slides: Vec<String> = Vec::new();
    pkg.with_entry("ppt/presentation.xml", |r| {
        let mut x = xml_reader(r);
        let mut buf = Vec::new();
        loop {
            match x.read_event_into(&mut buf)? {
                Event::Start(e) | Event::Empty(e) if e.local_name().as_ref() == b"sldId" => {
                    if let Some((t, _)) = attr_q(&e, b"r:id").and_then(|id| rels.get(&id)) {
                        slides.push(resolve("ppt", t));
                    }
                }
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }
        Ok(())
    })?
    .ok_or_else(|| super::ExtractError::Corrupt("PPTX is missing ppt/presentation.xml".into()))?;

    let mut ex = Extracted { title_hint: read_dc_title(&mut pkg, "docProps/core.xml"), ..Default::default() };
    let mut out = String::with_capacity(capacity_hint(path, 1));
    let mut pictures = 0usize;
    for (i, part) in slides.iter().enumerate() {
        let n = i as u32 + 1;
        ex.segments.push(Segment { start: out.len(), kind: SpanKind::Slide, from: n, to: n });
        let mut title = String::new();
        let mut body = String::new();
        pkg.with_entry(part, |r| {
            let mut x = xml_reader(r);
            let mut buf = Vec::new();
            let mut is_title = false;
            let mut para = String::new();
            let mut lvl = 0usize;
            let mut in_t = false;
            let mut table: Option<Vec<Vec<String>>> = None;
            let mut cell: Option<String> = None;
            loop {
                match x.read_event_into(&mut buf)? {
                    Event::Start(e) | Event::Empty(e) => match e.local_name().as_ref() {
                        b"sp" => is_title = false,
                        b"ph" => is_title = matches!(attr(&e, b"type").as_deref(), Some("title" | "ctrTitle")),
                        b"p" => {
                            para.clear();
                            lvl = 0;
                        }
                        b"pPr" => lvl = attr(&e, b"lvl").and_then(|v| v.parse().ok()).unwrap_or(0),
                        b"t" => in_t = true,
                        b"br" => para.push('\n'),
                        b"pic" => pictures += 1,
                        b"tbl" => table = Some(Vec::new()),
                        b"tr" => {
                            if let Some(t) = table.as_mut() {
                                t.push(Vec::new());
                            }
                        }
                        b"tc" if table.is_some() => cell = Some(String::new()),
                        _ => {}
                    },
                    ev @ (Event::Text(_) | Event::GeneralRef(_)) if in_t => para.push_str(&super::xml_text(&ev)),
                    Event::End(e) => match e.local_name().as_ref() {
                        b"t" => in_t = false,
                        b"p" => {
                            let text = para.trim();
                            if text.is_empty() {
                            } else if let Some(c) = cell.as_mut() {
                                if !c.is_empty() {
                                    c.push('\n');
                                }
                                c.push_str(text);
                            } else if is_title {
                                if !title.is_empty() {
                                    title.push(' ');
                                }
                                title.push_str(&text.replace('\n', " "));
                            } else {
                                body.push_str(&"  ".repeat(lvl));
                                body.push_str("- ");
                                body.push_str(&text.replace('\n', " "));
                                body.push('\n');
                            }
                        }
                        b"tc" => {
                            if let (Some(c), Some(row)) = (cell.take(), table.as_mut().and_then(|t| t.last_mut())) {
                                row.push(c);
                            }
                        }
                        b"tbl" => {
                            if let Some(rows) = table.take() {
                                body.push('\n');
                                write_table(&mut body, &rows);
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
        out.push_str(&format!("## Slide {n}"));
        if !title.is_empty() {
            out.push_str(": ");
            out.push_str(&title);
        }
        out.push_str("\n\n");
        if !body.is_empty() {
            out.push_str(body.trim_end());
            out.push_str("\n\n");
        }
    }
    if pictures > 0 {
        ex.warnings.push(format!("{pictures} picture(s) were not transcribed"));
    }
    if pkg.has("ppt/vbaProject.bin") {
        ex.warnings.push("presentation contains macros; they were ignored".into());
    }
    ex.slide_count = Some(slides.len() as u32);
    ex.pages = Some(super::PageCount { total: slides.len() as u32, method: "slide_count", exact: true });
    ex.content = out;
    Ok(ex)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn columns() {
        assert_eq!(col_index("A1"), Some(0));
        assert_eq!(col_index("Z9"), Some(25));
        assert_eq!(col_index("AA10"), Some(26));
        assert_eq!(col_index("12"), None);
    }
}
