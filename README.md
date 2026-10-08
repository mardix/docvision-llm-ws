# docvision-llm-ws

A lean HTTP service that converts documents to Markdown, and can extract structured JSON from them.

- **PDFs and images** are transcribed by an LLM (OpenAI, Gemini or any OpenAI-compatible API).
- **DOCX, XLSX, PPTX, HTML, Markdown, text** (plus ODT/ODS/ODP/EPUB with `odf-epub`) are parsed natively, without an LLM.
- Each result includes the Markdown, a title, a summary, chunks, statistics, LLM usage and timings.
- **Structured data:** pass a JSON Schema, and the result also carries the data it describes (a receipt's merchant and total, an invoice's line items, …).

One binary, ~10 MiB idle, ~25 ms startup, SQLite by default.

<br>

## Quick start

```bash
./scripts/setup-env.sh          # creates .env with a random token
./scripts/run-local.sh          # builds and starts on :4242
./scripts/run-local.sh --mock   # same, with a fake LLM (no API key needed)
```

<br>

## Configuration

Set these as environment variables or in `.env`. Only `DOCVISION_TOKEN` is required.

| Variable | Default | Description |
| --- | --- | --- |
| `DOCVISION_TOKEN` | — | Access token, at least 16 characters |
| `DOCVISION_BIND` | `0.0.0.0:4242` | Listen address |
| `DOCVISION_DATA_DIR` | `./data` | Database, downloads, results |
| `DOCVISION_DATABASE_URL` | SQLite in `DATA_DIR` | `postgres://…` needs the `postgres` feature |
| `DOCVISION_ENCRYPTION_KEY` | derived from token | 32 bytes, base64. Encrypts stored secrets |
| `DOCVISION_LOG_LEVEL` | `info` | |
| `DOCVISION_LOG_FORMAT` | `json` | `json` or `text` |
| `DOCVISION_MEMORY_BUDGET` | 60% of memory | Size or percent |
| `DOCVISION_MAX_CONCURRENT_JOBS` | `512` | Async jobs running at once |
| `DOCVISION_QUEUE_DEPTH` | `1024` | Async jobs waiting. When full: `429` + `Retry-After` |
| `DOCVISION_MAX_INPUT_BYTES` | `200MiB` | Largest source document |
| `DOCVISION_JOB_TIMEOUT` | `15m` | |
| `DOCVISION_RESULT_RETENTION` | `24h` | Results, jobs and cache |
| `DOCVISION_HISTORY_RETENTION` | `30d` | Request history |
| `DOCVISION_S3_ENDPOINT` | — | S3-compatible store (MinIO, R2, …) |

Sizes accept `KB`, `MiB`, `GB`, …. Durations accept `ms`, `s`, `m`, `h`, `d`.

### Default LLM

Requests can override any of these with the `llm_*` options.

| Variable | Default | Description |
| --- | --- | --- |
| `DOCVISION_LLM_PROVIDER` | `openai` | `openai`, `gemini`, or any name for an OpenAI-compatible API |
| `DOCVISION_LLM_MODEL` | — | The LLM is enabled once this is set |
| `DOCVISION_LLM_API_KEY` | — | |
| `DOCVISION_LLM_BASE_URL` | public endpoint | Required for providers other than `openai` and `gemini` |
| `DOCVISION_LLM_MAX_CONCURRENCY` | `64` | Calls in flight. Halved automatically on `429`/`503` |
| `DOCVISION_LLM_MAX_RETRIES` | `3` | Retries on network errors, `429` and `5xx` |
| `DOCVISION_LLM_MAX_OUTPUT_TOKENS` | `16384` | Model output limit. Also sets the PDF pages per call |

### S3 credentials

- **Server credentials:** for plain `s3://bucket/key`, set `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_REGION` (and `AWS_SESSION_TOKEN` if needed). On AWS, an IAM role works without keys.
- **Per request:** `s3://KEY:SECRET@bucket/key?region=eu-west-1&session_token=…`. Percent-encode `/`, `+` and `@` in the keys.
- **Region:** `?region=` if given. Otherwise plain `s3://` uses `AWS_REGION`, and URLs with credentials use `us-east-1`.

<br>

## Endpoints

