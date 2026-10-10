//! Enrichment: title, summary, language (one combined LLM call by default, or local
//! methods), bounded map-reduce for long documents, and ordered concurrent translation.

use crate::App;
use crate::db::{RequestFinish, RequestRow, WriteOp, now_ms};
use crate::gateway::{CallSpec, Prio};
use crate::llm::{Accounting, Endpoint, LlmRequest};
use crate::markdown;
use crate::result::{Body, FeatureStatus, LanguageDetection};
use crate::rpc::{AppError, Method, Options, ReqCtx};
use futures_util::future::try_join_all;
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::Semaphore;

const SYSTEM_ENRICH: &str = "You analyze documents and return strict JSON. Never include commentary outside the JSON object.";
const SYSTEM_MAP: &str = "You write faithful, concise summaries of document excerpts. Output plain text only.";
const SYSTEM_EXTRACT: &str = "You extract structured data from documents. Return one JSON object that follows the given JSON Schema. Use only facts stated in the document; never invent or guess values. Use null for anything the document does not contain. Copy numbers exactly (no currency symbols or thousands separators in numeric fields) and write dates as they are written unless the schema asks for a format.";
const SYSTEM_TRANSLATE: &str = "You are a professional translator for Markdown documents. Preserve Markdown structure exactly: headings, lists, tables, code blocks and inline code (never translate code), links and URLs, proper names and numbers. Output only the translated Markdown, with no commentary.";

pub struct Cx<'a> {
    pub app: &'a App,
    pub ep: Option<&'a Endpoint>,
    pub acct: &'a Accounting,
    pub sem: &'a Semaphore,
    pub prio: Prio,
    /// Optional features stop here so a job deadline keeps whatever already completed.
    pub deadline: Option<tokio::time::Instant>,
}

async fn within<T>(deadline: Option<tokio::time::Instant>, f: impl std::future::Future<Output = Result<T, String>>) -> Result<T, String> {
    match deadline {
        Some(d) => tokio::time::timeout_at(d, f).await.unwrap_or_else(|_| Err("job deadline reached before this feature finished".into())),
        None => f.await,
    }
}

fn est_tokens(s: &str) -> u64 {
    (s.len() as u64).div_ceil(4)
}

// ---------------------------------------------------------------- local methods

pub fn local_title(hint: Option<&str>, content: &str, file_name: &str) -> Option<String> {
    if let Some(h) = hint.map(str::trim).filter(|h| !h.is_empty()) {
        return Some(h.chars().take(200).collect());
    }
    let mut in_fence = false;
    for line in content.lines() {
        let t = line.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            in_fence = !in_fence;
        }
        if in_fence {
            continue;
        }
        let hashes = t.bytes().take_while(|b| *b == b'#').count();
        if (1..=6).contains(&hashes) && t[hashes..].starts_with(' ') {
            let h = t[hashes..].trim().trim_matches('*').trim();
            if !h.is_empty() && !h.starts_with("Slide ") && !h.starts_with("Page ") {
                return Some(h.chars().take(200).collect());
            }
        }
    }
    let stem = file_name.rsplit_once('.').map(|(s, _)| s).unwrap_or(file_name).trim();
    (!stem.is_empty()).then(|| stem.replace(['_', '-'], " "))
}

