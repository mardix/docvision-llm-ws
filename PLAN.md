# Build `doc2md-llm-ws`: a lean, high-throughput document-to-Markdown HTTP service in Rust

You are a senior Rust engineer. Build `doc2md-llm-ws` from an empty repository to a production-ready release. It is a single-process HTTP service: a caller references a document (server path, HTTPS URL or S3 URI), the service converts it to Markdown, using an LLM for PDFs and images and native parsers for modern office formats, and returns Markdown plus statistics, LLM usage and timings.

The service must be **small, memory- and CPU-efficient, fully async, fast, and able to run many conversions at once**. Conversions are dominated by LLM latency, so throughput comes from holding many cheap waiting tasks under strict resource budgets, not from more threads. Every design decision below serves that goal. Where this prompt is silent, choose the option that uses less memory, fewer allocations, fewer database writes and fewer dependencies.

Work through the phases in section 17 in order. Do not stop after the first phase: the task is complete only when every phase's acceptance gate and the definition of done (section 18) pass.

---

## 1. Naming

| Item | Value |
| --- | --- |
| Cargo package, binary, Docker image | `doc2md-llm-ws` |
| Rust crate / log target | `doc2md_llm_ws` |
| Environment variable prefix | `DOC2MD_` |
| Access token variable | `DOC2MD_TOKEN` |
| Default SQLite file | `doc2md.sqlite` in `DOC2MD_DATA_DIR` |
| Webhook headers | `x-doc2md-event-id`, `x-doc2md-signature`, `x-doc2md-timestamp` |
| Request auth header | `X-ACCESS-TOKEN` |

## 2. Scope and non-goals

In scope:

- HTTP only: one RPC endpoint `POST /rpc`, plus `GET /metrics` and `GET /livez`.
- One configured access token; every operation is available to anyone holding it. Startup fails without a token.
- Sources: absolute server-local paths or `file:///` URLs, `https://` URLs (including signed URLs), `s3://bucket/key`, and credential-bearing S3 DSNs.
- Asynchronous conversion by default, synchronous on request.
- LLM providers: OpenAI, Gemini, and OpenAI-compatible APIs.
- Optional titles, summaries, chunks, language detection, translation, output storage, and completion webhooks.
- SQLite by default; PostgreSQL behind a feature flag.

Non-goals (do not build): CLI commands or Clap, a frontend, users, tenants, roles, sessions, per-operation permissions, direct uploads (multipart, raw bodies, base64 documents, data URLs), local OCR engines (no Tesseract or OCR models), Redis or any broker, separate worker processes, distributed coordination, or legacy binary Office formats.

## 3. Performance budgets

Treat these as requirements. Build the benchmark harness first (phase 1) and gate CI on them; a regression of more than 10% fails the build.

| Budget | Target | How measured |
| --- | --- | --- |
| Idle RSS | ≤ 15 MB | `/proc/self/status` after startup + migrations |
| Peak RSS | ≤ idle + `DOC2MD_MEMORY_BUDGET` + 10% | 64 concurrent 50-page PDFs against the mock provider |
| Container image (core features) | ≤ 25 MB | `docker image inspect` |
| Stripped static binary | ≤ 15 MB | release build size |
| Cold start | ≤ 100 ms to first successful `/rpc` | exec timestamp to first 200 |
| Service overhead per RPC | p99 ≤ 2 ms, excluding fetch and LLM time | `oha` against `capabilities` and `job.get` |
| Read-only RPC throughput | ≥ 20,000 req/s on 4 vCPU | `oha` against `job.get` |
| Concurrent async conversions | ≥ 500 in flight on 2 vCPU / 1 GB, LLM-bound | mock provider with 2–8 s latency |
| Native DOCX, 50 pages | ≤ 50 ms CPU | `criterion` |
| Peak heap per job | ≤ 2× Markdown output size | `dhat` on a 200-page PDF and a large XLSX |
| SQLite fsyncs under load | ≤ 100/s | WAL statistics during the load test |

## 4. Runtime architecture

One process, three execution lanes. A slow lane must never block the others.

```
 Axum /rpc ──► Admission gates ──► Fetch + stage (Tokio I/O, blake3 while streaming)
                                         │
                                         ▼
 LLM gateway ◄──calls──► Extract (CPU pool) ◄──miss── Cache + single-flight
                                │                             │ hit
                                ▼                             ▼
                          Result file + response (streamed JSON on disk/S3)

 DB writer task: one SQLite write connection; every stage sends batched WriteOps
```

1. **I/O lane:** Tokio multi-thread runtime, worker threads = available cores (cgroup-aware). Handles HTTP, fetches, S3, provider requests. Never runs a parser. Keep `max_blocking_threads` small (16).
2. **CPU lane:** a dedicated `rayon` pool (or semaphore-guarded `spawn_blocking`) sized to `std::thread::available_parallelism()`. All XML, ZIP, PDF, Markdown, statistics and chunking work runs here, each job wrapped in `std::panic::catch_unwind` so a malformed document yields `422`, never a crashed process. Keep `panic = "unwind"`.
3. **LLM gateway:** a single component owning all provider traffic, rate limits, retries, priorities and accounting (section 9).