| Route | Auth | Purpose |
| --- | --- | --- |
| `POST /rpc` | token | All operations (below) |
| `GET /_/dashboard` | token, entered in the page | Dashboard: live activity, in-progress requests, paginated request history and failures (filter by requester, operation, format, status), and forms to run `convert`, `summarize`, `chunk` and `extract` |
| `GET /_/doc` | none | This README as a web page |
| `GET /metrics` | token | Prometheus metrics |
| `GET /livez` | none | `200 ok` while the process is up |

The token goes in the `X-ACCESS-TOKEN` header.

<br>

## Request and response

```json
{
  "operation": "convert",
  "payload": {},
  "options": {}
}
```

Every response has the same envelope:

```json
{
  "ok": true,
  "operation": "convert",
  "request_id": "01a10e96-…",
  "data": {},
  "error": null
}
```

On failure, `ok` is `false` and the error looks like `{"code": "…", "message": "…", "stage": "…"}`. If processing had started, `data` holds the partial result.

**No uploads.** Reference documents by path, URL or S3 location. Base64, `data:` URLs and multipart bodies return `415`.

<br>

## Operations

| Operation | Purpose |
| --- | --- |
| `convert` | Convert a document (async by default) |
| `summarize` | Title and summary for text |
| `chunk` | Split text into chunks |
| `extract` | Structured JSON from text, following a JSON Schema |
| `job.get` | A job's state and result |
| `job.wait` | Wait until a job changes state |
| `history.list` | Past requests, newest first |
| `history.get` | One request with its events and LLM calls |
| `stats` | Activity summary (what the dashboard shows) |
| `health` | Readiness, version and compiled features |

<br>

### convert

```json
{
  "operation": "convert",
  "payload": {
    "source": "https://example.com/report.pdf",
    "destination": "s3://bucket/out/report.md",
    "metadata": {
      "ref": "abc-123"
    },
    "requester_id": "billing-service",
    "webhook": {
      "url": "https://example.com/hook"
    }
  },
  "options": {
    "execution": "async"
  }
}
```

**Payload fields**

