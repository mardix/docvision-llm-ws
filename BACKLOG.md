# Backlog

Ideas that were discussed and shelved. Nothing here is scheduled; each item is something to think about and may eventually be built.

<br>

## Large results

**The problem:** a result can be big. A 10 MB Markdown document becomes 20 MB or more of JSON, because the text appears twice (`content`, plus every chunk's own text) alongside the LLM call records. Today the whole result is sent whenever a caller asks for it, which is the intended behavior for now (the content is wanted for full-text indexing, e.g. SQLite FTS5).

Where a large result travels today:

| Path | Sends the content? |
| --- | --- |
| The `202` reply to `convert` | No |
| Webhook, default event | No (status, paths, sizes, statistics) |
| Webhook with `"$"` or `"$.content"` in the body template | Yes |
| `job.get` | Yes, the whole result by default |
| Destination files | Written to disk or S3, not sent |

What already helps, with no changes:

- A webhook body template that picks only what's needed, e.g. `{"id": "$.metadata.ref", "title": "$.title", "content": "$.content"}`, or `"$.chunks"` instead of `content` when indexing per chunk. Sending both doubles the payload.
- A destination plus the default webhook event: the receiver reads `md_file` / `docv_file` itself, so nothing large goes through the webhook.
- `job.get` with `fields` returns only the listed top-level fields.
- `/rpc` responses are compressed (zstd or gzip) when the client sends `Accept-Encoding`.

### 1. Opt-in gzip for webhook bodies

- **What:** a webhook option such as `"compress": "gzip"` that sends the body with `Content-Encoding: gzip`. Markdown shrinks about 4–5×.
- **Why opt-in:** a sender can't know whether the receiver accepts compressed request bodies, and many frameworks don't decompress them automatically.
- **Notes:** the gzip library (`flate2`) is already in the build. The signature should cover the bytes as sent (the compressed body). Whole-result bodies are streamed from disk today, so compression should stream too rather than load the file.

### 2. Webhook timeout that scales with body size

- **What:** keep 10 s for small bodies and allow more for large ones, e.g. 10 s plus 1 s per MB, capped at a few minutes.
- **Why:** each attempt has a fixed 10 s limit for the whole request. A large body on a slow link times out, and all 6 retries then re-send the full body.
- **Related:** receivers often cap request bodies (nginx defaults to 1 MB). A `413` is not retried, so the delivery ends as `failed`. Worth documenting, and possibly surfacing more clearly in the delivery status.

### 3. Make `job.get` light by default

- **What:** return status, paths, sizes, title, summary, statistics, usage and timing, but not `content`, `chunks` or `translated_content` unless asked (`fields`, or a new `include` list).
- **Why:** polling a job stays small however large the document is.
- **Cost:** a breaking change for callers that rely on getting `content` without asking.

### 4. Plain download for the Markdown

- **What:** e.g. `GET /_/jobs/{job_id}/content`, token-protected, streamed as `text/markdown` and compressed in transit.
- **Why:** callers without a destination get the raw file directly, with no JSON wrapper to download and parse.

### 5. Chunks without their text

- **What:** an option to return each chunk as start/end positions into `content` (plus headings, pages and token count) instead of repeating the text.
- **Why:** removes the duplication for callers who want both `content` and `chunks` in one response.

### Decided against

- **Dropping `content` automatically above some size.** A field that sometimes disappears is a surprise for callers. An explicit rule (light by default, ask for the heavy parts) behaves the same for a 1 KB note and a 10 MB book.

<br>

## Anthropic provider

The `anthropic` provider sends PDFs and images inline (base64), which covers pieces up to Anthropic's 32 MB request limit. Not built yet:

- **Files API for very large pieces.** Upload a piece once and reference it by ID, as the OpenAI and Gemini adapters do. Only matters for PDFs whose ~16-page pieces exceed roughly 20 MB (image-heavy scans).
- **Prompt caching.** The fixed instructions are far below the minimum cacheable size today. Worth revisiting if prompts grow, or for repeated extraction against the same schema.
- **Refusal fallback.** When Claude declines a document the request fails with the reason. Anthropic offers server-side fallback to another model for the newest models; it is a beta feature and was left out.
- **Effort as an option.** Effort is fixed (low for transcription and summaries, medium for extraction). It could become a request option if quality on hard scans needs it.

<br>

## Zip

`create_zip`, `zip_only` and `job.zip` are built (v0.4.0). Left out:

### 1. Rewrite the destination's JSON result after `job.zip`

- **What:** when `job.zip` builds a zip later, update `zip_bytes` in the `.docv.json` at the destination (and the local result file), not just in `job.get` and the webhook.
- **Why:** today the destination's JSON still says `zip_bytes: 0`, so anyone who reads only the destination files can't tell the zip exists.
- **Notes:** `docv_bytes` is self-referential, so the file has to be rewritten until its size is stable, as at conversion time. With `overwrite: false` the rewrite would conflict with the existing file.

### 2. Resume `zip.*` webhooks after a restart

- **What:** pending `zip.completed` / `zip.failed` deliveries are retried after a restart, as `job.finished` deliveries are.
- **Why:** a restart during the retry window drops the event. The workaround is to call `job.zip` again, which replies at once with the existing zip.
- **Notes:** the zip event IDs end in `_zip-completed`, so `resume` skips them today. It would need the event kind stored, or parsed back from the ID, and the zip state from `<job_id>.zip.json`.

### 3. Body templates for zip events

- **What:** let `webhook.body` (or a separate `zip_body`) shape the `zip.*` events.
- **Why:** receivers built around a custom `job.finished` template get zip events in the fixed shape.

### 4. Update history when `job.zip` builds a zip

- **What:** record the new `zip_bytes` on the request's history entry.
- **Why:** history (and the dashboard's request details) only shows zips made during conversion; a later zip shows as "—".

### 5. Optional compression

- **What:** e.g. `"zip_compression": "deflate"`.
- **Why:** entries are stored uncompressed because pictures barely shrink. A zip that is mostly text (long Markdown, few or no images) would shrink about 4–5×.

### Decided against

- **Zips for sync conversions.** A sync conversion has no job to keep the zip with, so `create_zip` and `zip_only` require async.

<br>

## Input formats

### Decided against

- **HEIC / HEIF (iPhone photos).** There is no reliable pure-Rust decoder: support would need the system `libheif` (LGPL, plus an HEVC decoder with patent questions), or sending HEIC only to Gemini, which accepts it natively. Callers convert to PNG, JPEG or WebP instead; HEIC returns `415 unsupported_format`.