Background jobs are dispatched through an in-memory bounded `tokio::sync::mpsc` channel. The database is the durable record of a job, not its queue; never poll the database for work.

### Module layout

One Cargo package, one binary:

```
src/main.rs        startup, runtime/pool construction, graceful shutdown
src/config.rs      env + .env loading, validation, cgroup limits, defaults
src/rpc.rs         envelope, dispatch, status codes, typed request structs
src/admission.rs   queue, memory-byte semaphore, sync admission, Retry-After
src/source.rs      local/HTTPS/S3/DSN parsing, streaming fetch, format sniffing
src/cache.rs       content-addressed result index, single-flight
src/cpu.rs         CPU pool, catch_unwind wrapper
src/extract/       mod.rs (routing), pdf.rs, image.rs, ooxml.rs (docx/xlsx/pptx), html.rs, text.rs, odf.rs, epub.rs
src/markdown.rs    normalization, single-pass statistics, range-based chunking
src/gateway.rs     token buckets, AIMD, circuit breaker, priority queue, retries
src/llm/           mod.rs (trait + accounting), openai.rs, gemini.rs, compatible.rs, pricing.rs
src/enrich.rs      title, summary, language, translation
src/result.rs      result schema, streaming serialization, artifacts, destinations
src/writer.rs      DB writer task, group commit, WriteOp types
src/db/            mod.rs (repository trait), sqlite.rs, postgres.rs, migrations/
src/jobs.rs        job lifecycle, watch channels for job.wait, restart recovery
src/webhook.rs     delivery, HMAC, retry schedule
src/retention.rs   batched cleanup
src/metrics.rs     Prometheus registry
tests/ benches/ scripts/load/ docs/ Dockerfile .env.example
```

## 5. RPC interface

All operations use `POST /rpc` with `Content-Type: application/json` and `X-ACCESS-TOKEN`. Missing or invalid token → `401`. Compare tokens with `subtle::ConstantTimeEq` against a value hashed once at startup. Cap request bodies at 256 KB with `tower_http::limit::RequestBodyLimitLayer` before parsing (inline text for `summarize`/`chunk` has its own configurable cap, `DOC2MD_MAX_INLINE_TEXT_BYTES`, default 4 MB).

Envelope (RPC-style, not JSON-RPC 2.0):

```json
{ "operation": "convert", "payload": { }, "options": { } }
```

Response wrapper for every operation:

```json
{ "ok": true, "operation": "convert", "request_id": "…", "data": { }, "error": null }
```

Errors use `ok: false` and `error: {code, message, stage}`. Validation failures have `data: null`; failures after processing starts include a failed result in `data` with whatever statistics, call accounting and timings exist.

| Operation | Purpose |
| --- | --- |
| `convert` | Convert a referenced document; async by default, sync with `options.execution: "sync"` |
| `summarize` | Title + summary for `payload.source_content` (preferred) or a text/Markdown source |
| `chunk` | Chunks for `payload.source_content` or a text/Markdown source |
| `job.get` | Job state, result or error, webhook delivery status; `payload.fields` selects a subset |
| `job.wait` | Long-poll until the job changes state or `timeout_ms` (≤ 30,000) elapses |
| `history.list` | Paginated execution history with filters |
| `history.get` | One request's status, stage events, errors, timings and LLM calls (paginated detail) |
| `capabilities` | Supported formats, compiled features, providers, effective defaults and limits |
| `health` | Readiness: DB writer lag, queue depth, provider breaker states |

Extra routes:

- `GET /metrics` (requires token): Prometheus text format.
- `GET /livez` (no token, no DB access): returns `200` if the process is serving.

`summarize` and `chunk` reuse the enrichment and chunking code. Sources needing conversion (PDF, DOCX, HTML over HTTPS, etc.) return `conversion_required` without invoking conversion or OCR. Summary returns `{summary, title}`; chunk returns `{chunks}`. No jobs or artifacts are created.

`payload.metadata` (optional object) is returned unchanged in results and webhooks.

### Status codes

| Code | When |
| --- | --- |
| `200` | Completed RPC call (including `job.get` on a failed job) |
| `202` | Async conversion accepted and durably recorded |
| `400` | Invalid payload/options, unknown operation, malformed cursor |
| `401` | Token failure |
| `404` | Unknown job or request ID |
| `413` | Input exceeds limits, or job larger than the entire memory budget |
| `415` | Upload-like fields, unsupported or legacy format |
| `422` | Unusable document (corrupt, parser panic, scanned pages with OCR off) |
| `429` | Async queue full; include `Retry-After` |
| `502` / `504` | Upstream provider or source failure / timeout |
| `503` | Sync request cannot be admitted, DB writer unavailable for async accept, provider breaker open; include `Retry-After` |

Reject any file-like field (`file`, `content_base64`, `data:` URLs, multipart) anywhere in the JSON with `415`.