/// Deterministic extractive summary: leading prose sentences, max 5 sentences / 600 chars.
pub fn local_summary(content: &str) -> Option<String> {
    let mut out = String::new();
    let mut sentences = 0;
    let mut in_fence = false;
    'outer: for line in content.lines() {
        let t = line.trim();
        if t.starts_with("```") || t.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence || t.is_empty() || t.starts_with('#') || t.starts_with('|') || t.starts_with('>') || t.len() < 30 {
            continue;
        }
        let t = t.trim_start_matches(['-', '*', '+']).trim();
        let mut start = 0;
        let bytes = t.as_bytes();
        for (i, c) in t.char_indices() {
            let end = i + c.len_utf8();
            let boundary = matches!(c, '.' | '!' | '?' | '。') && (end == t.len() || bytes.get(end) == Some(&b' '));
            if boundary || end == t.len() {
                let s = t[start..end].trim();
                if s.len() >= 20 {
                    if out.len() + s.len() > 600 {
                        break 'outer;
                    }
                    if !out.is_empty() {
                        out.push(' ');
                    }
                    out.push_str(s);
                    sentences += 1;
                    if sentences >= 5 {
                        break 'outer;
                    }
                }
                start = end;
            }
        }
    }
    (!out.is_empty()).then(|| format!("Extractive summary: {out}"))
}

#[cfg(feature = "lang-detect")]
pub fn detect_language(content: &str) -> LanguageDetection {
    let sections: Vec<&str> = markdown::heading_sections(content, 600).into_iter().take(64).collect();
    let mut weights: std::collections::HashMap<whatlang::Lang, f64> = Default::default();
    let mut total = 0.0;
    let mut conf_sum = 0.0;
    for s in &sections {
        if let Some(info) = whatlang::detect(s) {
            let w = s.len() as f64;
            *weights.entry(info.lang()).or_default() += w;
            total += w;
            conf_sum += info.confidence() * w;
        }
    }
    if total == 0.0 {
        return LanguageDetection { code: "und".into(), method: "local_whatlang", confidence: 0.0 };
    }
    let mut ranked: Vec<_> = weights.into_iter().collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
    let top_share = ranked[0].1 / total;
    let second_share = ranked.get(1).map(|r| r.1 / total).unwrap_or(0.0);
    let confidence = (conf_sum / total * 1000.0).round() / 1000.0;
    let code = if top_share < 0.75 && second_share >= 0.2 {
        "mul".to_string()
    } else if confidence < 0.3 {
        "und".to_string()
    } else {
        bcp47(ranked[0].0.code())
    };
    LanguageDetection { code, method: "local_whatlang", confidence }
}

/// ISO 639-3 -> shortest BCP-47 primary subtag.
pub fn bcp47(code3: &str) -> String {
    const MAP: &[(&str, &str)] = &[
        ("eng", "en"),
        ("fra", "fr"),
        ("deu", "de"),
        ("spa", "es"),
        ("por", "pt"),
        ("ita", "it"),
        ("nld", "nl"),
        ("rus", "ru"),
        ("ukr", "uk"),
        ("pol", "pl"),
        ("ces", "cs"),
        ("slk", "sk"),
        ("slv", "sl"),
        ("hrv", "hr"),
        ("srp", "sr"),
        ("bul", "bg"),
        ("ron", "ro"),
        ("hun", "hu"),
        ("fin", "fi"),
        ("swe", "sv"),
        ("dan", "da"),
        ("nob", "nb"),
        ("isl", "is"),
        ("est", "et"),
        ("lav", "lv"),
        ("lit", "lt"),
        ("ell", "el"),
        ("tur", "tr"),
        ("ara", "ar"),
        ("heb", "he"),
        ("pes", "fa"),
        ("urd", "ur"),
        ("hin", "hi"),
        ("ben", "bn"),
        ("pan", "pa"),
        ("guj", "gu"),
        ("mar", "mr"),
        ("tam", "ta"),
        ("tel", "te"),
        ("kan", "kn"),
        ("mal", "ml"),
        ("ori", "or"),
        ("sin", "si"),
        ("nep", "ne"),
        ("tha", "th"),
        ("vie", "vi"),
        ("ind", "id"),
        ("msa", "ms"),
        ("tgl", "tl"),
        ("jpn", "ja"),
        ("kor", "ko"),
        ("cmn", "zh"),
        ("zho", "zh"),
        ("khm", "km"),
        ("mya", "my"),
        ("kat", "ka"),
        ("hye", "hy"),
        ("aze", "az"),
        ("kaz", "kk"),
        ("uzb", "uz"),
        ("bel", "be"),
        ("mkd", "mk"),
        ("cat", "ca"),
        ("eus", "eu"),
        ("glg", "gl"),
        ("afr", "af"),
        ("swh", "sw"),
        ("zul", "zu"),
        ("xho", "xh"),
        ("yor", "yo"),
        ("ibo", "ig"),
        ("hau", "ha"),
        ("amh", "am"),
        ("som", "so"),
        ("epo", "eo"),
        ("lat", "la"),
        ("yid", "yi"),
        ("jav", "jv"),
        ("sna", "sn"),
        ("aka", "ak"),
        ("tuk", "tk"),
        ("lav", "lv"),
        ("cym", "cy"),
        ("gle", "ga"),
        ("sqi", "sq"),
    ];
    MAP.iter().find(|(a, _)| *a == code3).map(|(_, b)| b.to_string()).unwrap_or_else(|| code3.to_string())
}

