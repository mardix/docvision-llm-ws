//! Plain text and Markdown pass through (UTF-8 required; a BOM is stripped).

use super::{ExtractError, Extracted, XResult};
use crate::source::Format;
use std::path::Path;

pub fn extract(path: &Path, format: Format) -> XResult<Extracted> {
    let bytes = std::fs::read(path).map_err(|e| ExtractError::Corrupt(e.to_string()))?;
    let mut ex = Extracted::default();
    let mut content = match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => {
            ex.warnings.push("input was not valid UTF-8; invalid sequences were replaced".into());
            String::from_utf8_lossy(e.as_bytes()).into_owned()
        }
    };
    if content.starts_with('\u{feff}') {
        content.drain(..3);
    }
    if format == Format::Markdown {
        ex.title_hint = content.lines().find_map(|l| l.strip_prefix("# ").map(|t| t.trim().to_string()));
    }
    ex.content = content;
    Ok(ex)
}