### Examples

Async (default):

```json
{
  "operation": "convert",
  "payload": {
    "source": "https://example.com/report.pdf",
    "destination": "s3://output-bucket/report.md.json?region=us-east-1",
    "metadata": { "reference": "report-123" },
    "webhook": {
      "url": "https://example.com/conversion-completed",
      "headers": { "Authorization": "Bearer callback-token" },
      "secret": "hmac-secret",
      "include": "reference"
    }
  },
  "options": {
    "execution": "async",
    "ocr": "on",
    "auto": { "summary": true, "title": true, "chunks": true },
    "llm": { "provider": "openai", "model": "configured-model" }
  }
}
```

`202` response:

```json
{
  "ok": true, "operation": "convert", "request_id": "…",
  "data": { "status": "queued", "job_id": "…", "queue_position": 12, "estimated_start_ms": 900, "cache_hit": false },
  "error": null
}
```

A cache hit on async returns `202` with `status: "completed"` and `cache_hit: true`; the result is immediately available via `job.get`.

Sync: `{"operation":"convert","payload":{"source":"/documents/report.pdf"},"options":{"execution":"sync"}}` returns `200` with the full result in `data`. Destinations, `write_markdown` and webhooks are invalid with sync (`400`).

## 6. Options and defaults

| Option | Default | Behavior |
| --- | --- | --- |
| `execution` | `async` | `async` persists job + result file; `sync` returns output directly, persisting diagnostics only |
| `ocr` | `on` | `on`, `off`, `auto` for PDFs and images |
| `pdf.skip_local_processing` | derived: `true` for `on`, `false` otherwise | Explicit `true` with `auto`/`off` is invalid |
| `auto.summary` / `auto.title` / `auto.chunks` | `true` | Disabled → `summary: null`, `title: null`, `chunks: []`, zero calls for that feature |
| `summary.method` / `title.method` | `llm` | `llm` or `local` |
| `chunks.target_tokens` | `1500` | Structural target; report whether counts are estimated |
| `language.detect` | `false` | Requires `lang-detect` feature for `local` method |
| `language.method` | `local` | `local` or `llm` |
| `translation.target_language` | `null` | Requires `translate` feature |
| `markdown.words_per_page` | `500` | Reading-length estimate only |
| `write_markdown` | `false` | Async only: sibling `.md` next to the `.md.json` destination |
| `overwrite` | `false` | Allow replacing an existing destination |
| `allow_partial` | `false` | Missing extraction content fails unless permitted |
| `cache` | `use` | `use`, `bypass`, `refresh` |
| `priority` | `normal` | Async only: `normal` or `low` |
| `llm.provider`, `llm.model`, `llm.base_url`, `llm.api_key` | from config | Request values override config; never logged or persisted in plaintext |

Requesting an option whose feature was not compiled returns `400 feature_not_compiled` naming the feature.

## 7. Admission control and backpressure

Accept only work the service can finish within its memory budget.

1. **Queue gate:** async jobs enter a bounded channel (`DOC2MD_QUEUE_DEPTH`, default 1024). Full → `429` with `Retry-After` from the observed drain rate. Two internal lanes: `normal` drains before `low`.
2. **Memory gate:** a `tokio::sync::Semaphore` whose permits are KiB (`DOC2MD_MEMORY_BUDGET`, default 60% of the cgroup memory limit read at startup, falling back to 60% of physical memory). Reserve an estimate before fetch from `Content-Length` / S3 `HEAD` / file size × format multiplier (PDF-to-LLM ×1.4, DOCX/PPTX ×4, XLSX ×8, HTML/text ×3; all configurable), and adjust after sniffing. A job bigger than the whole budget → `413`, never a deadlock.
3. **Provider gate:** LLM calls acquire from the gateway; a job waiting on a provider slot holds only its staged temp file, not RAM.
4. **Sync admission:** sync requests skip the queue but must pass gates 2 and 3 immediately or receive `503` + `Retry-After`.
5. **Fairness (optional):** `DOC2MD_MAX_INFLIGHT_PER_KEY` caps in-flight jobs per `metadata` key or source host. Off by default.
6. **Cancellation:** dropping a sync request's future (client disconnect) cancels its fetch and LLM calls; record them as `cancelled` in accounting. Async jobs end only by completion, timeout or shutdown.
7. **Graceful shutdown (SIGTERM/SIGINT):** stop accepting, finish in-flight sync work up to `DOC2MD_SHUTDOWN_GRACE` (30 s), leave queued async jobs `queued`, mark still-running jobs per restart policy, flush the DB writer, exit.

## 8. Sources, formats and extraction

### Sources