// ---------------------------------------------------------------- LLM methods

async fn call(cx: &Cx<'_>, purpose: &str, span: Option<String>, req: LlmRequest) -> Result<String, String> {
    let ep = cx.ep.ok_or("no LLM provider is configured")?;
    let spec = CallSpec { stage: "enrichment", purpose, span, prio: cx.prio };
    match cx.app.gateway.call(ep, &req, &spec, cx.sem, cx.acct).await {
        Ok(r) if r.truncated => Err("model output was truncated at the output token limit".into()),
        Ok(r) => Ok(crate::markdown::strip_llm_wrapper(&r.text)),
        Err(e) => Err(e.message()),
    }
}

/// The JSON object inside a model answer: providers without a JSON mode may wrap it in a
/// ```json fence or add a sentence around it.
fn json_text(s: &str) -> &str {
    let t = s.trim();
    match (t.find('{'), t.rfind('}')) {
        (Some(a), Some(b)) if a < b => &t[a..=b],
        _ => t,
    }
}

/// Structured extraction: one call returning JSON for `schema`, validated, with one retry that
/// tells the model what was wrong. Content beyond the model's input budget is cut (with a warning).
async fn extract_structured(cx: &Cx<'_>, content: &str, schema: &Value) -> Result<(Value, Option<String>), String> {
    let ep = cx.ep.ok_or("no LLM provider is configured")?;
    let schema_text = schema.to_string();
    let budget = (ep.cfg.max_input_tokens as usize).saturating_sub(4_000 + schema_text.len() / 4) * 4;
    let (doc, note) = if content.len() > budget {
        let cut = (0..=budget).rev().find(|i| content.is_char_boundary(*i)).unwrap_or(0);
        (&content[..cut], Some(format!("structured extraction used the first {cut} of {} bytes of content", content.len())))
    } else {
        (content, None)
    };
    let mut feedback = String::new();
    for attempt in 1..=2 {
        let req = LlmRequest {
            system: SYSTEM_EXTRACT.into(),
            user: format!(
                "Extract the data described by this JSON Schema from the document.{feedback}\n\n<schema>\n{schema_text}\n</schema>\n\n<document>\n{doc}\n</document>"
            ),
            doc: None,
            max_output_tokens: ep.cfg.max_output_tokens,
            json: true,
            schema: Some(schema.clone()),
        };
        let out = call(cx, "structured_extraction", Some(format!("attempt {attempt}")), req).await?;
        let problem = match serde_json::from_str::<Value>(json_text(&out)) {
            Ok(v) => match crate::schema::validate(&v, schema) {
                Ok(()) => return Ok((v, note)),
                Err(e) => e,
            },
            Err(_) => "the answer was not valid JSON".to_string(),
        };
        if attempt == 2 {
            return Err(format!("model output does not match extract_schema: {problem}"));
        }
        feedback = format!(" Your previous answer was rejected ({problem}); return a corrected JSON object.");
    }
    unreachable!()
}

