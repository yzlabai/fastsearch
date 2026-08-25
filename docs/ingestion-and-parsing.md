# Ingestion & parsing (fastsearch ingest)

> 🌏 中文版: [文件解析与摄取.md](文件解析与摄取.md)

> [docparse](../vendor/docparse) is **subtree-merged into this repo** (fusion Option B), so `fastsearch ingest <file>`
> parses many document formats into traceable chunks and indexes them **in-process** — no external parsing
> service, no shelling out intermediate JSON.
> **Search itself stays dependency-free**: parsing is a cargo feature, opt-in. Without it the binary is lean and
> carries no parsing/ONNX dependency at all.

---

## TL;DR

```bash
# 1) Build the CLI with parsing (multi-format, lightweight, no ONNX)
cargo build -p fastsearch-cli --features parse --bin fastsearch

# 2) Ingest a file: CLI parses client-side (parser chosen by extension) → POST /v1/index (server must be running)
./target/debug/fastsearch ingest --server http://localhost:8642 --key dev --collection kb --doc-id report.docx report.docx
./target/debug/fastsearch search --server http://localhost:8642 --key dev --collection kb --query "gross margin" --json
```

Supported formats (`--features parse`): **PDF · DOCX · HTML · Markdown · CSV · XLSX · PPTX · SRT (subtitles) · EML (email) · images**.

---

## Three ingest entry points (pick per need)

| Entry | Command | Use when | Deps |
|---|---|---|---|
| **Multi-format parse** | `ingest <file>` (`--features parse`) | You have the raw file and want in-process parsing | lightweight, no ONNX |
| **External chunks** | `index <chunks.json>` | You already produced chunks (JSON/NDJSON) with docparse/another tool | none (default build) |
| **Plain-text folder** | `index-dir <dir>` | A pile of `.md`/`.txt`, quick loop | none |

All three follow the same path — "adapt into the source-of-truth `Chunk` (with `tenant`/`acl`) → on-disk index →
search"; `ingest` just adds the in-process parsing step.

---

## Build tiers (feature-gated)

| Build | Includes | Weight |
|---|---|---|
| `cargo build` (default) | Search hot path (four faces + hybrid retrieval + ACL + CDC) | **zero docparse/ONNX** |
| `--features parse` | + multi-format parsers (9 formats + images) + **raw image bytes** (see below) | lightweight, pure-Rust, no ONNX |
| `--features parse-ocr` | + **PP-OCR text extraction** for scans/images | heavy (tract/ONNX) |
| `--features parse-tables` | + **non-VLM table structure recognition** (UniRec ONNX) | heavy (tract/ONNX + pure-Rust rasterization) |
| `--features parse-vlm` | + **VLM region recognition** (tables as HTML / region-level transcription), **needs an external service** | heavy (shares the tract-side orchestration) + a GPU service |

> The heavy tiers (parse-ocr/parse-tables/parse-vlm) only affect the **ingestion side**; the search/server binary can keep
> using the default lean build.

---

## Scanned / image OCR (`--features parse-ocr`)

Scans, text-layer-less PDFs, and images → **PP-OCR** (ONNX) extracts the text before indexing. Born-digital
documents that already have a text layer **do not trigger OCR** (saves compute).

```bash
cargo build -p fastsearch-cli --features parse-ocr --bin fastsearch
FASTSEARCH_OCR_MODELS=/path/to/models/ppocr-v5 \
  ./target/debug/fastsearch ingest --server http://localhost:8642 --key dev --collection kb --doc-id scan.png scan.png
# stderr: "OCR: 1/1 页经增强（PP-OCR）"
```

- env **`FASTSEARCH_OCR_MODELS`** points at a PP-OCR model dir (`*det*.onnx` + `*rec*.onnx` + char dict).
- Models are not shipped with the repo; fetch them via docparse's `scripts/fetch-models.sh` (`ppocr-v5`/`ppocr-v6`…).

---

## Tables / chart understanding (**no VLM** — `--features parse-tables`)