| Field | Description |
| --- | --- |
| `source` | **Required.** Absolute path, `file:///…`, `https://…`, or `s3://bucket/key` |
| `destination` | Async only. A folder (ending in `/`) or a `.md` path, local or S3 (details below) |
| `metadata` | Any object. Returned in results and webhooks |
| `requester_id` | Who is asking: a name or ID, up to 128 characters. Default `unknown`. Used to filter history and in `stats` (also accepted by `summarize`, `chunk` and `extract`) |
| `webhook` | Async only. One webhook or an array of them. See [Webhooks](#webhooks) |

**Sources:** `https://` always works. `http://` is accepted only for `localhost`/`127.0.0.1`. Redirects must stay on HTTPS.

**Destination:** two files are written, the Markdown and the full JSON result.

- **A folder** (ending in `/`): both are named after the source file, keeping its extension, so documents with the same name but different formats never collide.

  ```
  "destination": "s3://bucket/out/"      source: …/hello.pdf

  s3://bucket/out/hello.pdf.md           ← markdown
  s3://bucket/out/hello.pdf.docv.json    ← full result
  ```

- **A `.md` file:** the Markdown goes there, and the JSON is written next to it as `<name>.docv.json`.

  ```
  "destination": "/out/report.md"

  /out/report.md            ← markdown
  /out/report.docv.json     ← full result
  ```

Existing files are replaced. Set `overwrite: false` to keep them instead; the job then fails with `409 destination_exists`.

**Response:** async returns `202` with this `data`. Sync returns `200` with the full [result](#result).

```json
{
  "status": "queued",
  "job_id": "01a10e96-…",
  "queue_position": 1,
  "estimated_start_ms": 0,
  "cache_hit": false
}
```

<br>

### Options

All options are optional, and all are top-level keys in `options`.

**Execution**

| Option | Default | Values |
| --- | --- | --- |
| `execution` | `async` | `async`, `sync` |
| `priority` | `normal` | `normal`, `low` (async only) |
| `cache` | `use` | `use`, `bypass`, `refresh` |
| `allow_partial` | `false` | Accept partial extraction (`status: "partial"`) |
| `overwrite` | `true` | Replace existing destination files. `false` fails the job with `409 destination_exists` instead |

**PDF and images**

| Option | Default | Values |
| --- | --- | --- |
| `ocr` | `on` | `on`: LLM reads every page. `auto`: native text, LLM for scanned pages. `off`: native text only |
| `pdf_skip_local_processing` | `true` with `ocr: on` | `false` renders pages to images first (`pdf-render` feature) |

**How PDFs are processed**

- The page count is read from the PDF itself, so it is always exact. Limit: 2,000 pages.
- Pages are transcribed in batches of about 16, sized to the model's output limit (`DOCVISION_LLM_MAX_OUTPUT_TOKENS` ÷ ~800 tokens per page), all running in parallel.
- **Each batch gets its own small PDF** cut from the original, holding only its pages. Calls pay input tokens for their own pages only, and provider per-file limits (size, page count) apply to each piece, not the whole document.
- A piece over 8 MB is uploaded to the provider's Files API instead of being sent inline. Pieces and uploads are removed when the job ends.
- If a batch's output is cut off at the model's limit, the piece is split in half and retried, down to single pages.
- PDFs that can't be split (encrypted or malformed) are sent whole, with each call naming its page range.
- With `ocr: auto`, only the scanned pages are sent, as pieces of their own.
- The LLM receives the PDF itself, so it uses the text layer and the page images. Page-by-page images are only sent with `pdf_skip_local_processing: false` (`pdf-render` feature).

**Enrichment**

| Option | Default | Values |
| --- | --- | --- |
| `gen_title` | `true` | Off: `title: null` |
| `gen_summary` | `true` | Off: `summary: null` |
| `gen_chunks` | `true` | Off: `chunks: []` |
| `title_method` | `llm` | `llm`, `local` |
| `summary_method` | `llm` | `llm`, `local` (extractive) |
| `chunk_size` | `512` | Tokens (≈ characters ÷ 4). 128 to 200000; smaller values become 128 |
| `chunk_overlap` | 15% of `chunk_size` | Tokens repeated from the previous chunk. `0` disables it |
| `detect_language` | `false` | |
| `language_method` | `local` | `local` (`lang-detect` feature), `llm` |
| `translate_to` | `null` | A BCP-47 tag (`translate` feature) |
| `words_per_page` | `500` | For `markdown_estimated_total_pages` |

**Chunking:** chunks follow the document's structure. A heading starts a new chunk, and paragraphs, tables and code are split at line boundaries. A heading is never returned on its own: it stays with the text under it, even when a sub-heading comes first. Chunks under a quarter of `chunk_size` are merged into a neighbour. Each chunk's `headings` holds its heading path.

**Local title:** the document's own title (Office/ODF/EPUB metadata, HTML `<title>`, Markdown `# `), else the first heading, else the file name. If an LLM title fails, the local title is used and `feature_status.title.method` is `local_fallback`.

**LLM**

| Option | Default | Description |
| --- | --- | --- |
| `llm_provider` | `DOCVISION_LLM_PROVIDER` | `openai`, `gemini`, or a custom name |
| `llm_model` | `DOCVISION_LLM_MODEL` | |
| `llm_api_key` | `DOCVISION_LLM_API_KEY` | Never logged, stored encrypted |
| `llm_base_url` | `DOCVISION_LLM_BASE_URL` | Required for custom (OpenAI-compatible) providers |

A provider other than the default needs its own `llm_model` and `llm_api_key`. The default key is never sent to another provider.

**Structured extraction**

| Option | Default | Description |
| --- | --- | --- |
| `extract_schema` | `null` | A JSON Schema with `"type": "object"` at the top, up to 16 KiB. The result's `extracted` field follows it |

See [Structured extraction](#structured-extraction) for how it works.

<br>

### Result

```json
{
  "schema_version": 1,
  "request_id": "01a10e96-…",
  "job_id": "01a10e96-…",
  "status": "completed",
  "cache_hit": false,
  "src_file": "https://example.com/report.pdf",
  "dest_file": "s3://bucket/out/report.md",
  "files": [
    {
      "kind": "result_json",
      "location": "s3://bucket/out/report.docv.json",
      "bytes": 48213
    },
    {
      "kind": "markdown",
      "location": "s3://bucket/out/report.md",
      "bytes": 20117
    }
  ],
  "metadata": {
    "ref": "abc-123"
  },
  "format": "pdf",
  "content": "# Quarterly Report\n\n…",
  "title": "Quarterly Report Q3",
  "summary": "Revenue grew …",
  "extracted": null,
  "chunks": [
    {
      "index": 0,
      "content": "…",
      "headings": ["Quarterly Report"],
      "source_span": {
        "kind": "page",
        "from": 1,
        "to": 2
      },
      "tokens": 498,
      "tokens_estimated": true
    }
  ],
  "language": null,
  "translated_content": null,
  "statistics": {
    "original_total_pages": 12,
    "markdown_estimated_total_pages": 8,
    "total_words": 3912,
    "total_characters": 24310,
    "content_bytes": 24655,
    "chunk_count": 5
  },
  "feature_status": {
    "title": {
      "status": "completed",
      "method": "llm"
    },
    "summary": {
      "status": "completed",
      "method": "llm"
    }
  },
  "warnings": [],
  "error": null,
  "llm": {
    "calls": [],
    "totals": {
      "provider": "openai",
      "model": "your-model",
      "calls": 2,
      "failed_calls": 0,
      "input_tokens": 9100,
      "output_tokens": 5300,
      "total_tokens": 14400,
      "cached_tokens": 0,
      "reasoning_tokens": null,
      "usage_complete": true
    }
  },
  "timing": {
    "queue_ms": 12,
    "processing_ms": 21480,
    "total_ms": 21492,
    "stages": {
      "fetch_ms": 310,
      "extraction_ms": 18350,
      "enrichment_ms": 2600,
      "translation_ms": 0,
      "storage_ms": 40
    }
  }
}
```

- `status` is `completed`, `partial` or `failed`.
- `extracted` holds the structured data when `extract_schema` was given (see below), otherwise `null`.
- `feature_status` reports each optional feature (`title`, `summary`, `chunks`, `language`, `translation`, `extraction`) as `completed`, `failed`, `skipped` or `disabled`, with the method used and any error.
- `llm.calls` has one record per attempt (provider, model, status, tokens, duration), including retries and failures.
- Token counts the provider did not report are `null`, never `0`. The service does not calculate prices; use the token counts.

<br>

### summarize

```json
{
  "operation": "summarize",
  "payload": {
    "source_content": "# Notes\n\nWe agreed to …"
  },
  "options": {
    "title_method": "local"
  }
}
```

- Pass the text in `source_content`, or a text/Markdown file in `source`. `requester_id` and `metadata` are optional.
- Options: `gen_title`, `gen_summary`, `title_method`, `summary_method`, `llm_*`.
- Returns `title`, `summary`, `feature_status`, `warnings` and `llm`.

<br>

### chunk

```json
{
  "operation": "chunk",
  "payload": {
    "source_content": "# Guide\n\n…"
  },
  "options": {
    "chunk_size": 800,
    "chunk_overlap": 80
  }
}
```

- Returns `{"chunks": [...]}`, in the same format as the result's chunks.
- Inline text up to 4 MiB is accepted by `summarize`, `chunk` and `extract`. Other requests are limited to 256 KB.

<br>

### extract

Structured JSON from text or Markdown you already have. To extract from a PDF, image or Office file, use `convert` with `extract_schema` instead.

```json
{
  "operation": "extract",
  "payload": {
    "source_content": "Corner Café … Latte x2 9.00 … Total 9.00 USD",
    "requester_id": "expenses"
  },
  "options": {
    "extract_schema": {
      "type": "object",
      "required": ["merchant", "total"],
      "properties": {
        "merchant": { "type": "string" },
        "total": { "type": "number" },
        "currency": { "type": ["string", "null"] }
      }
    }
  }
}
```

- `source_content` or a text/Markdown `source`, as in `summarize`.
- `extract_schema` is required. The `llm_*` options are accepted.
- Returns `extracted`, `feature_status`, `warnings` and `llm`.
- If the model can't produce data that matches the schema, it returns `422 extraction_failed`.

<br>

### Structured extraction

Add `extract_schema` to `convert` (or use `extract` for plain text) to get JSON back alongside the Markdown. For example, converting a receipt photo:

```json
{
  "operation": "convert",
  "payload": {
    "source": "s3://bucket/receipts/r-1042.jpg"
  },
  "options": {
    "execution": "sync",
    "extract_schema": {
      "type": "object",
      "required": ["merchant", "total"],
      "properties": {
        "merchant": { "type": "string" },
        "date": { "type": ["string", "null"], "description": "YYYY-MM-DD" },
        "currency": { "type": ["string", "null"] },
        "total": { "type": "number" },
        "items": {
          "type": "array",
          "items": {
            "type": "object",
            "properties": {
              "name": { "type": "string" },
              "qty": { "type": "number" },
              "price": { "type": "number" }
            }
          }
        }
      }
    }
  }
}
```

The result keeps `content` (the Markdown) and adds:

```json
{
  "extracted": {
    "merchant": "Corner Café",
    "date": "2026-10-04",
    "currency": "USD",
    "total": 9.0,
    "items": [
      { "name": "Latte", "qty": 2, "price": 4.5 }
    ]
  },
  "feature_status": {
    "extraction": { "status": "completed", "method": "llm" }
  }
}
```

**How it works**

- The document is converted to Markdown first. One extra LLM call then reads that Markdown and returns JSON for your schema, so it works for every format.
- The model is told to use only facts from the document and to return `null` for anything that is missing, never to guess. Use `["string", "null"]` style types for fields that may be absent.
- OpenAI enforces the schema with structured outputs. Gemini and OpenAI-compatible APIs use JSON mode.
- Every answer is checked against `type`, `properties`, `required`, `items` and `enum`. Other keywords (`description`, `format`, …) guide the model but aren't checked. If the answer doesn't match, the model gets one retry with the reason.
- **If extraction fails,** the conversion still succeeds: `extracted` is `null`, `feature_status.extraction` is `failed` with the reason, and a warning is added. On `extract`, a failure returns `422 extraction_failed`.
- **Requirements:** `extract_schema` needs an LLM. Without one configured (or passed in `llm_*`), the request is rejected with `400 provider_not_configured`.
- **Long documents:** content beyond the model's input budget is cut, with a warning.
- **Caching:** the schema is part of the cache key, so a different schema means a new extraction.
- **Webhooks:** templates can pick fields, e.g. `{"total": "$.extracted.total"}`.

<br>

### job.get

```json
{
  "operation": "job.get",
  "payload": {
    "job_id": "01a10e96-…",
    "fields": ["status", "content", "title"]
  }
}
```

- Returns `job_id`, `request_id`, `status` (`queued`, `running`, `completed`, `partial`, `failed`), timestamps, `error`, `webhooks` (delivery status) and `result`.
- `result` is `null` until the job is done.
- `fields` limits the result to those top-level fields.

<br>

### job.wait

```json
{
  "operation": "job.wait",
  "payload": {
    "job_id": "01a10e96-…",
    "timeout_ms": 30000
  }
}
```

Waits for a state change (30 s at most). Returns the same data as `job.get` without `result`, plus `changed: true|false`.

<br>

### history.list

```json
{
  "operation": "history.list",
  "payload": {
    "limit": 25,
    "cursor": null,
    "operation": "convert",
    "execution": "async",
    "execution_status": "completed",
    "requester_id": "billing-service",
    "format": "pdf",
    "in_progress": false,
    "created_from": "2026-10-01T00:00:00Z",
    "created_to": "2026-10-02T00:00:00Z"
  }
}
```

- Every filter is optional. `limit` is 1–100.
- `in_progress: true` returns only requests that are `queued` or `running`.
- `format` filters by detected document format (`pdf`, `docx`, `xlsx`, `pptx`, `html`, `markdown`, `text`, `png`, `jpeg`, `webp`, …).
- Each item includes `requester_id`, `format`, `llm_provider` and `llm_model` (`null` when no LLM was used), status, timings, statistics and token usage.
- Returns `{"items": [...], "has_more": true, "next_cursor": "…"}`.
- Pass `next_cursor` back as `cursor` to get the next page.
- History never stores tokens, credentials, document content or prompts.

<br>

### history.get

```json
{
  "operation": "history.get",
  "payload": {
    "request_id": "01a10e96-…",
    "limit": 100
  }
}
```

Returns `request` (the same fields as a `history.list` item, plus options and metadata), `events` (stage timeline) and `calls` (LLM attempts). Events and calls are paginated with `events_cursor` and `calls_cursor`.

<br>

### stats

```json
{
  "operation": "stats",
  "payload": {
    "window": "24h"
  }
}
```

- `window` is `1h`, `24h`, `7d` or `30d`.
- Returns request counts by status, operation, format, top requesters (`by_requester`) and models (`by_model`); duration avg/p50/p95/max; pages and words; LLM calls and tokens; recent failures; and a live snapshot (running, queued, memory, provider state).

<br>

### health

```json
{
  "operation": "health"
}
```

- Returns `status`, `version`, `features`, `uptime_ms`, `db_writer`, `queue`, `memory` and the `providers` breaker states (`closed`, `open`, `half_open`).
- Returns `503 not_ready` when the database writer is down or lagging.

<br>

## Webhooks

Called after an async job finishes (`webhooks` feature). `payload.webhook` is one webhook object, or an array of up to 10. Each endpoint is delivered independently, with its own event ID, retries and status.

```json
[
  {
    "url": "https://example.com/hook",
    "headers": {
      "Authorization": "Bearer xyz"
    },
    "secret": "hmac-secret",
    "body": {
      "doc_id": "$.metadata.ref",
      "markdown": "$.content",
      "title": "$.title",
      "first_chunk": "$.chunks[0].content",
      "source": "docvision"
    }
  },
  {
    "url": "https://example.com/notify",
    "fire_and_forget": true
  }
]
```

| Field | Description |
| --- | --- |
| `url` | `https://` (`http://` only for localhost) |
| `headers` | Extra headers, up to 32. Stored encrypted |
| `secret` | Adds an `x-docvision-signature` header |
| `body` | Optional JSON template, up to 16 KiB |
| `fire_and_forget` | `false`. When `true`, one attempt and no retries. Any HTTP response marks it `sent` (the status code is ignored). Only a connection failure marks it `failed` |

**Body template**

| Value | Becomes |
| --- | --- |
| `"$.content"`, `"$.metadata.ref"` | That field of the [result](#result). Missing fields become `null` |
| `"$.chunks[0].content"` or `"$.chunks.0.content"` | An array item |
| `"$"` | The whole result. As the entire `body`, the result file is streamed as-is |
| `"$.event_id"`, `"$.event"` | Event ID and `"job.finished"` |
| `"$$text"` | The literal string `"$text"` |
| anything else | Sent unchanged |

Without `body`, a short event is sent: `event_id`, `event`, `job_id`, `request_id`, `status`, `files`, `metadata`, `statistics`, `llm` (totals), `timing` and `error`.

**Headers sent**

| Header | Description |
| --- | --- |
| `x-docvision-event-id` | Same on every retry; use it to deduplicate |
| `x-docvision-timestamp` | Unix seconds |
| `x-docvision-signature` | `sha256=` + hex HMAC-SHA256(`secret`, `timestamp + "." + body`) |

**Delivery status** (in `job.get` → `webhooks`, one entry per endpoint, in request order): `pending`, `delivered`, `sent` (fire-and-forget) or `failed`.

**Retries:** network errors, `429` and `5xx` are retried at 0 s, 30 s, 2 min, 10 min, 20 min and 30 min. Other `4xx` responses are not retried. Delivery is at-least-once, and a failed webhook never changes the job status.

<br>

## Errors

| HTTP | Codes |
| --- | --- |
| `400` | `invalid_json`, `invalid_envelope`, `unknown_operation`, `invalid_payload`, `invalid_options`, `invalid_source`, `invalid_destination`, `invalid_webhook`, `invalid_cursor`, `feature_not_compiled`, `provider_not_configured`, `conversion_required` |
| `401` | `unauthorized` |
| `404` | `job_not_found`, `request_not_found`, `source_not_found` |
| `409` | `destination_exists` |
| `413` | `body_too_large`, `input_too_large`, `inline_text_too_large`, `result_too_large`, `archive_too_large`, `image_too_large`, `too_many_pages`, `job_too_large` |
| `415` | `upload_not_supported`, `unsupported_media_type`, `unsupported_format` (includes legacy `.doc`/`.xls`/`.ppt`) |
| `422` | `corrupt_document`, `unusable_document`, `scanned_pages`, `ocr_required`, `partial_extraction`, `render_failed`, `extraction_failed` |
| `429` | `queue_full` (with `Retry-After`) |
| `502` | `source_fetch_failed`, `source_read_failed`, `provider_error`, `provider_auth_failed`, `destination_write_failed` |
| `503` | `busy`, `db_unavailable`, `provider_unavailable`, `not_ready` |
| `504` | `source_timeout`, `provider_timeout`, `job_timeout` |

<br>

## Security

The access token grants full use of the service, so treat it like a password and give it only to trusted callers. With it, a caller can:

- **Read local files** the service can read (by absolute path). `/proc`, `/sys` and `/dev` are always refused, after symlinks are resolved, so the process environment and its secrets can't be read. In Docker the service sees only its image and what you mount; mount document folders read-only.
- **Write destinations** (`.md` plus `.docv.json`) anywhere the service can write, replacing existing files of those names unless `overwrite: false`. In Docker, that's `/data` and any writable mounts.
- **Fetch URLs** over `https://` (and `http://` on localhost only), including hosts on your internal network. Webhooks follow the same rule. Run the service where it can only reach what it should.

What the service guarantees:

- The token is compared in constant time. Tokens, API keys, S3 credentials and webhook headers/secrets are never logged and never stored in plain text: job secrets are encrypted, and history keeps no credentials, signed URL queries, document content or prompts.
- The server's LLM API key is only ever sent to the configured LLM endpoint. A request that points `llm_base_url` elsewhere must bring its own `llm_api_key`.
- API keys echoed in provider error messages are masked.
- Uploads, archives (zip-bomb limits), images (pixel limits), PDFs (page limits) and request bodies are bounded. The Docker image runs as non-root on a read-only filesystem.

<br>

## Build features

| Feature | Default | Adds |
| --- | --- | --- |
| `s3` | on | S3 sources and destinations |
| `webhooks` | on | Completion webhooks |
| `pdf-native` | on | Native PDF text (`ocr: auto` / `off`) and per-batch PDF splitting |
| `translate` | on | `translate_to` |
| `lang-detect` | on | Local language detection |
| `odf-epub` | on | ODT, ODS, ODP, EPUB |
| `postgres` | on | PostgreSQL (SQLite stays the default database) |
| `pdf-render` | off | Render PDF pages to images for image-only models. Needs the Pdfium library at runtime, so it isn't in the static binary or Docker image |

Everything except `pdf-render` is in the default build (about 11.9 MB). To add it, or to build a smaller binary without some features:

```bash
cargo build --release --features pdf-render
cargo build --release --no-default-features --features s3,webhooks,pdf-native
```

<br>

## Docker

**Docker Compose** (`docker-compose.yml` in the repo):

```yaml
services:
  docvision-llm-ws:
    image: ghcr.io/mardix/docvision-llm-ws:latest
    container_name: docvision-llm-ws
    restart: unless-stopped
    environment:
      DOCVISION_TOKEN: ${DOCVISION_TOKEN:?set DOCVISION_TOKEN}
      DOCVISION_LLM_PROVIDER: ${DOCVISION_LLM_PROVIDER:-openai}
      DOCVISION_LLM_MODEL: ${DOCVISION_LLM_MODEL:-}
      DOCVISION_LLM_API_KEY: ${DOCVISION_LLM_API_KEY:-}
      DOCVISION_LLM_BASE_URL: ${DOCVISION_LLM_BASE_URL:-}
    ports:
      - "4242:4242"
    volumes:
      - docvision-data:/data
      # - ./documents:/documents:ro
    read_only: true
    mem_limit: 512M
    cpus: 1.0

volumes:
  docvision-data:
```

```bash
DOCVISION_TOKEN=your-long-random-token DOCVISION_LLM_MODEL=your-model DOCVISION_LLM_API_KEY=your-key docker compose up -d
```

Compose also reads these values from a `.env` file next to `docker-compose.yml`. Any other `DOCVISION_*` setting from [Configuration](#configuration) can be added under `environment`.

**Plain Docker:**

```bash
docker build -t docvision-llm-ws .
docker run --read-only -v docvision-data:/data -p 4242:4242 --env-file .env docvision-llm-ws
```

- The image is distroless and runs as non-root. `/data` is the only folder it writes to.
- Mount documents read-only, e.g. `-v /srv/docs:/docs:ro`, then use `"source": "/docs/a.pdf"`.
- On SIGTERM, running work gets 30 s to finish. Queued jobs resume on restart.

<br>

## Development

Requires Rust 1.88 or newer (`rustup update`, or `brew upgrade rust`).

```bash
./scripts/run-local.sh --debug     # fast rebuilds
cargo test --all-features          # uses a mock LLM
```

Set `CARGO_TARGET_DIR` outside synced folders (Dropbox, iCloud). `run-local.sh` uses `~/.cache/docvision-llm-ws/target` by default.