/// Reduce text to fit the model's input budget: summarize sections concurrently per level.
async fn map_reduce(cx: &Cx<'_>, content: &str, budget_tokens: u64) -> Result<String, String> {
    let mut text = content.to_string();
    for level in 0..5 {
        if est_tokens(&text) <= budget_tokens {
            return Ok(text);
        }
        let parts = markdown::sections(&text, (budget_tokens / 2).clamp(500, 100_000) as u32);
        let n = parts.len();
        let futs = parts.iter().enumerate().map(|(i, p)| {
            let req = LlmRequest {
                system: SYSTEM_MAP.into(),
                user: format!("Summarize this excerpt (part {} of {n}) in at most 200 words, keeping key facts, names and numbers.\n\n<excerpt>\n{p}\n</excerpt>", i + 1),
                doc: None,
                max_output_tokens: 600,
                json: false,
                schema: None,
            };
            call(cx, "summary_map", Some(format!("level {level} part {}/{n}", i + 1)), req)
        });
        let summaries = try_join_all(futs).await?;
        text = summaries.join("\n\n");
    }
    Err("document too large to summarize within the reduction depth".into())
}

#[derive(Deserialize, Default)]
struct Combined {
    title: Option<String>,
    summary: Option<String>,
    language: Option<String>,
    language_confidence: Option<f64>,
}

async fn combined(cx: &Cx<'_>, content: &str, title: bool, summary: bool, lang: bool) -> Result<Combined, String> {
    let ep = cx.ep.ok_or("no LLM provider is configured")?;
    let budget = (ep.cfg.max_input_tokens as u64 * 8 / 10).max(1000);
    let text = map_reduce(cx, content, budget).await?;
    let mut keys = Vec::new();
    if title {
        keys.push("\"title\": a concise, specific document title (max 15 words)");
    }
    if summary {
        keys.push("\"summary\": a faithful 3-6 sentence summary");
    }
    if lang {
        keys.push("\"language\": the BCP-47 code of the main language (\"mul\" if substantially mixed, \"und\" if undeterminable), and \"language_confidence\": a number from 0 to 1");
    }
    let req = LlmRequest {
        system: SYSTEM_ENRICH.into(),
        user: format!("Return a JSON object with these keys:\n- {}\n\n<document>\n{text}\n</document>", keys.join("\n- ")),
        doc: None,
        max_output_tokens: 1024,
        json: true,
        schema: None,
    };
    let out = call(cx, "title_summary_language", None, req).await?;
    serde_json::from_str::<Combined>(json_text(&out)).map_err(|_| "model did not return valid JSON".to_string())
}

pub struct Translation {
    pub content: String,
    pub language: String,
}

async fn translate(cx: &Cx<'_>, content: &str, target: &str) -> Result<Translation, String> {
    let ep = cx.ep.ok_or("no LLM provider is configured")?;
    let max_out = ep.cfg.max_output_tokens.max(512);
    let section_tokens = (max_out / 3).clamp(256, 4000);
    let parts = markdown::sections(content, section_tokens);
    let n = parts.len();
    let futs = parts.iter().enumerate().map(|(i, p)| {
        let req = LlmRequest {
            system: SYSTEM_TRANSLATE.into(),
            user: format!("Translate the following Markdown into {target}.\n\n{p}"),
            doc: None,
            max_output_tokens: ((est_tokens(p) * 3) as u32 + 256).min(max_out),
            json: false,
            schema: None,
        };
        call(cx, "translation", Some(format!("section {}/{n}", i + 1)), req)
    });
    // All sections must succeed; a partial translation is never reported as complete.
    let out = try_join_all(futs).await?;
    let mut content = String::with_capacity(content.len() + content.len() / 4);
    for (i, s) in out.iter().enumerate() {
        if i > 0 && !content.ends_with("\n\n") {
            content.push_str(if content.ends_with('\n') { "\n" } else { "\n\n" });
        }
        content.push_str(s.trim_matches('\n'));
        content.push('\n');
    }
    Ok(Translation { content, language: target.to_string() })
}

