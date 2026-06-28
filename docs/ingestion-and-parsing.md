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

# 2) Ingest a file (parser chosen by extension) → on-disk index
./target/debug/fastsearch ingest --data ./idx --collection kb --doc-id report.docx report.docx
./target/debug/fastsearch search --data ./idx --collection kb --query "gross margin" --json
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
| `--features parse` | + multi-format parsers (9 formats + images) | lightweight, pure-Rust, no ONNX |
| `--features parse-ocr` | + **PP-OCR text extraction** for scans/images | heavy (tract/ONNX) |
| `--features parse-tables` | + **non-VLM table structure recognition** (UniRec ONNX) | heavy (tract/ONNX + pure-Rust rasterization) |

> The heavy tiers (parse-ocr/parse-tables) only affect the **ingestion side**; the search/server binary can keep
> using the default lean build.

---

## Scanned / image OCR (`--features parse-ocr`)

Scans, text-layer-less PDFs, and images → **PP-OCR** (ONNX) extracts the text before indexing. Born-digital
documents that already have a text layer **do not trigger OCR** (saves compute).

```bash
cargo build -p fastsearch-cli --features parse-ocr --bin fastsearch
FASTSEARCH_OCR_MODELS=/path/to/models/ppocr-v5 \
  ./target/debug/fastsearch ingest --data ./idx --collection kb --doc-id scan.png scan.png
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
  ./target/debug/fastsearch ingest --data ./idx --collection kb --doc-id r.pdf r.pdf
# stderr: "UniRec: 重识别 N 个表格结构（非 VLM）"
```

- Detected table regions → pure-Rust rasterize + crop → **UniRec** re-recognizes structure as an HTML table → replace in the index.
- Corresponds to docparse-cli's `--unirec` (local ONNX route), as opposed to `--vlm-tables` (the VLM route).
- env **`FASTSEARCH_UNIREC_MODELS`** points at the UniRec model dir.
- ⚠️ **Performance**: UniRec is a 2000-token autoregressive decode — **a single complex table can take minutes on CPU**; use a GPU for bulk.

---

## Not wired yet (next iteration)

- **VLM natural-image captioning** (`parse-vlm`): caption figures/charts; needs an OpenAI-compatible VLM service (e.g. Ollama llava).
- **Formula → LaTeX** (same UniRec model), **standalone layout enhancement**: same ONNX route, can follow.

---

## After ingestion

Whichever entry point you use, the output is a uniform `Chunk` (`kind`/`page`/`bbox`/`heading_path` +
`tenant`/`acl`), after which it's standard retrieval — keyword / vector / hybrid, hits carrying **page+bbox
citations**. See [Using fastsearch in an Agent](using-fastsearch-in-an-agent.md).

> The source of truth is Postgres: the production path is "write PG → logical-replication CDC → engine derived
> index"; the CLI `ingest`/`index` write the local derived index directly, for offline/single-box demos. Both
> paths produce the same chunk schema (the `from_docparse_chunk` adapter aligns them at compile time).