**To be clear**: table/formula/layout **structure** uses **deterministic ONNX models** (UniRec/SLANet/layout
detection) — **no VLM needed**. VLM is only for the **semantic description** of natural images/charts ("what does
this line chart say"); that's the part needing an external HTTP service.

```bash
cargo build -p fastsearch-cli --features parse-tables --bin fastsearch
FASTSEARCH_UNIREC_MODELS=/path/to/models/unirec \
  ./target/debug/fastsearch ingest --server http://localhost:8642 --key dev --collection kb --doc-id r.pdf r.pdf
# stderr: "UniRec: 重识别 N 个表格结构（非 VLM）"
```

- Detected table regions → pure-Rust rasterize + crop → **UniRec** re-recognizes structure as an HTML table → replace in the index.
- Corresponds to docparse-cli's `--unirec` (local ONNX route), as opposed to `--vlm-tables` (the VLM route).
- env **`FASTSEARCH_UNIREC_MODELS`** points at the UniRec model dir.
- ⚠️ **Performance**: UniRec is a 2000-token autoregressive decode — **a single complex table can take minutes on CPU**; use a GPU for bulk.

---

## VLM region recognition (`--features parse-vlm`, **needs an external service**)

The same orchestration and the same `RegionReader` seam as the UniRec route above — only the
recognition backend changes, to an OpenAI-compatible HTTP service (vLLM / SGLang / LM Studio). It is
for the pages UniRec can't crack: hard academic tables, CJK design layouts.

**Coordinates survive**: region geometry still comes from layout/table detection and the VLM only
*reads*, so `resolve_citation`'s in-page highlighting keeps working. (A whole-page end-to-end parse
would drop body-text coordinates, which is why this project does **not** take that route.)

```bash
# Service side, once: e.g. vLLM serving a page-parsing model
vllm serve ATH-MaaS/OvisOCR2 --port 8000

cargo build -p fastsearch-cli --features parse-vlm --bin fastsearch
FASTSEARCH_VLM_URL=http://localhost:8000 FASTSEARCH_VLM_MODEL=OvisOCR2 \
  ./target/debug/fastsearch ingest --server http://localhost:8642 --key dev --collection kb --doc-id r.pdf r.pdf

# Add a layout model to also enable region-level whole-page transcription
FASTSEARCH_LAYOUT_MODEL=/path/to/models/layout-ppv2/PP-DoclayoutV2_simp.onnx \
  ./target/debug/fastsearch ingest ...
```

| env | required | effect |
|---|---|---|
| `FASTSEARCH_VLM_URL` | ✅ | service base URL |
| `FASTSEARCH_VLM_MODEL` | ✅ | model name as the service knows it |
| `FASTSEARCH_VLM_KEY` | — | bearer token |
| `FASTSEARCH_LAYOUT_MODEL` | — | layout ONNX path; **set it to enable transcription**, otherwise tables only |
| `FASTSEARCH_VLM_MAX_PAGES` | — | per-document page cap sent to the VLM (default 50) |

- **Capability follows configuration**: the two required env vars give table re-extraction; a layout model adds transcription.
- **Precedence**: the VLM pass runs before UniRec, and UniRec skips tables whose `source` already starts with `table:vlm:` — so both can be configured (VLM wins, UniRec backfills) without double inference or blind overwrites.
- **PDF only** (needs source bytes to rasterize, same as parse-tables); image scans keep going through `parse-ocr`.
- **Failure degrades**: unreachable/timeout/junk answer → the deterministic result stands, parsing does not fail.
- ⚠️ **Not verified against a real model yet** (`待运行验证`): the mock end-to-end passes (request shape, HTML table rowspan/colspan expansion, `source` tagging), but the quality/speed gates have not been run — see [the integration spec §7](plans/2026-07-27-OvisOCR2接入需求分析与功能设计.md).

---

## Raw image bytes (`--images`, 2026-08-25)

A parsed figure is more than a rectangle on a page — the **original bytes** are the foundation for
"show the figure behind a citation", image-to-image search, and visual embeddings. `ingest` now carries
them along:

```bash
# Default: bytes ride along with the index request; the server puts them in object storage
./target/debug/fastsearch ingest --server http://localhost:8642 --key dev \
  --collection kb --doc-id r.pdf r.pdf
# stderr: images: 1 张字节已随索引上传（3 KiB）；1 张需 PNG 重编码（Rgb8/Gray8，本版不支持）；0 张解析层无字节

# Fetch the original back (server needs FASTSEARCH_OBJECT_DIR + FASTSEARCH_ASSET_SIGNING_KEY)
curl -sL -H "Authorization: Bearer dev" http://localhost:8642/v1/asset/kb:r.pdf:0 -o figure.jpg
```

| `--images` | Where the bytes go | When |
|---|---|---|
| `object` (default) | Server uploads to object storage; PG keeps only the uri | Normal case. `/v1/asset/{citation_id}` signs a short-lived URL that serves the original |
| `inline` | Server inlines them into PG `bytea` | Small crops, or when the bytes should ride the logical-replication stream. **Requires the server to have `DATABASE_URL`**; a whole PDF's base64 easily blows the server's 20MB body limit, hence opt-in |
| `none` | Nothing collected | Text-only retrieval. The PDF backend does not even turn on `decode_images` — behavior is **field-for-field identical** to before this feature |

**Coverage (stated honestly)**:

| Source | `ImageKind` | Status |
|---|---|---|
| JPEG inside a PDF (DCTDecode passthrough) | `Jpeg` | ✅ bytes pass through verbatim, zero new dependencies |
| DOCX / PPTX / HTML media files | `Encoded` | ✅ bytes pass through verbatim, MIME from the source |
| **Raw bitmaps** decoded out of Flate/CCITT/JBIG2/JPX in a PDF | `Rgb8` / `Gray8` | ❌ **not supported in this iteration** (see below) |
| Unsupported encodings (CMYK JPEG, multi-channel JPX…), below the size gate | `None` | ❌ the parser never produced bytes |

The last two are **never dressed up as embedded**: the chunk records
`image_vector_status = missing_bytes` and the CLI reports the counts on stderr. Coordinates are always
preserved (`page`/`bbox`/`region` untouched), so `resolve_citation` highlighting and the `/v1/asset`
`doc_render` jump-to-source still work.

> **Why `Rgb8`/`Gray8` is not supported yet**: those are raw pixel buffers that need PNG encoding to be
> usable as images. The ready-made encoder is `docparse_vlm::encode_png_rgb` — pulling it in would drag
> `docparse-vlm` (+`docparse-raster`) into the **lightweight `parse` profile**, breaking its
> "pure-Rust, no ONNX, no rasterization" promise; writing our own encoder means new `png`/`flate2`
> dependencies. Both need their own evaluation, so this iteration ships Jpeg/Encoded and labels the rest.

---

## Not wired yet (next iteration)

- **VLM natural-image captioning**: caption figures/charts (docparse has `--vlm-describe`; not surfaced on the fastsearch ingest side).
- **`Rgb8`/`Gray8` raw-bitmap byte loop**: blocked on deciding how a PNG encoder can arrive without weighing down the `parse` profile (see above).
- **Batched upload for large documents**: `ingest` POSTs a whole document in one request today; an image-heavy PDF can hit the server's 20MB body limit (the CLI warns on stderr past 16MB).
- **Formula → LaTeX** (same UniRec model), **standalone layout enhancement**: same ONNX route, can follow.

---

## After ingestion

Whichever entry point you use, the output is a uniform `Chunk` (`kind`/`page`/`bbox`/`heading_path` +
`tenant`/`acl`), after which it's standard retrieval — keyword / vector / hybrid, hits carrying **page+bbox
citations**. See [Using fastsearch in an Agent](using-fastsearch-in-an-agent.md).

> The source of truth is Postgres: the production path is "write PG → logical-replication CDC → engine derived
> index"; the CLI `ingest`/`index` write the local derived index directly, for offline/single-box demos. Both
> paths produce the same chunk schema (the `from_docparse_chunk` adapter aligns them at compile time).