/// Fill title/summary/language/translation fields of `body` (content must be final).
pub async fn enrich(cx: &Cx<'_>, opts: &Options, title_hint: Option<&str>, file_name: &str, body: &mut Body) {
    let content = body.content.as_str();
    let want_title = opts.gen_title;
    let want_summary = opts.gen_summary;
    let want_lang = opts.detect_language;
    let llm_title = want_title && opts.title_method == Method::Llm;
    let llm_summary = want_summary && opts.summary_method == Method::Llm;
    let llm_lang = want_lang && opts.language_method == Method::Llm;
    let empty = content.trim().is_empty();
    let mut fs = std::mem::take(&mut body.feature_status);
    let mut warnings = Vec::new();

    if !want_title {
        fs.insert("title", FeatureStatus::disabled());
    } else if !llm_title || empty {
        body.title = local_title(title_hint, content, file_name);
        fs.insert("title", FeatureStatus::done("local"));
    }
    if !want_summary {
        fs.insert("summary", FeatureStatus::disabled());
    } else if !llm_summary || empty {
        body.summary = local_summary(content);
        fs.insert("summary", FeatureStatus::done("local_extractive"));
    }
    if !want_lang {
        fs.insert("language", FeatureStatus::disabled());
    } else if !llm_lang || empty {
        #[cfg(feature = "lang-detect")]
        {
            let d = detect_language(content);
            body.language = Some(d.code.clone());
            body.language_detection = Some(d);
            fs.insert("language", FeatureStatus::done("local_whatlang"));
        }
    }

    let any_llm = !empty && (llm_title || llm_summary || llm_lang);
    let target = opts.translate_to.clone();
    let combined_fut =
        async { if any_llm { Some(within(cx.deadline, combined(cx, content, llm_title, llm_summary, llm_lang)).await) } else { None } };
    let translate_fut = async {
        match &target {
            Some(t) if !empty => {
                let started = std::time::Instant::now();
                let r = within(cx.deadline, translate(cx, content, t)).await;
                Some((r, started.elapsed().as_millis() as u64))
            }
            _ => None,
        }
    };
    let extract_fut = async {
        match &opts.extract_schema {
            Some(schema) if !empty => Some(within(cx.deadline, extract_structured(cx, content, schema)).await),
            _ => None,
        }
    };
    let (c, t, x) = tokio::join!(combined_fut, translate_fut, extract_fut);
    match x {
        None if opts.extract_schema.is_some() => {
            fs.insert("extraction", FeatureStatus { status: "skipped", method: Some("llm"), error: Some("empty content".into()) });
        }
        None => {
            fs.insert("extraction", FeatureStatus::disabled());
        }
        Some(Ok((v, note))) => {
            body.extracted = Some(v);
            fs.insert("extraction", FeatureStatus::done("llm"));
            warnings.extend(note);
        }
        Some(Err(e)) => {
            fs.insert("extraction", FeatureStatus::failed("llm", e.clone()));
            warnings.push(format!("structured extraction failed: {e}"));
        }
    }
    let t = t.map(|(r, ms)| {
        body.stages.translation_ms = ms;
        r
    });

    match c {
        Some(Ok(c)) => {
            if llm_title {
                match c.title.map(|t| t.trim().to_string()).filter(|t| !t.is_empty()) {
                    Some(t) => {
                        body.title = Some(t);
                        fs.insert("title", FeatureStatus::done("llm"));
                    }
                    None => {
                        body.title = local_title(title_hint, content, file_name);
                        fs.insert("title", FeatureStatus::fallback("the LLM returned no title".into()));
                    }
                }
            }
            if llm_summary {
                body.summary = c.summary.map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
                fs.insert("summary", FeatureStatus::done("llm"));
            }
            if llm_lang {
                let code = c.language.unwrap_or_else(|| "und".into());
                body.language = Some(code.clone());
                body.language_detection =
                    Some(LanguageDetection { code, method: "llm", confidence: c.language_confidence.unwrap_or(0.0).clamp(0.0, 1.0) });
                fs.insert("language", FeatureStatus::done("llm"));
            }
        }
        Some(Err(e)) => {
            // A title is always useful: fall back to the local one when the LLM call fails.
            if llm_title {
                body.title = local_title(title_hint, content, file_name);
                fs.insert("title", FeatureStatus::fallback(e.clone()));
            }
            for (on, name) in [(llm_summary, "summary"), (llm_lang, "language")] {
                if on {
                    fs.insert(name, FeatureStatus::failed("llm", e.clone()));
                }
            }
            warnings.push(format!("enrichment failed: {e}"));
        }
        None => {}
    }
    match t {
        None => {
            fs.insert(
                "translation",
                if target.is_some() {
                    FeatureStatus { status: "skipped", method: Some("llm"), error: Some("empty content".into()) }
                } else {
                    FeatureStatus::disabled()
                },
            );
        }
        Some(Ok(tr)) => {
            let (stats, _) = markdown::analyze(&tr.content, opts.effective_chunk_size(), &[]);
            let mut ts = body.statistics.clone();
            ts.total_words = stats.total_words;
            ts.total_characters = stats.total_characters;
            ts.content_bytes = stats.content_bytes;
            ts.markdown_estimated_total_pages = stats.total_words.div_ceil(opts.words_per_page as u64);
            ts.chunk_count = 0;
            body.translated_statistics = Some(ts);
            body.translated_content = Some(tr.content);
            body.translated_language = Some(tr.language);
            fs.insert("translation", FeatureStatus::done("llm"));
        }
        Some(Err(e)) => {
            fs.insert("translation", FeatureStatus::failed("llm", e.clone()));
            warnings.push(format!("translation failed: {e}"));
        }
    }
    body.feature_status = fs;
    body.warnings.extend(warnings);
}

