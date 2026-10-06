//! HTML -> Markdown using the zero-copy `tl` parser. Scripts and styles are stripped and
//! never executed; external resources are never fetched.

use super::{ExtractError, Extracted, XResult, write_table};
use std::path::Path;
use tl::{Node, NodeHandle, Parser};

const MAX_DEPTH: usize = 128;

pub fn extract_file(path: &Path) -> XResult<Extracted> {
    let bytes = std::fs::read(path).map_err(|e| ExtractError::Corrupt(e.to_string()))?;
    let html = String::from_utf8_lossy(&bytes);
    Ok(convert(&html))
}

/// Remove `<script>`, `<style>`, `<noscript>`, `<template>` blocks and comments, case-insensitively.
fn strip_blocks(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len());
    let mut i = 0;
    'outer: while i < html.len() {
        let rest = &lower[i..];
        if rest.starts_with("<!--") {
            match rest.find("-->") {
                Some(end) => {
                    i += end + 3;
                    continue;
                }
                None => break,
            }
        }
        for tag in ["script", "style", "noscript", "template"] {
            if rest.starts_with('<')
                && rest[1..].starts_with(tag)
                && rest[1 + tag.len()..].starts_with(|c: char| c == '>' || c.is_whitespace())
            {
                let close = format!("</{tag}");
                match rest.find(&close) {
                    Some(end) => {
                        let after = rest[end..].find('>').map(|g| end + g + 1).unwrap_or(rest.len());
                        i += after;
                    }
                    None => i = html.len(),
                }
                continue 'outer;
            }
        }
        let ch = html[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find('&') {
        out.push_str(&rest[..p]);
        rest = &rest[p..];
        let end = rest[1..].find(|c: char| c == ';' || c == '&' || c.is_whitespace()).map(|e| e + 1);
        let decoded = end.filter(|e| rest.as_bytes().get(*e) == Some(&b';')).and_then(|e| {
            let name = &rest[1..e];
            let c = if let Some(num) = name.strip_prefix('#') {
                let n = if let Some(h) = num.strip_prefix(['x', 'X']) { u32::from_str_radix(h, 16).ok() } else { num.parse().ok() };
                n.and_then(char::from_u32)
            } else {
                match name {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    "nbsp" => Some(' '),
                    "mdash" => Some('—'),
                    "ndash" => Some('–'),
                    "hellip" => Some('…'),
                    "copy" => Some('©'),
                    "reg" => Some('®'),
                    "rsquo" => Some('’'),
                    "lsquo" => Some('‘'),
                    "rdquo" => Some('”'),
                    "ldquo" => Some('“'),
                    "euro" => Some('€'),
                    _ => None,
                }
            };
            c.map(|c| (c, e + 1))
        });
        match decoded {
            Some((c, len)) => {
                out.push(c);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

struct Ctx<'p, 'a> {
    parser: &'p Parser<'a>,
    out: String,
    line: String,
    lists: Vec<(bool, u32)>,
    pre: usize,
    title: Option<String>,
    images: usize,
    table: Option<(Vec<Vec<String>>, Option<String>)>,
}

impl Ctx<'_, '_> {
    fn flush(&mut self) {
        let t = self.line.trim();
        if !t.is_empty() {
            if let Some((_, Some(c))) = self.table.as_mut() {
                if !c.is_empty() {
                    c.push(' ');
                }
                c.push_str(t);
            } else {
                self.out.push_str(t);
                self.out.push_str("\n\n");
            }
        }
        self.line.clear();
    }

    fn text(&mut self, raw: &str) {
        let t = decode_entities(raw);
        if self.pre > 0 {
            self.line.push_str(&t);
            return;
        }
        let mut last_space = self.line.ends_with(' ') || self.line.is_empty();
        for c in t.chars() {
            if c.is_whitespace() {
                if !last_space {
                    self.line.push(' ');
                    last_space = true;
                }
            } else {
                self.line.push(c);
                last_space = false;
            }
        }
    }

    fn children(&mut self, h: &NodeHandle, depth: usize) {
        let Some(Node::Tag(tag)) = h.get(self.parser) else { return };
        for c in tag.children().top().iter() {
            self.node(c, depth + 1);
        }
    }

    fn inner(&mut self, h: &NodeHandle, depth: usize) -> String {
        let saved = std::mem::take(&mut self.line);
        self.children(h, depth);
        std::mem::replace(&mut self.line, saved)
    }

    fn node(&mut self, h: &NodeHandle, depth: usize) {
        let Some(node) = h.get(self.parser) else { return };
        let tag = match node {
            Node::Raw(b) => {
                self.text(&b.as_utf8_str());
                return;
            }
            Node::Comment(_) => return,
            Node::Tag(t) => t,
        };
        if depth > MAX_DEPTH {
            // Pathologically deep markup: flatten to text iteratively (no recursion).
            let mut stack: Vec<NodeHandle> = tag.children().top().as_slice().iter().rev().copied().collect();
            let mut text = String::new();
            while let Some(h) = stack.pop() {
                match h.get(self.parser) {
                    Some(Node::Raw(b)) => {
                        text.push_str(&b.as_utf8_str());
                        text.push(' ');
                    }
                    Some(Node::Tag(t)) => stack.extend(t.children().top().as_slice().iter().rev().copied()),
                    _ => {}
                }
            }
            self.text(&text);
            return;
        }
        let name = tag.name().as_utf8_str().to_ascii_lowercase();
        match name.as_str() {
            "head" => {
                if let Some(t) =
                    tag.find_node(self.parser, &mut |n| n.as_tag().is_some_and(|t| t.name().as_utf8_str().eq_ignore_ascii_case("title")))
                {
                    if let Some(Node::Tag(tt)) = t.get(self.parser) {
                        let s = decode_entities(tt.inner_text(self.parser).trim());
                        if !s.is_empty() {
                            self.title = Some(s);
                        }
                    }
                }
            }
            "script" | "style" | "noscript" | "template" | "svg" | "iframe" | "object" | "embed" | "canvas" | "button" | "select" => {}
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.flush();
                let level = (name.as_bytes()[1] - b'0') as usize;
                let text = self.inner(h, depth);
                let text = text.trim();
                if !text.is_empty() {
                    if self.table.as_ref().is_some_and(|t| t.1.is_some()) {
                        self.line.push_str(text);
                        self.flush();
                    } else {
                        self.out.push_str(&"#".repeat(level));
                        self.out.push(' ');
                        self.out.push_str(text);
                        self.out.push_str("\n\n");
                    }
                }
            }
            "p" | "div" | "section" | "article" | "header" | "footer" | "main" | "nav" | "aside" | "figure" | "figcaption" | "address"
            | "form" | "fieldset" | "dl" | "dt" | "dd" | "body" | "html" | "details" | "summary" => {
                self.flush();
                self.children(h, depth);
                self.flush();
            }
            "br" => {
                if self.pre > 0 {
                    self.line.push('\n');
                } else {
                    self.line.push_str("  \n");
                }
            }
            "hr" => {
                self.flush();
                self.out.push_str("---\n\n");
            }
            "pre" => {
                self.flush();
                self.pre += 1;
                let text = self.inner(h, depth);
                self.pre -= 1;
                let text = text.trim_matches('\n');
                let fence = if text.contains("```") { "~~~" } else { "```" };
                self.out.push_str(fence);
                self.out.push('\n');
                self.out.push_str(text);
                self.out.push('\n');
                self.out.push_str(fence);
                self.out.push_str("\n\n");
            }
            "code" if self.pre == 0 => {
                let text = self.inner(h, depth);
                if !text.trim().is_empty() {
                    let tick = if text.contains('`') { "``" } else { "`" };
                    self.line.push_str(tick);
                    self.line.push_str(text.trim());
                    self.line.push_str(tick);
                }
            }
            "strong" | "b" | "em" | "i" => {
                let m = if matches!(name.as_str(), "strong" | "b") { "**" } else { "*" };
                let text = self.inner(h, depth);
                let t = text.trim();
                if !t.is_empty() {
                    if text.starts_with(' ') && !self.line.ends_with(' ') {
                        self.line.push(' ');
                    }
                    self.line.push_str(m);
                    self.line.push_str(t);
                    self.line.push_str(m);
                    if text.ends_with(' ') {
                        self.line.push(' ');
                    }
                }
            }
            "a" => {
                let href = tag.attributes().get("href").flatten().map(|b| decode_entities(&b.as_utf8_str()));
                let text = self.inner(h, depth);
                let t = text.trim();
                match href.filter(|u| !u.starts_with("javascript:") && !u.is_empty()) {
                    Some(u) if !t.is_empty() && !u.starts_with('#') => {
                        self.line.push('[');
                        self.line.push_str(t);
                        self.line.push_str("](");
                        self.line.push_str(&u.replace(' ', "%20"));
                        self.line.push(')');
                    }
                    _ => self.line.push_str(t),
                }
            }
            "img" => {
                self.images += 1;
                if let Some(alt) = tag.attributes().get("alt").flatten() {
                    let alt = decode_entities(alt.as_utf8_str().trim());
                    if !alt.is_empty() {
                        self.line.push_str(&format!("[image: {alt}]"));
                    }
                }
            }
            "ul" | "ol" => {
                self.flush();
                self.lists.push((name == "ol", 0));
                self.children(h, depth);
                self.lists.pop();
                if self.lists.is_empty() {
                    self.out.push('\n');
                }
            }
            "li" => {
                self.flush();
                let indent = "   ".repeat(self.lists.len().saturating_sub(1));
                let marker = match self.lists.last_mut() {
                    Some((true, n)) => {
                        *n += 1;
                        format!("{n}. ")
                    }
                    _ => "- ".to_string(),
                };
                let saved = std::mem::take(&mut self.out);
                self.children(h, depth);
                self.flush();
                let body = std::mem::replace(&mut self.out, saved);
                let mut lines = body.trim().lines().filter(|l| !l.trim().is_empty());
                self.out.push_str(&indent);
                self.out.push_str(&marker);
                self.out.push_str(lines.next().unwrap_or(""));
                self.out.push('\n');
                for l in lines {
                    if !l.starts_with(' ') {
                        self.out.push_str(&indent);
                        self.out.push_str("   ");
                    }
                    self.out.push_str(l);
                    self.out.push('\n');
                }
            }
            "blockquote" => {
                self.flush();
                let saved = std::mem::take(&mut self.out);
                self.children(h, depth);
                self.flush();
                let body = std::mem::replace(&mut self.out, saved);
                for l in body.trim().lines() {
                    self.out.push_str("> ");
                    self.out.push_str(l);
                    self.out.push('\n');
                }
                self.out.push('\n');
            }
            "table" => {
                self.flush();
                let outer = self.table.take();
                self.table = Some((Vec::new(), None));
                self.children(h, depth);
                let (rows, _) = self.table.take().unwrap_or_default();
                self.table = outer;
                if let Some((_, Some(c))) = self.table.as_mut() {
                    for r in rows {
                        c.push_str(&r.join(" / "));
                        c.push(' ');
                    }
                } else {
                    write_table(&mut self.out, &rows);
                }
            }
            "tr" => {
                if let Some((rows, _)) = self.table.as_mut() {
                    rows.push(Vec::new());
                }
                self.children(h, depth);
            }
            "td" | "th" => {
                if self.table.is_some() {
                    self.flush();
                    self.table.as_mut().unwrap().1 = Some(String::new());
                    self.children(h, depth);
                    self.flush();
                    if let Some((rows, cell)) = self.table.as_mut() {
                        let c = cell.take().unwrap_or_default();
                        match rows.last_mut() {
                            Some(r) => r.push(c),
                            None => rows.push(vec![c]),
                        }
                    }
                } else {
                    self.children(h, depth);
                }
            }
            _ => self.children(h, depth),
        }
    }
}

pub fn convert(html: &str) -> Extracted {
    let cleaned = strip_blocks(html);
    let mut ex = Extracted::default();
    let dom = match tl::parse(&cleaned, tl::ParserOptions::default()) {
        Ok(d) => d,
        Err(_) => {
            ex.content = cleaned.clone();
            ex.warnings.push("HTML could not be parsed; returned as text".into());
            return ex;
        }
    };
    let parser = dom.parser();
    let mut ctx = Ctx {
        parser,
        out: String::with_capacity(cleaned.len() / 2),
        line: String::new(),
        lists: Vec::new(),
        pre: 0,
        title: None,
        images: 0,
        table: None,
    };
    for h in dom.children() {
        ctx.node(h, 0);
    }
    ctx.flush();
    if ctx.images > 0 {
        ex.warnings.push(format!("{} image(s) were not transcribed", ctx.images));
    }
    ex.title_hint = ctx.title;
    ex.content = ctx.out;
    ex
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn converts_basic_html() {
        let ex = convert(
            "<html><head><title>T &amp; U</title><script>if(a<b){}</script></head><body><h1>Hello</h1><p>One <b>bold</b> and <a href=\"https://x.com\">link</a>.</p><ul><li>a</li><li>b</li></ul><table><tr><th>h1</th><th>h2</th></tr><tr><td>1</td><td>2|3</td></tr></table><pre>code\n  x</pre></body></html>",
        );
        assert_eq!(ex.title_hint.as_deref(), Some("T & U"));
        let c = &ex.content;
        assert!(c.contains("# Hello\n"), "{c}");
        assert!(c.contains("One **bold** and [link](https://x.com)."), "{c}");
        assert!(c.contains("- a\n- b\n"), "{c}");
        assert!(c.contains("| h1 | h2 |\n| --- | --- |\n| 1 | 2\\|3 |"), "{c}");
        assert!(c.contains("```\ncode\n  x\n```"), "{c}");
        assert!(!c.contains("if(a"), "{c}");
    }
    #[test]
    fn entities() {
        assert_eq!(decode_entities("a &lt;b&gt; &#65;&#x42; &bogus; & c"), "a <b> AB &bogus; & c");
    }
    #[test]
    fn deep_nesting_does_not_overflow() {
        let html = "<div>".repeat(5000) + "deep" + &"</div>".repeat(5000);
        let ex = convert(&html);
        assert!(ex.content.contains("deep"));
    }
}
