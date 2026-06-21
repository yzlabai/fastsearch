---
name: docparse-document-intelligence
description: >
  Parse, convert, chunk, and analyze documents with docparse-rs — a pure-Rust,
  single-binary, zero-runtime-dependency, deterministic parser that extracts
  positioned structured content (text / layout / reading order, with PDF-user-space
  bbox per element). Use this skill when the user gives a document — PDF, DOCX,
  HTML, XLSX, PPTX, Markdown, CSV, SRT, TeX, EML, image (PNG/JPG/…), or AsciiDoc —
  and wants to: extract text or structured content, convert to Markdown / JSON /
  plain text, chunk for RAG ingestion (page + bbox + heading breadcrumb per chunk),
  inspect layout/reading-order/tables, OCR a scanned PDF, or re-extract tables and
  formulas. Triggers: "parse this PDF", "convert to markdown", "chunk for RAG",
  "extract tables", "analyze document structure", "OCR this scan", "prepare for
  ingestion", "get citable text with coordinates", "process document".
license: Apache-2.0
compatibility: >
  Requires the `docparse` binary (pure Rust, no runtime deps). Build from this
  repo with `cargo build --release` → `./target/release/docparse`, or use
  `docparse` if it is already on PATH. Optional model files (OCR / layout / table /
  formula) are opt-in and only needed for the enhancement flags.
metadata:
  author: docparse-rs
  version: "1.0"
  upstream: docparse-rs (this repo)
allowed-tools: Bash(docparse:*) Bash(./target/release/docparse:*) Bash(cargo:*)
---

# docparse-rs Document Intelligence Skill

Use this skill to parse, convert, chunk, and analyze documents with **docparse-rs**
via its **`docparse` CLI** — no Python, no services, no GPU. The default fast path
extracts document *structure* (text, layout, reading order) without rendering
pixels or touching any model; neural enhancers (OCR / layout / table / formula /
VLM) are **opt-in** and only fire when you pass their flags.

**Design identity to keep in mind when answering:** pure Rust single binary,
deterministic and reproducible (same input + same format ⇒ byte-identical output
across CLI / MCP / REST), and every element carries a citable bbox in PDF user
space. Models are optional enhancements — digital documents never invoke one.

## Scope

| Task | Covered |
|---|---|
| Parse PDF / DOCX / HTML / XLSX / PPTX / MD / CSV / SRT / TeX / EML / image / AsciiDoc | ✅ |
| Convert to Markdown / plain text | ✅ |
| Export structured JSON IR (pages → elements with bbox, font size, tag, span, source) | ✅ |
| Chunk for RAG (page + bbox + heading breadcrumb + section_id per chunk) | ✅ (`-f chunks`) |
| Document structure tree for agentic navigation (nested sections, citable) | ✅ (`-f outline`, MCP `outline`) |
| Export a git-native, citable OKF knowledge bundle | ✅ (`-f okf`, `--okf-tar`, MCP `export_okf`) |
| OCR scanned / image-only PDFs | ✅ (`--ocr`, opt-in) |
| Re-extract tables (merged cells / multi-row headers) | ✅ (`--table-model` local, or `--vlm-tables` service) |
| Formulas → LaTeX | ✅ (`--formula-model`) |
| Macro reading-order rerank (complex / CJK layouts) | ✅ (`--layout`, `--transcribe-model`) |
| Image export / base64 embed | ✅ (`--image-dir`, `--image-embed`) |
| Figures as retrievable RAG chunks (caption + context, file/bbox) | ✅ (`-f chunks`; `--vlm-describe` for neural captions) |
| Quality / routing diagnostics (the self-check loop) | ✅ (`--quality`, `--profile`, `--route-plan`) |

## Step-by-Step Instructions

### 1. Resolve the binary

Prefer `docparse` if it is on PATH. Otherwise build it once from this repo:

```bash
cargo build --release        # → ./target/release/docparse
```