// ---------------------------------------------------------------- summarize / chunk operations

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TextPayload {
    #[serde(default)]
    source_content: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    metadata: Option<serde_json::Map<String, Value>>,
    #[serde(default)]
    requester_id: Option<String>,
}

async fn load_text(app: &App, p: &TextPayload) -> Result<(String, String), AppError> {
    let cap = app.cfg.max_inline_text_bytes;
    if let Some(c) = &p.source_content {
        if c.len() > cap {
            return Err(AppError::new(
                413,
                "inline_text_too_large",
                format!("source_content exceeds DOCVISION_MAX_INLINE_TEXT_BYTES ({cap})"),
            ));
        }
        return Ok((c.clone(), String::new()));
    }
    let src = p
        .source
        .as_deref()
        .ok_or_else(|| AppError::bad_request("invalid_payload", "payload.source_content or payload.source is required"))?;
    let parsed = crate::source::parse(src)?;
    let fctx = crate::source::FetchCtx { http: crate::source::http_client(), staging: &app.staging_dir(), max_bytes: cap as u64 };
    let staged = crate::source::fetch(&fctx, &parsed).await.map_err(|e| {
        if e.code == "input_too_large" {
            AppError::new(413, "inline_text_too_large", "text source exceeds DOCVISION_MAX_INLINE_TEXT_BYTES")
        } else {
            e
        }
    })?;
    let name = crate::source::file_name(src);
    let path = staged.path.clone();
    let n2 = name.clone();
    let format = app.cpu.run(move || crate::source::sniff(&path, &n2)).await.map_err(|_| AppError::internal("sniff failed"))??;
    if !format.is_text() {
        return Err(AppError::bad_request(
            "conversion_required",
            format!("source is {}; run `convert` first and pass the Markdown as payload.source_content", format.name()),
        ));
    }
    let bytes = tokio::fs::read(&staged.path).await.map_err(|_| AppError::internal("cannot read staged source"))?;
    Ok((String::from_utf8_lossy(&bytes).into_owned(), name))
}

