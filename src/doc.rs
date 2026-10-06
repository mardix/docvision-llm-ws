//! `GET /_/doc`: the README rendered as HTML (public, rendered once).

use axum::body::Body;
use axum::http::{HeaderValue, header};
use axum::response::Response;
use pulldown_cmark::{CowStr, Event, HeadingLevel, Options, Parser, Tag, TagEnd, html};
use std::sync::OnceLock;

const README: &str = include_str!("../README.md");

/// GitHub-style anchor: lowercase, alphanumerics, `-` and `_`; spaces become `-`.
fn slug(text: &str) -> String {
    text.chars()
        .filter_map(|c| match c {
            ' ' => Some('-'),
            c if c.is_alphanumeric() || c == '-' || c == '_' => Some(c.to_ascii_lowercase()),
            _ => None,
        })
        .collect()
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Markdown -> (body HTML, table of contents HTML). Headings get ids so `#anchors` work.
pub fn render(md: &str) -> (String, String) {
    let mut events: Vec<Event> = Parser::new_ext(md, Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH).collect();
    let mut toc = String::new();
    let mut seen = std::collections::HashMap::<String, usize>::new();
    let mut i = 0;
    while i < events.len() {
        if let Event::Start(Tag::Heading { level, .. }) = &events[i] {
            let level = *level;
            let text: String = events[i + 1..]
                .iter()
                .take_while(|e| !matches!(e, Event::End(TagEnd::Heading(_))))
                .filter_map(|e| match e {
                    Event::Text(t) | Event::Code(t) => Some(t.as_ref()),
                    _ => None,
                })
                .collect();
            let mut id = slug(&text);
            let n = seen.entry(id.clone()).or_default();
            if *n > 0 {
                id = format!("{id}-{n}");
            }
            *n += 1;
            if matches!(level, HeadingLevel::H2 | HeadingLevel::H3) {
                let cls = if level == HeadingLevel::H2 { "l2" } else { "l3" };
                toc.push_str(&format!("<a class=\"{cls}\" href=\"#{id}\">{}</a>", escape(&text)));
            }
            if let Event::Start(Tag::Heading { id: hid, .. }) = &mut events[i] {
                *hid = Some(CowStr::from(id));
            }
        }
        i += 1;
    }
    let mut body = String::with_capacity(md.len() * 2);
    html::push_html(&mut body, events.into_iter());
    (body, toc)
}

fn page() -> &'static str {
    static PAGE: OnceLock<String> = OnceLock::new();
    PAGE.get_or_init(|| {
        let (body, toc) = render(README);
        include_str!("doc.html").replace("{{TOC}}", &toc).replace("{{BODY}}", &body).replace("{{VERSION}}", env!("CARGO_PKG_VERSION"))
    })
}

pub async fn doc() -> Response {
    let mut r = Response::new(Body::from(page()));
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; style-src 'unsafe-inline'; img-src 'self' data:; frame-ancestors 'none'; base-uri 'none'",
        ),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn renders_with_anchors() {
        let (body, toc) = render("# T\n\n## Quick start\n\n| a | b |\n| --- | --- |\n| 1 | 2 |\n\n## Quick start\n\n`x`\n");
        assert!(body.contains("<h2 id=\"quick-start\">") && body.contains("<h2 id=\"quick-start-1\">"), "{body}");
        assert!(body.contains("<table>"));
        assert!(toc.contains("href=\"#quick-start\""));
        assert_eq!(slug("job.get"), "jobget");
        assert_eq!(slug("Request and response"), "request-and-response");
        assert!(page().contains("Webhooks"));
    }
}