- Absolute local path or `file:///absolute/path` (the server's filesystem).
- `https://` only; bounded redirects (5) that must stay HTTPS; connect/read timeouts; streamed byte counting against `DOC2MD_MAX_INPUT_BYTES`.
- `s3://bucket/key` with server AWS credentials.
- S3 DSN `s3://access-key:secret-key@bucket/key?region=us-east-1[&session_token=…]`: parse with the `url` crate, percent-decode credentials, require percent-encoding of reserved characters, default region `us-east-1`. DSN credentials apply only to that operation; never touch process-global AWS credentials. Source and destination DSNs are independent.

Stream every source to a request-scoped temp file under `DOC2MD_DATA_DIR/staging`, hashing with `blake3` in the same pass. Never buffer a whole source in RAM. Sniff the format by signature and package manifest, not extension.

Sanitize everywhere outside the fetch itself: S3 locations without credentials, HTTPS query strings masked. Secrets needed for async execution or restart recovery go into an encrypted column (`chacha20poly1305`, key `DOC2MD_ENCRYPTION_KEY`); no plaintext DSNs or callback headers in any JSON column or log.

### Format policy: modern formats only

- Supported natively: Markdown, plain text, HTML, DOCX, XLSX, PPTX; ODT/ODS/ODP and EPUB behind the `odf-epub` feature.
- **Legacy binary Office formats (.doc, .xls, .ppt and any OLE/CFB file) are not supported in any build.** Detect the CFB signature (`D0 CF 11 E0 A1 B1 1A E1`) and return `415 unsupported_format` with a message asking for the modern equivalent. Do not depend on `calamine`'s XLS support or any CFB parser.
- PNG, JPEG, WebP via LLM vision.
- PDF via LLM by default.

### Native extraction (CPU lane)

- `quick-xml` streaming reader with borrowed events for all OOXML/ODF; `zip` with lazy per-entry reads, enforcing `DOC2MD_MAX_ARCHIVE_EXPANSION` (ratio and absolute bytes) against zip bombs.
- Resolve styles and relationships, presentation slide order, worksheet names, cached formula values, EPUB spine order. Preserve headings, paragraphs, lists, table cells, links and logical order.
- Never execute macros or scripts; never fetch external relationships.
- Report unsupported visuals or layout loss in `warnings`.
- Write Markdown into one `String` pre-sized from an estimate. Normalize to CommonMark with GFM tables.

### PDF

| `ocr` | Path |
| --- | --- |
| `on` | All PDF content transcribed by the LLM; no local text extraction or rendering by default |
| `auto` | Native extraction per page (`pdf-native` feature); LLM OCR for scanned/unusable pages, judged by text quality and image coverage, not a single character threshold |
| `off` | Native only; scanned pages → `422`, or a marked partial result if `allow_partial` |

- Always count pages locally and exactly with a small trailer/xref/page-tree scan in core (independent of `pdf-native`). Never trust an LLM page count.
- `pdf.skip_local_processing=false` with `ocr=on` uses Pdfium page images (`pdf-render` feature) for providers that accept images but not PDFs.
- Batch size per call = floor(model max output tokens ÷ expected tokens per page × 0.8), bounded by provider input limits. Batches run concurrently and reassemble in page order.
- OpenAI-compatible providers declare `pdf`, `image` or `text` support in config. Requesting direct PDF from one that lacks it returns an actionable capability error; never silently switch paths or truncate.

### Images

`on` and `auto` transcribe with LLM vision; `off` on image-only content → `422`. Enforce `DOC2MD_MAX_DECODED_PIXELS` from the header before sending.

## 9. LLM gateway and providers

A provider trait with adapters for OpenAI, Gemini and OpenAI-compatible endpoints. One shared `reqwest::Client` per provider endpoint: `rustls`, HTTP/2, connection pooling, keep-alive.

### Request bodies without copies

Build bodies as streams: JSON prefix → base64 encoder wrapping the staged file reader (64 KB buffer) → JSON suffix, via `reqwest::Body::wrap_stream`. For PDFs above `DOC2MD_PROVIDER_UPLOAD_THRESHOLD` (default 8 MB), use the provider's file-upload API once and reference the file ID across batches and retries, then delete it. Never hold a base64 copy of a document in memory.

### Scheduling and limits

- Token buckets per provider + model + API key for requests/min and tokens/min (`DOC2MD_PROVIDER_<NAME>_RPM`, `_TPM`). Reserve estimated input tokens before sending; reconcile with reported usage.
- Adaptive concurrency (AIMD): start at `DOC2MD_PROVIDER_<NAME>_MAX_CONCURRENCY` (default 64); halve on `429`/`503`; +1 after every 20 consecutive successes. Honor `Retry-After` and `x-ratelimit-*` headers.
- Per-request concurrency cap `DOC2MD_LLM_PER_REQUEST_CONCURRENCY` (default 16), shared across all features of that request.
- Jittered exponential backoff on network errors, `429`, `5xx`, bounded by `DOC2MD_PROVIDER_MAX_RETRIES` (default 3). A retrying call releases its slot while waiting.
- Circuit breaker per endpoint: open after 5 consecutive 5xx/timeouts, half-open after 30 s. Open breaker → fail fast with `503`.
- Priority queue: sync > async extraction > async enrichment > `low` priority jobs.
- Required-stage failure cancels sibling calls and finalizes their call records; optional features apply results as they finish so a deadline keeps completed enrichment.
- Stream responses (SSE) for transcription and translation; append to the output buffer as chunks arrive; detect truncation (`finish_reason=length` or equivalent) and report it instead of returning incomplete output as complete.
- Use provider prompt caching for the fixed system prompt and, where supported, the uploaded PDF.

### Accounting (every attempt)

Record per call attempt: `call_id, stage, purpose, provider, endpoint (sanitized), model, provider_request_id, status, attempt, source_pages_or_chunks, started_at, duration_ms, input_tokens, output_tokens, total_tokens, cached_tokens, reasoning_tokens, usage_source, pricing, estimated_cost, currency, request_summary, response_summary, error`.

- Prefer provider-reported usage. Missing usage is `null`, never zero. Totals disclose incomplete accounting. Do not double-count cached/reasoning subsets.
- Summaries describe scope, task and outcome briefly; never include credentials, full prompts, hidden reasoning or document content.
- `pricing` feature: a dated, versioned price table from config (`DOC2MD_PRICING_JSON` or file), with currency, token categories, tiers and per-request overrides for custom APIs. Use `rust_decimal`. Unknown rates → `null` estimate. Never hard-code undated prices or scrape them.

## 10. Result cache and single-flight

- Key: `blake3(source bytes)` + hash of normalized output-affecting options + provider + model + extractor version.
- Index in the DB (key → result file path, created_at, size) with TTL `DOC2MD_CACHE_TTL` (default 24 h). Hits return immediately with `cache_hit: true` and no LLM calls.
- Single-flight: concurrent identical requests share one in-flight conversion (`DashMap<Key, Shared<BoxFuture>>` or equivalent). Fifty identical submissions = one conversion; each still gets its own `request_id`, and async ones their own `job_id`.
- `cache: "bypass"` skips lookup and storage; `"refresh"` recomputes and replaces.
- Cache entries for results with S3-only storage store the S3 reference; never cache sync-only outputs on disk beyond the request unless `DOC2MD_CACHE_SYNC_RESULTS=true`.

## 11. Enrichment

Run after extraction, concurrently, through the gateway.

- **Combined call by default:** when title and summary (and LLM language detection) are all LLM-based, make one structured-output call returning all three.
- **LLM summary:** bounded map-reduce with recursive reduction only when content exceeds model limits; levels run concurrently within a level and wait for the previous level.
- **Local title:** document metadata → first meaningful heading → filename.
- **Local summary:** clearly labeled extractive summary of leading/key sentences with deterministic length limits.
- **Language (`lang-detect`):** BCP-47 code, method, confidence; `und` when uncertain, `mul` when mixed. Use `whatlang`.
- **Translation (`translate`):** only with `target_language`. Translate Markdown in ordered, concurrent sections preserving headings, tables, code, links, names and numbers. Return `translated_content`, `translated_language`, separate `translated_statistics`; original `content` is untouched. Report translation status separately; never return a partial translation as complete. Titles, summaries and chunks use original content.
- Optional enrichment failure keeps the conversion successful with a warning and `feature_status` entry.

## 12. Results, statistics and timing

### Result schema

```
schema_version, request_id, job_id, status, cache_hit
src_file, dest_file, files, format, metadata
content, title, summary, chunks
language, language_detection
translated_content, translated_language
statistics, translated_statistics
feature_status, warnings, error
llm, timing
```

- `files`: published JSON/Markdown locations with sizes (async); empty for sync. `job_id` and `dest_file` are null for sync.
- Chunks: index, content, heading context, source page/slide/sheet spans when known, token count and estimate method. Internally chunks are `(start, end)` byte ranges into the content buffer plus heading context; materialize text only during serialization. Split large tables and code blocks without losing content.

### Statistics (single pass)

Compute words, characters, bytes and chunk boundaries in one scan of the final Markdown.

- `original_total_pages`: exact for PDFs; slide count for presentations; image count for images; `null` for unpaginated formats unless reliable metadata exists. Plus `original_page_count_method` and `original_page_count_exact`. Never invent pages from text length.
- `markdown_estimated_total_pages`: `ceil(total_words / words_per_page)`, 0 for empty content, with the divisor included.
- `total_words`: Unicode word segmentation (`unicode-segmentation`), documented method.
- `total_characters`: Unicode scalar values including whitespace.
- `content_bytes`, plus sheet/slide counts where relevant.

### Timing

`queue_ms`, `processing_ms`, `total_ms`, stage durations (fetch, extraction, enrichment, translation, storage), `llm_wall_ms` (union of active attempt intervals), `llm_call_sum_ms`, `llm_retry_wait_ms`. Monotonic clock (`Instant`). Webhook timing recorded separately.

### Serialization and storage

- Async results serialize once, streaming (`serde_json::to_writer` over `BufWriter<File>`), to `DOC2MD_DATA_DIR/results/<job_id>.json`, then optionally publish to the destination (`.md.json`, sibling `.md` with `write_markdown`). Local writes are atomic (temp + rename); S3 writes use conditional puts when `overwrite=false`. Publish all required artifacts before marking completion. Never claim a destination was written when storage failed.
- `job.get` streams the result file into the response; it does not load it into memory. `payload.fields` (e.g. `["status","statistics","llm"]`) returns only those fields by streaming through the file with a selective deserializer.
- Enforce `DOC2MD_MAX_RESULT_BYTES` while serializing.
- Responses above 4 KB are compressed (`tower_http::compression`, zstd and gzip).

## 13. Persistence

The database records what happened; it is not on the hot path. Only one write must be durable before replying: accepting an async job.

### Single writer with group commit

One task owns the write connection. Handlers send `WriteOp` values through a bounded channel. The writer drains up to 500 ops or 10 ms, whichever comes first, and commits them in one transaction. Callers needing durability (async accept) await a `oneshot` for their batch; all others fire and forget. Expose writer lag and batch sizes as metrics.

SQLite: bundled `libsqlite3-sys`, WAL, `synchronous=NORMAL`, `busy_timeout=5000`, `temp_store=MEMORY`, `mmap_size=268435456`, cached prepared statements, a separate read pool of 4 connections. Use `sqlx` with compile-checked queries; migrations embedded and run at startup.

### What each operation writes

| Operation | Writes |
| --- | --- |
| `convert` async | Job + request log in one awaited batch before `202`; state transitions, events and call records batched as they happen |
| `convert` sync | Request log, events and calls batched, written after the response is sent |
| `summarize` / `chunk` | Request log + calls, batched after response |
| `job.get`, `job.wait`, `history.*`, `capabilities` | Metrics + tracing span only; optional sampled log rows via `DOC2MD_LOG_READS` (`off`, `all`, `sample:0.01`; default `off`) |
| `health`, `/livez`, `/metrics` | Nothing |
| Rejected requests (400/401/415) | Batched log row, rate-capped (`DOC2MD_REJECT_LOG_PER_SEC`, default 50) so floods cannot fill the DB |

Never store the access token, raw rejected bodies, Markdown content, chunk bodies, translations or prompts in history. Stage events are buffered per job and flushed with state transitions.

If the writer channel is full or the DB is down: async `convert` → `503`; sync conversion and reads keep working and emit a structured fallback log.

### Tables

`request_logs` (monotonic sequence, request ID, operation, mode, sanitized source/options/metadata, request status, execution status/stage, job ID, HTTP status, times, timings, statistics, usage/cost totals, warnings, safe error, result availability), `request_events`, `jobs` (with encrypted secrets column and result path), `llm_calls`, `webhook_deliveries`, `result_cache`. Index sequence, created_at, operation, mode/status, request ID, job ID, cache key.

Request status is separate from execution status: an async submission is `accepted` while its job is `queued → running → completed | partial | failed`. Polling never overwrites a job's state.

### Jobs and restart

- `job.wait` and internal waiters use a `tokio::sync::watch` per active job.
- On restart: requeue `queued` jobs into the channel; mark interrupted `running` jobs `failed` with reason `interrupted` (never silently re-run billed LLM work); mark unfinished request logs `interrupted`.

### History pagination

`history.list` with `limit` (default 25, max 100), opaque `cursor`, and filters (`operation`, `execution`, `execution_status`, `created_from` inclusive, `created_to` exclusive, UTC). Newest-first keyset pagination on the request sequence. The first page captures a high-water sequence; the cursor (base64 of a small signed struct: high-water, last sequence, filter hash) excludes newer rows so pagination never duplicates or runs forever. Reject malformed cursors or changed filters with `400`. No total-count query. End page: `has_more: false`, `next_cursor: null`. `history.get` paginates events and calls the same way.

### PostgreSQL (`postgres` feature)

Same repository trait and logical schema, batch inserts with `UNNEST`, run against the same contract test suite.

### Retention

`retention.rs` runs every 60 s, deleting in batches of 1000: staging, results, jobs and cache entries after `DOC2MD_RESULT_RETENTION` (24 h); history after `DOC2MD_HISTORY_RETENTION` (30 days). Keep pending webhook records until delivered or exhausted. History marks expired results. Delete only service-owned objects, never inputs or caller destinations.

## 14. Webhooks (`webhooks` feature)

- Supplied entirely in `payload.webhook`: `url` (HTTPS), optional `headers`, optional `secret`, `include: "reference" | "full"` (default `reference`). Invalid with sync.
- `reference` payload: event ID, job/request IDs, terminal status, file locations, metadata, statistics, LLM totals, timing, error. `full` streams the complete result file, subject to `DOC2MD_MAX_RESULT_BYTES`.
- Delivery starts after the terminal state is persisted. Headers: `x-doc2md-event-id` (stable for deduplication), `x-doc2md-timestamp`, `x-doc2md-signature` (HMAC-SHA256 over timestamp + body when a secret is set).
- 10 s timeout; retry network errors, `429` and `5xx` with bounded backoff (e.g. 6 attempts over ~1 h); 2xx is success; at-least-once. Attempts and status appear in `job.get`. Webhook failure never changes a successful conversion's status.
- Callback headers are encrypted at rest and never logged.

## 15. Feature flags

Use `default-features = false` on every dependency and enable only what is used.

| Feature | Contents | Default |
| --- | --- | --- |
| core (always) | Axum RPC, auth, SQLite, local + HTTPS sources, PDF/image via LLM, OpenAI/Gemini/compatible, Markdown/text/HTML/DOCX/XLSX/PPTX, chunks, local + LLM title/summary, history, cache, metrics | on |
| `s3` | S3 sources/destinations and DSNs (`object_store` with only `aws`) | on |
| `webhooks` | Delivery, HMAC, retries | on |
| `pdf-native` | Native PDF text, `auto`/`off` modes (`lopdf` or `pdf`) | on |
| `pricing` | Dated price table and cost estimates | on |
| `translate` | Translation | off |
| `lang-detect` | `whatlang` detection | off |
| `odf-epub` | ODT/ODS/ODP/EPUB | off |
| `pdf-render` | Pdfium page rendering (dynamic library) | off |
| `postgres` | PostgreSQL repository | off |

`capabilities` reports compiled features, supported formats (explicitly listing legacy Office as unsupported), provider capabilities, effective defaults and limits.

## 16. Build, configuration and observability

### Cargo profiles

```toml
[profile.release]
opt-level = 3
lto = "fat"
codegen-units = 1
strip = "symbols"
panic = "unwind"

[profile.release-small]
inherits = "release"
opt-level = "s"
```

- Global allocator: `mimalloc`.
- Targets: `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` via `cargo-zigbuild`; fully static.
- TLS: `rustls` only (no OpenSSL anywhere in the dependency tree; verify with `cargo tree`).
- Dockerfile: multi-stage; final stage `gcr.io/distroless/static` (or `scratch` + CA bundle), non-root, read-only root filesystem, one volume for `DOC2MD_DATA_DIR`. Publish `doc2md-llm-ws:core` and `doc2md-llm-ws:full`.
- CI: `cargo fmt --check`, `cargo clippy -- -D warnings` for core and all-features, tests, `cargo deny`, `cargo udeps`, `cargo bloat` report, image size and idle RSS checks, benchmark gates.
- Commit `Cargo.lock`. Pick current compatible versions at bootstrap.

### Configuration

Load `.env` (via `dotenvy`) with real environment variables taking precedence; works identically with Docker `--env-file` and `-e`. Validate everything at startup and fail fast with clear messages. `.env.example` documents every variable and default; `.env` is git-ignored. Generate a random token into `.env` on first local setup via a script, never in the binary.

Core variables (add others as needed, all prefixed `DOC2MD_`): `TOKEN`, `BIND` (default `0.0.0.0:8080`), `DATA_DIR`, `DATABASE_URL`, `ENCRYPTION_KEY`, `MEMORY_BUDGET`, `QUEUE_DEPTH`, `MAX_INPUT_BYTES`, `MAX_RESULT_BYTES`, `MAX_INLINE_TEXT_BYTES`, `MAX_PDF_PAGES`, `MAX_DECODED_PIXELS`, `MAX_ARCHIVE_EXPANSION`, `JOB_TIMEOUT`, `SHUTDOWN_GRACE`, `LLM_PER_REQUEST_CONCURRENCY`, `PROVIDER_<NAME>_{API_KEY,BASE_URL,MODEL,RPM,TPM,MAX_CONCURRENCY,TIMEOUT,MAX_RETRIES,CAPABILITIES}`, `DEFAULT_PROVIDER`, `PROVIDER_UPLOAD_THRESHOLD`, `CACHE_TTL`, `RESULT_RETENTION`, `HISTORY_RETENTION`, `LOG_READS`, `REJECT_LOG_PER_SEC`, `PRICING_JSON`, `LOG_LEVEL`, `LOG_FORMAT`.

### Observability

- `tracing` with JSON output; one span per request and per LLM call; `request_id` on every event; secrets redacted by type (wrap secrets in a `Secret<T>` newtype whose `Debug`/`Display`/`Serialize` print `***`).
- Metrics: queue depth per lane, memory permits in use, CPU-pool busy threads, per-provider in-flight / AIMD ceiling / 429s / bucket fill / latency histograms / breaker state, DB writer batch size and lag, cache hit ratio and single-flight joins, jobs by state, conversion duration by format, bytes fetched.

## 17. Build order and acceptance gates

Each phase ends releasable, with tests green.

| Phase | Deliverable | Acceptance gate |
| --- | --- | --- |
| 1. Foundation + measurement | Cargo workspace setup, config, Axum `/rpc` with envelope/auth/body cap, `/livez`, `/metrics`, SQLite with migrations, DB writer task, mock provider (configurable latency, 429/5xx injection, usage reporting), benchmark harness (`criterion`, `oha` scripts, k6 burst script, memory capture) | Token/upload/invalid-option rejection tests; writer batches under load; benchmarks run and report |
| 2. Lanes + native formats | CPU pool with `catch_unwind`; local/HTTPS streaming intake with blake3; format sniffing incl. CFB rejection; Markdown/text/HTML/DOCX/XLSX/PPTX extraction; single-pass statistics; range-based chunking; sync `convert` end-to-end | A 300-page XLSX does not raise `capabilities` p99 above 5 ms; DOCX budget met; `.doc/.xls/.ppt` → `415`; fixtures pass |
| 3. Jobs + admission | Async `convert`, bounded queue lanes, memory semaphore, `job.get` (streamed, field selection), `job.wait`, restart recovery, graceful shutdown, `429/503 + Retry-After` | ≥ 20k req/s on `job.get`; burst test returns `429`s without latency collapse for admitted work; kill -9 loses no queued job and re-runs no running job |
| 4. LLM gateway + PDF/images | Provider trait, OpenAI/Gemini/compatible adapters, streamed request bodies, file-upload path, token buckets, AIMD, breaker, priorities, SSE, accounting, pricing; PDF `on/auto/off`, exact page count, adaptive batching, ordered reassembly; image vision | 500 concurrent PDFs on 2 vCPU / 1 GB without OOM; under injected 429s throughput settles at the provider limit with zero failed jobs; peak heap ≤ 2× output for a 200-page PDF; accounting complete including failed/cancelled attempts |
| 5. Enrichment | Combined title/summary/language call, local methods, map-reduce summary, `summarize`/`chunk` operations, `lang-detect`, `translate` | Disabled features make zero calls; translation preserves original content and structure; partial translations never reported complete |
| 6. Cache | Result cache, single-flight, `cache` option | 50 identical concurrent submissions → 1 provider conversion; hits make zero LLM calls |
| 7. Storage + webhooks | S3 sources/destinations/DSNs, atomic/conditional writes, `.md.json` + `.md`, encrypted secrets, webhooks with HMAC and retries, retention, history list/get with stable pagination | DSN percent-encoding and region fallback work; webhook retries deduplicate by event ID; polling agrees with callback; pagination stable while inserts happen |
| 8. PostgreSQL, ODF/EPUB, release | `postgres` repository passing the shared contract suite; `odf-epub`; `pdf-render`; static musl builds; distroless images; docs | Core image ≤ 25 MB; idle RSS ≤ 15 MB; binary ≤ 15 MB; cold start ≤ 100 ms; all budgets in section 3 pass in CI |

## 18. Testing requirements

- Unit + integration tests for: RPC envelope, auth, upload rejection, every status code; async default and sync paths; source and DSN parsing; format sniffing and CFB rejection; native fixtures for every claimed format; PDF routing per mode; exact page counts; statistics on multilingual text; chunk splitting of large tables/code; provider usage normalization and missing-usage handling; pricing arithmetic; AIMD and bucket behavior; breaker transitions; cancellation finalizing call records; cache keys and single-flight; writer batching and durability ack; restart recovery; history filters, cursors, page boundaries and concurrent inserts; retention independence; webhook signing, retry and dedup; secret redaction in logs, DB rows and responses.
- Property tests (`proptest`) for Markdown chunking (no content lost, order preserved) and DSN parsing.
- Fuzz targets (`cargo-fuzz`) for format sniffing, OOXML parsing and the PDF page counter.
- SQLite and PostgreSQL run the same repository contract suite (PostgreSQL via an opt-in env var in CI).
- Live-provider tests are opt-in and budget-capped; normal CI uses the mock provider only.

## 19. Definition of done

- Every phase gate in section 17 and every budget in section 3 passes in CI.
- `cargo fmt`, `clippy -D warnings` (core and all-features), `cargo deny` and all tests pass.
- No OpenSSL, Clap, local OCR, Redis, CFB/legacy Office parser, or unused dependency in the tree.
- `docs/` contains: API reference with examples for every operation, configuration reference, format support matrix (stating legacy Office is unsupported), deployment guide (Docker, resource sizing, provider rate-limit tuning), and the benchmark results with hardware details.
- `README.md` covers quick start in under 10 commands.

## 20. Working rules

- Measure before optimizing and after: record benchmark numbers in `docs/benchmarks.md` at the end of each phase.
- Prefer typed structs over `serde_json::Value` in hot paths; borrow instead of clone; avoid `Arc<Mutex<_>>` on hot paths (use channels, atomics, `DashMap`, or sharding).
- No blocking calls on Tokio worker threads; enforce with `clippy::await_holding_lock` and a debug-build blocking detector.
- Every limit is configurable, has a safe default, and fails clearly.
- Never log, persist or return credentials, signed URL queries, callback headers or provider keys.
- Keep commits small and phase-scoped, with tests in the same commit as the code they cover.
- When a requirement here conflicts with an efficiency budget, keep the requirement and find an efficient implementation; if truly impossible, document the trade-off in `docs/decisions.md` and continue.