fn log_text_op(app: &App, ctx: &ReqCtx, p: &TextPayload, status: u16, err: Option<&AppError>, usage: Option<String>) {
    app.writer.send(WriteOp::InsertRequest(RequestRow {
        request_id: ctx.request_id.clone(),
        operation: ctx.operation.clone(),
        mode: Some("sync".into()),
        source: p.source.as_deref().map(crate::source::sanitize),
        metadata: p.metadata.as_ref().map(|m| Value::Object(m.clone()).to_string()),
        requester_id: crate::rpc::requester_id(p.requester_id.as_deref()).ok(),
        request_status: "running".into(),
        created_at: ctx.created_at,
        ..Default::default()
    }));
    app.writer.send(WriteOp::FinishRequest(RequestFinish {
        request_id: ctx.request_id.clone(),
        request_status: if err.is_some() { "failed".into() } else { "completed".into() },
        execution_status: Some(if err.is_some() { "failed".into() } else { "completed".into() }),
        http_status: Some(status as i64),
        finished_at: now_ms(),
        timings: Some(serde_json::json!({"total_ms": ctx.started.elapsed().as_millis() as u64}).to_string()),
        usage,
        error: err.map(|e| serde_json::to_string(&e.body()).unwrap_or_default()),
        ..Default::default()
    }));
}

pub async fn summarize_op(app: &Arc<App>, ctx: &ReqCtx, payload: Value, options: Value) -> Result<axum::response::Response, AppError> {
    let p: TextPayload = crate::rpc::typed(payload, "payload")?;
    let opts: Options = crate::rpc::typed(options, "options")?;
    opts.validate()?;
    crate::rpc::requester_id(p.requester_id.as_deref())?;
    let res = async {
        let (content, name) = load_text(app, &p).await?;
        let needs_llm = (opts.gen_title && opts.title_method == Method::Llm) || (opts.gen_summary && opts.summary_method == Method::Llm);
        let ep = crate::jobs::endpoint(app, &opts, needs_llm)?;
        let acct = Accounting::new(ctx.request_id.clone(), Some(app.writer.clone()));
        let sem = Semaphore::new(app.cfg.llm_per_request_concurrency);
        let mut body = Body { content, ..Default::default() };
        let mut o = opts.clone();
        o.detect_language = false;
        o.translate_to = None;
        let cx = Cx { app, ep: ep.as_ref(), acct: &acct, sem: &sem, prio: Prio::Sync, deadline: None };
        enrich(&cx, &o, None, &name, &mut body).await;
        let section = acct.section();
        Ok::<_, AppError>(serde_json::json!({
            "summary": body.summary,
            "title": body.title,
            "feature_status": body.feature_status,
            "warnings": body.warnings,
            "llm": section,
        }))
    }
    .await;
    match res {
        Ok(data) => {
            log_text_op(app, ctx, &p, 200, None, data.get("llm").map(|l| l["totals"].to_string()));
            Ok(crate::rpc::ok("summarize", &ctx.request_id, 200, &data))
        }
        Err(e) => {
            log_text_op(app, ctx, &p, e.status, Some(&e), None);
            Err(e)
        }
    }
}

