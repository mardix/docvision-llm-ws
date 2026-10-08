//! Markdown normalization, single-pass statistics and range-based chunking.

use serde::Serialize;
use unicode_segmentation::UnicodeSegmentation;

pub const WORD_METHOD: &str = "unicode-segmentation UAX#29 word boundaries";
pub const TOKEN_METHOD: &str = "estimate: ceil(chars / 4)";

/// Where a byte range of the content came from (page, slide or sheet numbers, 1-based).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SpanKind {
    Page,
    Slide,
    Sheet,
}

#[derive(Debug, Clone, Copy)]
pub struct Segment {
    pub start: usize,
    pub kind: SpanKind,
    pub from: u32,
    pub to: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkRange {
    pub start: usize,
    pub end: usize,
    /// Bytes of the previous chunk repeated at the start of this one (`chunk_overlap`).
    /// `start..end` stays contiguous; the served text is `start - overlap..end`.
    pub overlap: usize,
    pub headings: Vec<String>,
    pub tokens: u32,
    pub span: Option<(SpanKind, u32, u32)>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct TextStats {
    pub total_words: u64,
    pub total_characters: u64,
    pub content_bytes: u64,
}

/// Normalize line endings, strip trailing whitespace, collapse runs of blank lines
/// (outside fenced code), and end with exactly one newline.
pub fn normalize(input: &str) -> String {
    let mut s = input.to_string();
    normalize_in_place(&mut s, &mut []);
    s
}

fn is_fence(line: &[u8]) -> bool {
    let t = &line[line.iter().take_while(|b| **b == b' ').count()..];
    t.starts_with(b"```") || t.starts_with(b"~~~")
}

/// In-place normalization (no second copy of the document). Only ASCII whitespace and whole
/// blank lines are removed, so the write cursor never passes the read cursor and UTF-8 stays
/// valid. Segment offsets are remapped to the compacted buffer.
pub fn normalize_in_place(s: &mut String, segments: &mut [Segment]) {
    let mut v = std::mem::take(s).into_bytes();
    let len = v.len();
    let (mut r, mut w, mut seg) = (0usize, 0usize, 0usize);
    let mut blank_run = 0;
    let mut in_fence = false;
    while r < len {
        while seg < segments.len() && segments[seg].start <= r {
            segments[seg].start = w;
            seg += 1;
        }
        let end = v[r..].iter().position(|b| *b == b'\n').map_or(len, |p| r + p);
        let mut keep = end;
        if keep > r && v[keep - 1] == b'\r' {
            keep -= 1;
        }
        let fence = is_fence(&v[r..keep]);
        if !in_fence {
            while keep > r && matches!(v[keep - 1], b' ' | b'\t' | b'\r' | b'\x0c') {
                keep -= 1;
            }
        }
        if keep == r && !in_fence {
            blank_run += 1;
            if blank_run > 1 || w == 0 {
                r = end + 1;
                continue;
            }
        } else {
            blank_run = 0;
        }
        if fence {
            in_fence = !in_fence;
        }
        v.copy_within(r..keep, w);
        w += keep - r;
        if w < v.len() {
            v[w] = b'\n';
        } else {
            v.push(b'\n');
        }
        w += 1;
        r = end + 1;
    }
    for sg in &mut segments[seg..] {
        sg.start = w;
    }
    while w >= 2 && v[w - 1] == b'\n' && v[w - 2] == b'\n' {
        w -= 1;
    }
    if w == 1 && v[0] == b'\n' {
        w = 0;
    }
    v.truncate(w);
    for sg in segments.iter_mut() {
        sg.start = sg.start.min(w);
    }
    *s = String::from_utf8(v).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
}

fn heading_level(line: &str) -> Option<(usize, &str)> {
    let hashes = line.bytes().take_while(|b| *b == b'#').count();
    if (1..=6).contains(&hashes) && line[hashes..].starts_with(' ') { Some((hashes, line[hashes..].trim())) } else { None }
}

fn est_tokens(chars: u64) -> u32 {
    chars.div_ceil(4) as u32
}

fn span_for(segments: &[Segment], start: usize, end: usize) -> Option<(SpanKind, u32, u32)> {
    let mut out: Option<(SpanKind, u32, u32)> = None;
    for (i, s) in segments.iter().enumerate() {
        let seg_end = segments.get(i + 1).map(|n| n.start).unwrap_or(usize::MAX);
        if s.start < end && seg_end > start {
            out = Some(match out {
                None => (s.kind, s.from, s.to),
                Some((k, a, b)) => (k, a.min(s.from), b.max(s.to)),
            });
        }
    }
    out
}

/// One scan of the final Markdown: counts words/characters/bytes and builds chunk ranges.
/// Ranges are contiguous and cover the whole content, so no content is lost.
pub fn analyze(content: &str, target_tokens: u32, segments: &[Segment]) -> (TextStats, Vec<ChunkRange>) {
    let (stats, chunks) = split(content, target_tokens, segments);
    (stats, merge_small(content, chunks, target_tokens.max(16), segments))
}

/// Structural split (headings, then size), before small chunks are merged.
fn split(content: &str, target_tokens: u32, segments: &[Segment]) -> (TextStats, Vec<ChunkRange>) {
    let target = target_tokens.max(16);
    let mut stats = TextStats { content_bytes: content.len() as u64, ..Default::default() };
    let mut chunks = Vec::new();
    let mut headings: Vec<(usize, String)> = Vec::new();
    let mut chunk_start = 0usize;
    let mut chunk_chars = 0u64;
    let mut chunk_headings: Vec<String> = Vec::new();
    // Whether the open chunk has any text besides headings and blank lines.
    let mut chunk_has_body = false;
    let mut in_fence = false;
    let mut pos = 0usize;

    let mut close = |start: &mut usize, end: usize, chars: &mut u64, ctx: &mut Vec<String>, next_ctx: Vec<String>| {
        if end > *start {
            chunks.push(ChunkRange {
                start: *start,
                end,
                overlap: 0,
                headings: std::mem::replace(ctx, next_ctx),
                tokens: est_tokens(*chars),
                span: span_for(segments, *start, end),
            });
        } else {
            *ctx = next_ctx;
        }
        *start = end;
        *chars = 0;
    };

    for line in content.split_inclusive('\n') {
        let line_start = pos;
        pos += line.len();
        let text = line.trim_end_matches('\n');
        let chars = line.chars().count() as u64;
        stats.total_characters += chars;
        stats.total_words += text.unicode_words().count() as u64;

        let t = text.trim_start();
        let fence = t.starts_with("```") || t.starts_with("~~~");
        let mut is_heading = false;
        if !in_fence && let Some((level, title)) = heading_level(text) {
            is_heading = true;
            while headings.last().is_some_and(|(l, _)| *l >= level) {
                headings.pop();
            }
            headings.push((level, title.to_string()));
            let ctx: Vec<String> = headings.iter().map(|(_, h)| h.clone()).collect();
            // A heading starts a new chunk, unless the open chunk is only headings so far
            // (e.g. "## 2 Section" directly followed by "### 2.1 Part"): those stay together.
            if chunk_has_body {
                close(&mut chunk_start, line_start, &mut chunk_chars, &mut chunk_headings, ctx);
                chunk_has_body = false;
            } else {
                chunk_headings = ctx;
            }
        }
        if fence {
            in_fence = !in_fence;
        }
        chunk_chars += chars;
        let tokens = est_tokens(chunk_chars);
        let in_table = t.starts_with('|');
        let blank = text.trim().is_empty();
        chunk_has_body |= !blank && !is_heading;
        // Split at block boundaries once the target is reached; inside large tables or code,
        // split at line boundaries; hard cap at 2x target for very long paragraphs.
        if tokens >= target && chunk_has_body && (blank || in_fence || in_table || tokens >= target * 2) {
            let ctx: Vec<String> = headings.iter().map(|(_, h)| h.clone()).collect();
            close(&mut chunk_start, pos, &mut chunk_chars, &mut chunk_headings, ctx);
            chunk_has_body = false;
        }
    }
    let ctx = chunk_headings.clone();
    close(&mut chunk_start, pos, &mut chunk_chars, &mut chunk_headings, ctx);
    (stats, chunks)
}

/// Chunks under a quarter of the target (a short section, a lone heading at the end, stray
/// whitespace) are merged into a neighbour when the result stays near the target, so every
/// chunk carries meaningful text. Coverage stays contiguous; headings become the shared path.
fn merge_small(content: &str, chunks: Vec<ChunkRange>, target: u32, segments: &[Segment]) -> Vec<ChunkRange> {
    let small = |c: &ChunkRange| c.tokens < target / 4 || content[c.start..c.end].trim().is_empty();
    let mut out: Vec<ChunkRange> = Vec::with_capacity(chunks.len());
    for c in chunks {
        if let Some(prev) = out.last_mut() {
            let fits = prev.tokens + c.tokens <= target + target / 4;
            if (small(prev) || small(&c)) && (fits || content[c.start..c.end].trim().is_empty()) {
                let shared = prev.headings.iter().zip(&c.headings).take_while(|(a, b)| a == b).count();
                // A chunk that is only a heading takes the headings of what follows it.
                if content[prev.start..prev.end].lines().all(|l| l.trim().is_empty() || heading_level(l).is_some()) {
                    prev.headings = c.headings.clone();
                } else {
                    prev.headings.truncate(shared);
                }
                prev.end = c.end;
                prev.tokens += c.tokens;
                prev.span = span_for(segments, prev.start, prev.end);
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// Remove a wrapper an LLM put around its Markdown answer, e.g.
/// "```markdown\n…\n```", "~~~md", a bare "```", or a one-line preamble such as
/// "Here is the transcription:" before the fence. Only a fence that encloses the whole
/// answer (optionally unterminated, when output was cut off) is removed; code blocks inside
/// the document are left alone.
pub fn strip_llm_wrapper(text: &str) -> String {
    let t = text.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    let mut lines: Vec<&str> = t.lines().collect();
    // Optional short preamble line ending with ':' right before the opening fence.
    if lines.len() > 1 && is_wrapper_fence(lines[1]).is_some() {
        let first = lines[0].trim();
        if first.ends_with(':') && first.len() <= 120 && !first.starts_with('#') {
            lines.remove(0);
        }
    }
    let Some((ch, len)) = lines.first().and_then(|l| is_wrapper_fence(l)) else { return t.to_string() };
    lines.remove(0);
    // Find the fence that closes the wrapper, skipping code blocks nested inside it
    // (an inner block opens with an info string, e.g. "```rust", and closes with a bare fence).
    let bare = |l: &str, min: usize| {
        let l = l.trim();
        let c = l.chars().next();
        (c == Some('`') || c == Some('~')) && l.chars().all(|x| Some(x) == c) && l.len() >= min
    };
    let mut inner = false;
    let mut close = None;
    for (i, l) in lines.iter().enumerate() {
        let lt = l.trim();
        if inner {
            if bare(lt, 3) {
                inner = false;
            }
        } else if bare(lt, len) && lt.starts_with(ch) {
            close = Some(i);
            break;
        } else if lt.starts_with("```") || lt.starts_with("~~~") {
            inner = true;
        }
    }
    match close {
        // Closing fence is the last non-empty line: a wrapper.
        Some(i) if lines[i + 1..].iter().all(|l| l.trim().is_empty()) => lines.truncate(i),
        // Content follows the closing fence: a real code block at the start of the document.
        Some(_) => return t.to_string(),
        // No closing fence: the answer was cut off inside the wrapper.
        None => {}
    }
    lines.join("\n").trim_matches('\n').to_string()
}

/// `Some((fence char, fence length))` for an opening fence whose info string is empty or a
/// Markdown language tag.
fn is_wrapper_fence(line: &str) -> Option<(char, usize)> {
    let l = line.trim();
    let ch = l.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let len = l.chars().take_while(|c| *c == ch).count();
    if len < 3 {
        return None;
    }
    let info = l[len..].trim().to_ascii_lowercase();
    matches!(info.as_str(), "" | "markdown" | "md" | "gfm" | "commonmark").then_some((ch, len))
}

/// Prepend up to `overlap_tokens` of the previous chunk's text to every chunk after the first.
/// The overlap starts at a line boundary when one fits, else at a word boundary, so chunks
/// never begin mid-word. Token counts and source spans are updated to include it.
pub fn apply_overlap(content: &str, chunks: &mut [ChunkRange], overlap_tokens: u32, segments: &[Segment]) {
    if overlap_tokens == 0 {
        return;
    }
    let want = overlap_tokens as usize * 4; // chars ~ tokens x 4, the same estimate used for sizing
    for i in 1..chunks.len() {
        let (prev_start, start) = (chunks[i - 1].start, chunks[i].start);
        // Walk back `want` characters (not bytes) within the previous chunk.
        let mut from = start;
        for (n, (idx, _)) in content[prev_start..start].char_indices().rev().enumerate() {
            from = prev_start + idx;
            if n + 1 >= want {
                break;
            }
        }
        if from >= start {
            continue;
        }
        let window = &content[from..start];
        let snapped = if from == prev_start || content[..from].ends_with('\n') {
            Some(from)
        } else if let Some(nl) = window.find('\n').filter(|nl| from + nl + 1 < start) {
            Some(from + nl + 1)
        } else {
            window.find(char::is_whitespace).map(|ws| from + ws + 1).filter(|p| *p < start)
        };
        if let Some(begin) = snapped {
            let c = &mut chunks[i];
            c.overlap = start - begin;
            c.tokens = (content[begin..c.end].chars().count() as u64).div_ceil(4) as u32;
            c.span = span_for(segments, begin, c.end).or(c.span);
        }
    }
}

/// Serializes chunks by materializing text from ranges only at serialization time.
pub struct ChunksSer<'a> {
    pub content: &'a str,
    pub chunks: &'a [ChunkRange],
}

impl Serialize for ChunksSer<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        #[derive(Serialize)]
        struct C<'a> {
            index: usize,
            content: &'a str,
            headings: &'a [String],
            #[serde(skip_serializing_if = "Option::is_none")]
            source_span: Option<Span>,
            tokens: u32,
            tokens_estimated: bool,
            token_method: &'static str,
        }
        #[derive(Serialize)]
        struct Span {
            kind: SpanKind,
            from: u32,
            to: u32,
        }
        let mut seq = s.serialize_seq(Some(self.chunks.len()))?;
        for (i, c) in self.chunks.iter().enumerate() {
            seq.serialize_element(&C {
                index: i,
                content: self.content[c.start - c.overlap..c.end].trim_matches('\n'),
                headings: &c.headings,
                source_span: c.span.map(|(kind, from, to)| Span { kind, from, to }),
                tokens: c.tokens,
                tokens_estimated: true,
                token_method: TOKEN_METHOD,
            })?;
        }
        seq.end()
    }
}

/// Split Markdown into ordered sections of roughly `max_tokens`, at heading/blank-line
/// boundaries, never inside a fenced code block (used by translation and map-reduce).
pub fn sections(content: &str, max_tokens: u32) -> Vec<&str> {
    let (_, chunks) = analyze(content, max_tokens, &[]);
    chunks.iter().map(|c| &content[c.start..c.end]).collect()
}

/// Like [`sections`], but every heading starts a section, however short (language detection).
pub fn heading_sections(content: &str, max_tokens: u32) -> Vec<&str> {
    let (_, chunks) = split(content, max_tokens, &[]);
    chunks.iter().map(|c| &content[c.start..c.end]).filter(|t| !t.trim().is_empty()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_multilingual() {
        let (s, _) = analyze("Hello, world!\n日本語のテキスト\nПривет мир\n", 100, &[]);
        assert_eq!(s.total_characters, "Hello, world!\n日本語のテキスト\nПривет мир\n".chars().count() as u64);
        assert!(s.total_words >= 5);
        assert_eq!(s.content_bytes, "Hello, world!\n日本語のテキスト\nПривет мир\n".len() as u64);
    }

    #[test]
    fn normalizes() {
        assert_eq!(normalize("a  \r\n\r\n\r\n\r\nb\n\n"), "a\n\nb\n");
        assert_eq!(normalize("```\nx  \n\n\n```\n"), "```\nx  \n\n\n```\n");
        assert_eq!(normalize("\n\n"), "");
    }

    #[test]
    fn no_heading_only_or_tiny_chunks() {
        let para = |n: usize| "Standard SQL evaluation compares query results across engines and versions. ".repeat(n);
        let md = format!(
            "# Paper\n\n{}\n\n## 2 EVALUATING STANDARD SQL\n\n### 2.1 Setup\n\n{}\n\n## 3 Short\n\n{}\n\n## 4 Results\n\n{}\n\n## 5 Tiny\n\nOne line.\n",
            para(30),
            para(30),
            para(10),
            para(30)
        );
        let (_, chunks) = analyze(&md, 512, &[]);
        let texts: Vec<&str> = chunks.iter().map(|c| &md[c.start..c.end]).collect();
        for t in &texts {
            let body = t.lines().filter(|l| !l.trim().is_empty() && heading_level(l).is_none()).count();
            assert!(body > 0, "heading-only chunk: {t:?}");
        }
        // The section heading stays with its first subsection and text.
        let c = texts.iter().position(|t| t.contains("## 2 EVALUATING STANDARD SQL")).unwrap();
        assert!(texts[c].contains("### 2.1 Setup") && texts[c].contains("Standard SQL evaluation"));
        assert_eq!(chunks[c].headings, vec!["Paper", "2 EVALUATING STANDARD SQL", "2.1 Setup"]);
        // The one-line last section was merged into the previous chunk.
        assert!(texts.last().unwrap().contains("## 4 Results") && texts.last().unwrap().contains("One line."));
        assert_eq!(chunks.last().unwrap().headings, vec!["Paper"], "a merged chunk keeps the shared heading path");
        assert!(chunks.iter().all(|c| c.tokens >= 512 / 4), "{:?}", chunks.iter().map(|c| c.tokens).collect::<Vec<_>>());
        // Coverage is still contiguous.
        assert_eq!(chunks.first().unwrap().start, 0);
        assert_eq!(chunks.last().unwrap().end, md.len());
        assert!(chunks.windows(2).all(|w| w[0].end == w[1].start));
    }

    #[test]
    fn chunks_split_large_tables_and_keep_headings() {
        let mut md = String::from("# Title\n\nIntro.\n\n## Data\n\n| a | b |\n|---|---|\n");
        for i in 0..400 {
            md.push_str(&format!("| row {i} | value {i} |\n"));
        }
        let (_, chunks) = analyze(&md, 100, &[]);
        assert!(chunks.len() > 5);
        assert_eq!(chunks.first().unwrap().start, 0);
        assert_eq!(chunks.last().unwrap().end, md.len());
        for w in chunks.windows(2) {
            assert_eq!(w[0].end, w[1].start);
        }
        assert_eq!(chunks.last().unwrap().headings, vec!["Title".to_string(), "Data".to_string()]);
    }

    #[test]
    fn normalizes_segments() {
        let mut out = "a  \n\n\n\nb\nc\n".to_string();
        let mut segs =
            [Segment { start: 0, kind: SpanKind::Page, from: 1, to: 1 }, Segment { start: 9, kind: SpanKind::Page, from: 2, to: 2 }];
        normalize_in_place(&mut out, &mut segs);
        assert_eq!(out, "a\n\nb\nc\n");
        assert_eq!(&out[segs[1].start..], "c\n");
        let mut u = "日本語  \r\n\r\n\r\nテキスト".to_string();
        normalize_in_place(&mut u, &mut []);
        assert_eq!(u, "日本語\n\nテキスト\n");
    }

    #[test]
    fn overlap_repeats_previous_text_at_line_boundaries() {
        let md = "# A\n\nfirst line of text\nsecond line of text\n\n# B\n\nthird line\n";
        let (_, mut chunks) = analyze(md, 16, &[]);
        assert_eq!(chunks.len(), 2);
        apply_overlap(md, &mut chunks, 6, &[]);
        let second = &md[chunks[1].start - chunks[1].overlap..chunks[1].end];
        assert!(second.starts_with("second line of text\n"), "{second:?}");
        assert!(second.ends_with("third line\n"));
        assert_eq!(chunks[0].overlap, 0);
        // Contiguous base ranges are unchanged.
        assert_eq!(chunks[0].end, chunks[1].start);
    }

    #[test]
    fn strips_llm_wrappers() {
        let body = "# Title\n\nText with `code`.\n\n```rust\nfn main() {}\n```\n\nEnd.";
        for wrapped in [
            format!("```markdown\n{body}\n```"),
            format!("```Markdown\n{body}\n```\n"),
            format!("```md\n{body}\n```"),
            format!("```\n{body}\n```"),
            format!("~~~markdown\n{body}\n~~~"),
            format!("````markdown\n{body}\n````"),
            format!("  \n```markdown\n{body}\n```  \n"),
            format!("Here is the transcription:\n```markdown\n{body}\n```"),
            format!("```markdown\n{body}"), // unterminated (cut off)
        ] {
            assert_eq!(strip_llm_wrapper(&wrapped), body, "{wrapped:?}");
        }
        // Not wrappers: plain Markdown, a document that starts with a real code block, JSON fences.
        assert_eq!(strip_llm_wrapper(body), body);
        let starts_with_code = "```\nls -la\n```\n\nThen read the output.";
        assert_eq!(strip_llm_wrapper(starts_with_code), starts_with_code);
        assert_eq!(strip_llm_wrapper("```python\nx = 1\n```"), "```python\nx = 1\n```");
        assert_eq!(strip_llm_wrapper("```json\n{}\n```"), "```json\n{}\n```");
    }

    #[test]
    fn spans_from_segments() {
        let md = "page one\n\npage two\n";
        let segs =
            [Segment { start: 0, kind: SpanKind::Page, from: 1, to: 1 }, Segment { start: 10, kind: SpanKind::Page, from: 2, to: 2 }];
        let (_, chunks) = analyze(md, 1000, &segs);
        assert_eq!(chunks[0].span, Some((SpanKind::Page, 1, 2)));
    }
}
