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