/// `extract`: structured JSON from text/Markdown, following `options.extract_schema`.
pub async fn extract_op(app: &Arc<App>, ctx: &ReqCtx, payload: Value, options: Value) -> Result<axum::response::Response, AppError> {
    let p: TextPayload = crate::rpc::typed(payload, "payload")?;
    let opts: Options = crate::rpc::typed(options, "options")?;
    opts.validate()?;
    crate::rpc::requester_id(p.requester_id.as_deref())?;
    let schema = opts
        .extract_schema
        .clone()
        .ok_or_else(|| AppError::bad_request("invalid_options", "options.extract_schema is required for extract"))?;
    let res = async {
        let ep = crate::jobs::endpoint(app, &opts, true)?;
        let (content, _) = load_text(app, &p).await?;
        let acct = Accounting::new(ctx.request_id.clone(), Some(app.writer.clone()));
        let sem = Semaphore::new(app.cfg.llm_per_request_concurrency);
        let cx = Cx { app, ep: ep.as_ref(), acct: &acct, sem: &sem, prio: Prio::Sync, deadline: None };
        let (extracted, status, warnings) = if content.trim().is_empty() {
            (Value::Null, FeatureStatus { status: "skipped", method: Some("llm"), error: Some("empty content".into()) }, Vec::new())
        } else {
            match extract_structured(&cx, &content, &schema).await {
                Ok((v, note)) => (v, FeatureStatus::done("llm"), note.into_iter().collect()),
                Err(e) => {
                    let section = acct.section();
                    let mut err = AppError::new(422, "extraction_failed", e).stage("enrichment");
                    err.data = serde_json::to_vec(&serde_json::json!({"extracted": null, "llm": section})).ok();
                    return Err(err);
                }
            }
        };
        Ok::<_, AppError>(serde_json::json!({
            "extracted": extracted,
            "feature_status": {"extraction": status},
            "warnings": warnings,
            "llm": acct.section(),
        }))
    }
    .await;
    match res {
        Ok(data) => {
            log_text_op(app, ctx, &p, 200, None, data.get("llm").map(|l| l["totals"].to_string()));
            Ok(crate::rpc::ok("extract", &ctx.request_id, 200, &data))
        }
        Err(e) => {
            log_text_op(app, ctx, &p, e.status, Some(&e), None);
            Err(e)
        }
    }
}

pub async fn chunk_op(app: &Arc<App>, ctx: &ReqCtx, payload: Value, options: Value) -> Result<axum::response::Response, AppError> {
    let p: TextPayload = crate::rpc::typed(payload, "payload")?;
    let opts: Options = crate::rpc::typed(options, "options")?;
    opts.validate()?;
    crate::rpc::requester_id(p.requester_id.as_deref())?;
    let res = load_text(app, &p).await;
    let (content, _) = match res {
        Ok(v) => v,
        Err(e) => {
            log_text_op(app, ctx, &p, e.status, Some(&e), None);
            return Err(e);
        }
    };
    let (target, overlap) = (opts.effective_chunk_size(), opts.effective_chunk_overlap());
    let bytes = app
        .cpu
        .run(move || {
            let (_, mut chunks) = markdown::analyze(&content, target, &[]);
            markdown::apply_overlap(&content, &mut chunks, overlap, &[]);
            serde_json::to_vec(&serde_json::json!({ "chunks": markdown::ChunksSer { content: &content, chunks: &chunks } }))
        })
        .await
        .map_err(|_| AppError::new(422, "unusable_document", "chunking failed"))?
        .map_err(|_| AppError::internal("serialization failed"))?;
    log_text_op(app, ctx, &p, 200, None, None);
    Ok(crate::rpc::json_response(200, crate::rpc::envelope("chunk", &ctx.request_id, Some(&bytes), None), None))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_methods() {
        assert_eq!(local_title(Some("Meta"), "# H", "f.pdf").as_deref(), Some("Meta"));
        assert_eq!(local_title(None, "intro\n\n## Real Heading\n", "f.pdf").as_deref(), Some("Real Heading"));
        assert_eq!(local_title(None, "no headings", "my_report.pdf").as_deref(), Some("my report"));
        let s = local_summary("# T\n\nThis is the first sentence of the body. This is the second sentence here! Short.\n").unwrap();
        assert!(s.starts_with("Extractive summary: This is the first sentence of the body. This is the second sentence here!"), "{s}");
        assert!(local_summary("# Only heading\n").is_none());
    }
    #[test]
    fn bcp() {
        assert_eq!(bcp47("eng"), "en");
        assert_eq!(bcp47("xyz"), "xyz");
    }
}