Use `./target/release/docparse` in commands below if it is not on PATH. Model
paths (`models/ppocr-v6`, `models/layout/…`) are resolved relative to the current
working directory, so run from the repo root when using enhancement flags.

### 2. Resolve the input

`docparse` takes one **local file path**; the backend is picked by extension.
It does not fetch URLs — if the user gives a URL, download it to a temp file
first, then pass the local path.

```bash
docparse path/to/file.pdf            # backend auto-selected by extension
```

### 3. Choose the output format

One positional input, one `-f/--format`. Output goes to **stdout** unless you
pass `-o/--out <file>`.

| Format | Flag | Use it for |
|---|---|---|
| **JSON** (default) | `-f json` | Fullest structure: pages → elements (text/table/image) with bbox, font size, `tag`, `span`, `source`. |
| **Markdown** | `-f markdown` | Human/LLM-friendly linearization (heading levels, tables, lists, code fences, image refs). |
| **Text** | `-f text` | Plain text in reading order. |
| **Chunks** | `-f chunks` | **RAG-preferred** retrieval chunks, each with source page + bbox + heading breadcrumb + `section_id`. |
| **Outline** | `-f outline` | Document **structure tree**: nested sections (title/level/page/bbox). For "what's the structure / table of contents" and agentic navigation (section ids match chunks' `section_id`). |
| **OKF** | `-f okf` | **Open Knowledge Format** bundle (writes a *directory*, or `--okf-tar` to stdout): one Markdown+frontmatter concept file per section, git-native and citable. For delivering a parsed doc into a knowledge base. |

```bash
docparse report.pdf -f markdown -o /tmp/report.md
docparse report.pdf -f json     -o /tmp/report.json
docparse report.pdf -f chunks   -o /tmp/report.chunks.json
```

> If the user does not specify a format: for **RAG / ingestion** default to
> `-f chunks`; for **"convert"/"read it"** default to `-f markdown`; for
> **"give me the structure/coordinates"** default to `-f json`; for **"what's the
> structure / outline / table of contents"** or navigating a long doc, `-f outline`;
> for **delivering into a knowledge base** (git-native, citable), `-f okf`. When
> genuinely ambiguous, ask: "Markdown, plain text, structured JSON, RAG chunks,
> structure tree, or an OKF bundle?"

#### chunk schema (`-f chunks`)

```jsonc
{
  "id": 0,
  "kind": "paragraph",          // heading | paragraph | table | code | list_item | image
  "text": "…",
  "page": 1,                     // 1-based
  "bbox": { "x0": 72.0, "y0": 690.1, "x1": 523.4, "y1": 705.8 },
  "heading_path": ["3 Methods", "3.1 Setup"],   // enclosing heading breadcrumb
  "section_id": 12,              // enclosing structure-tree section (matches -f outline ids)
  "char_len": 142
}
```

- **Coordinate system:** PDF user space — origin bottom-left, y up, unit pt.
  Formats without real coordinates (DOCX/HTML/MD/…) use a synthetic layout mapped
  to the same convention.
- **Citation:** `page` + `bbox` point straight back to the source location;
  `heading_path` gives each chunk its section context for retrieval/filtering.
- **Image chunks** (`"kind": "image"`, PDF + DOCX + PPTX): the page-covering figures.
  `text` carries the caption + surrounding context (the searchable field), and an
  `image` object carries `{ file?, data_base64?, media_type?, caption?, caption_source? }`
  for rendering & citation. Caption binds the adjacent "Figure N" line for free
  (`caption_source: "caption-line"`); `--vlm-describe` writes a neural description
  (`"vlm:<model>"`). Pass `--image-dir <dir>` (or `--image-embed`) so the chunk's
  `image.file`/`data_base64` is populated for the consumer to display.
- In `-f json`, elements replaced by a model carry a `source` tag
  (`ocr:ppocr`, `table:unirec-0.1b`, `formula:unirec-0.1b`, `vlm:<model>`,
  `layout:<model>`) so provenance stays visible.

### 4. Self-check the parse (do this whenever fidelity matters)

docparse has the quality loop **built into the CLI** — no separate evaluator
script. These flags print a JSON report to **stderr** (the document still goes to
stdout/`-o`), so capture stderr separately:

```bash
docparse report.pdf -f text --quality   2>/tmp/quality.json   >/dev/null
docparse report.pdf -f text --profile   2>/tmp/profile.json   >/dev/null
docparse report.pdf -f text --route-plan 2>/tmp/route.json    >/dev/null
```

`--quality` report:

```jsonc
{
  "pages": 12, "text_pages": 12, "coverage": 1.0,
  "total_chars": 38201, "garbled_chars": 0, "garbled_ratio": 0.0,
  "hidden_chunks": 0,
  "flags": []        // see table below
}
```

| Flag | Meaning | What to do |
|---|---|---|
| `ScannedNoText` | Page(s) have (almost) no extractable text — a scan/image-only PDF | Re-run with `--ocr` |
| `PartialTextCoverage` | Some pages parsed, others empty (mixed scan + digital) | Re-run with `--ocr` (digital pages still skip the model) |
| `HighGarble` | >10% replacement/control chars — bad font decode (broken CMap/encoding) | Try `--ocr`; if a digital PDF with broken fonts, `--transcribe-model` |
| `HiddenTextPresent` | Invisible OCR text layer detected | Usually informational; report it |
| *(empty)* + `coverage` ≈ 1.0 | Clean born-digital parse | Done — no models needed |

`--profile` gives per-page `kind` (`digital`/`scanned`), `image_coverage`,
`tables`, and `reading_order_anomaly` (high ⇒ layout may be misordered →
consider `--layout`). `--route-plan` reports `hard_pages` — on a clean digital
doc this is empty, which is the whole point: cost stays at zero.

### 5. Refinement loop (max 3 attempts unless told otherwise)

1. Parse once with no enhancers (the fast path) and run `--quality` / `--profile`.
2. If a flag or an obvious defect appears, apply **one** enhancement from the
   decision matrix below, re-parse, re-check.
3. Stop when the report is clean (`flags: []`, coverage high, no anomaly) or the
   visible defect is gone — or after 3 iterations. Then summarize what was wrong,
   which flag fixed it, and any residue.

**Decision matrix — symptom → flag** (full details in [reference.md](reference.md)):

| Symptom | First move |
|---|---|
| Clean digital PDF, just needs text/markdown | No flags — fast path, no models |
| `ScannedNoText` / image-only / `coverage` low | `--ocr` |
| `HighGarble` (broken font decode) | `--ocr`, else `--transcribe-model <dir>` |
| Multi-column / complex layout misordered (`reading_order_anomaly` high) | `--layout` |
| CJK / design-heavy reading order the geometry can't order | `--transcribe-model <dir>` |
| Table present but merged cells / multi-row headers wrong | `--table-model <dir>` (local) or `--vlm-tables` (service, best) |
| Tables missing entirely on a messy layout | `--layout --layout-model models/layout-ppv2/PP-DoclayoutV2_simp.onnx` |
| Display formulas as glyph-soup | `--formula-model <dir>` |
| Need image files / inline base64 | `--image-dir <dir>` / `--image-embed` |

### 6. OCR a scanned PDF

```bash
docparse scan.pdf -f markdown --ocr            # PP-OCRv6 tiny (default), digital pages skip the model
docparse scan.pdf -f markdown --ocr --ocr-models models/ppocr   # fall back to PP-OCRv4
```

On first use, if `models/ppocr-v6` is missing the CLI offers to download it
(~7 MB, Apache-2.0) — but **only in an interactive terminal**. In a non-TTY
context (script, pipe, CI, or driving MCP/REST) it errors instead. To fetch
non-interactively, either run `./scripts/fetch-models.sh ppocr-v6` first or set
`DOCPARSE_OCR_DOWNLOAD=1`:

```bash
DOCPARSE_OCR_DOWNLOAD=1 docparse scan.pdf -f markdown --ocr
```

### 7. Re-extract tables and formulas (local, no service)

These render the relevant region on demand (PDF only) and replace it in place;
on failure the deterministic result is kept. They need a UniRec model directory.

```bash
docparse invoice.pdf -f markdown --table-model models/unirec        # merged cells / multi-row headers
docparse paper.pdf   -f markdown --formula-model models/unirec      # display formulas → LaTeX
docparse design.pdf  -f json     --transcribe-model models/unirec   # whole-page re-recognition (CJK/design)
```

### 8. VLM enhancement (optional, needs an OpenAI-compatible service)

```bash
docparse doc.pdf -f markdown --vlm-tables \
  --vlm-url http://127.0.0.1:8000 --vlm-model <name> [--vlm-api-key <token>]
docparse doc.pdf -f markdown --vlm-describe --vlm-url … --vlm-model …   # caption figures
```

`--vlm-tables` often beats the geometric/local table extractor on the hardest
tables; `--vlm-describe` captions figures — the description is written onto each
figure's image chunk (`image.caption`, `caption_source: "vlm:<model>"`) and
surfaces in markdown alt text, so `-f chunks` figures become richly searchable.

## Other interfaces (same parse, byte-identical output)

The CLI is the right tool for this skill, but mention these when the user is
wiring docparse into a larger system:

| Interface | When | Start |
|---|---|---|
| **Library (Rust crate)** | In-process embedding | depend on `docparse-core` + backend crates |
| **MCP (stdio)** | Agent tool calls (Claude / MCP runtimes) | `docparse mcp` |
| **REST (axum)** | Language-agnostic service | `docparse serve --port 8642` (binds 127.0.0.1; no auth) |

Same input + same format ⇒ identical bytes across all faces — choosing one does
not change results.

## Common edge cases

| Situation | Handling |
|---|---|
| Input is a URL | Download to a temp file first; `docparse` takes a local path only |
| Scanned / image-only PDF | `--ocr` (digital pages still skip the model) |
| Mixed scan + digital pages | `--ocr` — routing keeps born-digital pages model-free |
| Broken fonts / `�` everywhere | `--ocr`; if digital, `--transcribe-model` |
| Multi-column reading order wrong | `--layout` (renders pages, ~2.4 s/page) |
| Merged-cell tables | `--table-model` (local) or `--vlm-tables` (service) |
| Tables missed on messy layout | `--layout-model …PP-DoclayoutV2_simp.onnx` (≈3× YOLO table detection) |
| Very large PDF | Stay on the fast path; enhancers are per-page and opt-in — don't add them blindly |
| Non-PDF format + a PDF-only flag | PDF-only enhancers (`--layout`, `--table-model`, `--formula-model`, `--vlm-*`) are silently skipped for other formats |
| Models missing in non-TTY | Pre-fetch with `./scripts/fetch-models.sh …` or set `DOCPARSE_OCR_DOWNLOAD=1` |

## Output conventions

- Always report **page count** and parse status; if a quality flag fired, say
  which one and which flag/enhancer resolved it.
- **Markdown:** render directly; wrap in a fence only if the user will copy/paste.
- **JSON:** it is already pretty-printed; don't re-serialize.
- **Chunks:** report total chunk count and the kind breakdown
  (headings / paragraphs / tables / …).
- **Never claim a model ran when it didn't** — if you stayed on the fast path,
  say so (that's a feature: zero model cost on digital docs).

## Reference

Full flag list, model directory layout, the two layout backends, quality-flag
semantics, and the symptom→flag decision matrix in detail: [reference.md](reference.md).
A worked end-to-end example: [EXAMPLE.md](EXAMPLE.md).
