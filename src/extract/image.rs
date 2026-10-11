//! Images (PNG, JPEG, WebP) are transcribed with LLM vision; locally we only validate the
//! header so oversized images are rejected before anything is sent.

use crate::rpc::AppError;
use std::path::Path;

pub fn check_pixels(path: &Path, max_pixels: u64) -> Result<(u64, u64), AppError> {
    let size = imagesize::size(path).map_err(|_| AppError::new(422, "corrupt_document", "unreadable image header").stage("extraction"))?;
    let (w, h) = (size.width as u64, size.height as u64);
    if w.saturating_mul(h) > max_pixels {
        return Err(AppError::new(
            413,
            "image_too_large",
            format!("image is {w}x{h}; exceeds DOCVISION_MAX_DECODED_PIXELS ({max_pixels})"),
        )
        .stage("extraction"));
    }
    Ok((w, h))
}

const SYSTEM_VISION: &str = "You convert images to clean GitHub-flavored Markdown so they can be read and searched. Transcribe all visible text exactly, preserving headings, paragraphs, lists, tables (as GFM tables) and reading order. For content that is not text (photos, illustrations, charts, diagrams, logos, screenshots), add a concise factual description of what is shown, including labels and data values that are readable. If the image contains no text at all, output only that description. Never return an empty response. Do not translate, and do not comment on the task. Output only the Markdown.";

/// Transcribe an image with LLM vision.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    app: &crate::App,
    staged: &crate::source::Staged,
    format: crate::source::Format,
    opts: &crate::rpc::Options,
    ep: Option<&crate::llm::Endpoint>,
    acct: &crate::llm::Accounting,
    sem: &tokio::sync::Semaphore,
    prio: crate::gateway::Prio,
) -> Result<super::Extracted, AppError> {
    if opts.ocr == crate::rpc::Ocr::Off {
        return Err(AppError::new(422, "ocr_required", "image-only content needs ocr=on or ocr=auto").stage("extraction"));
    }
    let max = app.cfg.max_decoded_pixels;
    let p = staged.path.clone();
    app.cpu
        .run(move || check_pixels(&p, max))
        .await
        .map_err(|_| AppError::new(422, "corrupt_document", "unreadable image").stage("extraction"))??;
    let ep = ep.ok_or_else(|| AppError::bad_request("provider_not_configured", "image transcription needs an LLM provider"))?;
    let req = crate::llm::LlmRequest {
        system: SYSTEM_VISION.into(),
        user: "Convert this image to Markdown: transcribe its text and describe any non-text content.".into(),
        doc: Some(crate::llm::DocRef::Inline {
            path: staged.path.clone(),
            mime: format.mime(),
            file_name: format!("image.{}", format.name()),
        }),
        max_output_tokens: ep.cfg.max_output_tokens,
        json: false,
        schema: None,
    };
    let spec = crate::gateway::CallSpec { stage: "extraction", purpose: "image_transcription", span: Some("image 1".into()), prio };
    let r = app.gateway.call(ep, &req, &spec, sem, acct).await.map_err(|e| e.to_app("extraction"))?;
    let mut ex = super::Extracted {
        content: crate::markdown::strip_llm_wrapper(&r.text),
        pages: Some(super::PageCount { total: 1, method: "image_count", exact: true }),
        ..Default::default()
    };
    if r.truncated {
        ex.partial = true;
        ex.warnings.push("transcription was truncated at the model output limit".into());
    }
    if ex.content.trim().is_empty() {
        ex.warnings.push("the model returned no content for this image".into());
    }
    Ok(ex)
}
