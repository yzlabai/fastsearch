# Connecting document ingestion (for non-Rust callers)

> 🌏 中文：[接入文档摄取.md](接入文档摄取.md)
>
> fastsearch's contract is: **the caller owns parsing and chunking, the engine owns retrieval**
> (see the [responsibility-boundary ADR](governance/2026-08-24-职责边界-不承担身份与控制面.md)).
> Having pushed parsing out to callers, we owe them the **contract** — this page is it.
> Every field and error message below was verified against a running server, not inferred from structs.

## TL;DR

```python
from fastsearch_client import FastsearchClient, chunks_from_docparse, chunk_text
c = FastsearchClient(base_url="http://127.0.0.1:8642", api_key="dev")

# (A) You have a parser: any tool's chunks → adapt → index
c.index("kb", "report.pdf", chunks_from_docparse(parsed, doc_id="report.pdf"),
        store_media="object")

# (B) Plain text / markdown: just chunk it
c.index("kb", "notes.md", chunk_text(text, doc_id="notes.md"))
```

TypeScript has the same API: `chunksFromDocparse(parsed, { docId })` / `chunkText(text, { docId })`.

## Field mapping — only three things change

| Parser output | fastsearch chunk | Note |
|---|---|---|
| `id` | `chunk_id` | the **only** rename |
| `kind` / `text` / `page` / `bbox` / `heading_path` / `section_id` / `char_len` | same name, same meaning | pass through |
| `image.data_base64` | `media.asset = {"kind":"inline"}` + **`media_bytes`** | see the pitfall below |
| `image.file` | `media.asset = {"kind":"object","uri":…}` | bytes already in object storage |
| (image, neither present) | `media.asset = {"kind":"doc_region",…}` | can only jump back to the source region |
| — | `doc_id` | you inject it (the parser doesn't know the document id) |

## ⚠️ The pitfall: `media_bytes` is a **byte array**, not a base64 string

Parsers hand you base64; fastsearch wants bytes. Sending the string fails:

```
400 ... chunks[0]: invalid type: string "iVBORw0KGgo=", expected a sequence
```

The correct shape is `"media_bytes": [137, 80, 78, 71, ...]`. The SDK helpers do this for you.

## ⚠️ Do not set `tenant` / `acl` on chunks

The server overwrites them from the API key (`apply_ingest_identity`), so whatever you set is
ignored. To control visibility, **use a different key**. (The MCP `index_chunks` tool goes further
and *rejects* smuggled identity rather than silently overwriting it.)

## `store_media`

`"auto"` (server default) uploads to object storage when configured, otherwise inline ·
`"object"` forces upload · `"inline"` stores bytes in the source of truth (**requires
`DATABASE_URL`**, otherwise `/v1/asset` 404s for inline) · `"none"` drops the bytes.
The server body limit is **20MB** — a whole PDF as inline base64 will exceed it.

## Plain text: coordinates are placeholders

`chunk_text` gives `page=1` and an all-zero `bbox` — plain text has no layout. If you want
`resolve_citation` to highlight inside the original document, you need a real parser (path A).

## Verifying, and what is not possible yet

`search` returns `citation_id` + `page` + `bbox`; `resolve_assets` returns a short-lived URL —
which **may be a relative path** when the server has no public base configured, so join it against
your base URL.

Not available yet, stated plainly: **you cannot hand a raw file to the server** (no upload endpoint;
`/v1/index` takes chunks only), and **there is no document ledger** (`GET /v1/chunks` needs the
`doc_id` up front and requires PostgreSQL). Both are covered by KB-3 in the iteration plan —
designed, not implemented.
