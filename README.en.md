# docparse-rs

[中文](README.md) | **English**

A fast, pure-Rust **multi-format document parsing system**: extracts **positioned, structured content** from PDF/DOCX/HTML (text / layout / reading order / tables → unified IR → JSON / Markdown / Text / RAG chunks) via the "structure extraction, not rasterization" fast path. Built for agents and RAG: results are **deterministic, reproducible, and citable** (every chunk carries page + bbox, with bidirectional lookup).

> The design was motivated by an architecture analysis of [opendataloader-pdf](https://github.com/opendataloader-project/opendataloader-pdf): it is fast because it never renders pages to pixels — it interprets content streams for coordinates and runs layout analysis per page in parallel. docparse-rs re-implements and extends that fast path in pure Rust — no JVM, no C++, no Python, one binary.

## Highlights

- **19.1 MB single binary, zero runtime dependencies**: <10 ms warm parse, 700 pages/s, byte-identical output for identical input;
- **Four faces, one output**: CLI / library / MCP (stdio, direct agent integration) / REST — **byte-identical across interfaces**, OCR path included;
- **RAG as a first-class citizen**: structured chunks with page + bbox + heading breadcrumbs, `locate(x, y)` reverse lookup, 100% citation coverage;
- **Security pre-checks built in**: hidden-text filtering (anti prompt-injection — flagged and auditable, never silently dropped), zip-bomb / page-count resource guards, per-page complexity profiling;
- **Scanned-document OCR without breaking the pure-Rust identity**: `--ocr` runs in-process ONNX inference on `tract` (PP-OCRv4, the de-facto standard models for Chinese; ~16 MB external model files). The page image is the embedded raster's *original bytes* — nothing is rendered. Quality-routed per page: digital pages never touch the model;
- **Pluggable AI boundary**: the deterministic pipeline stands alone; models trigger only on pages the quality score flags as hard, and their output carries a `source` tag and capped confidence.

## Status & scoreboards

All ten roadmap modules are closed (IR / PDF / layout / semantics / multi-format / RAG output / quality routing / AI boundary / security / serving).

**Quality scoreboard** (2026-06-10, born-digital LTR; **agreement** with the reference systems, not human-ground-truth accuracy):

| Reference | NID (reading order) | MHS (headings) | TEDS (tables) |
|---|---|---|---|
| vs OpenDataLoader (deterministic peer, 15 docs) | **0.764** | **0.627** | 0.098 |
| vs Docling (neural pipeline, 13 docs) | **0.833** | **0.645** | 0.187 |

Clean documents score 0.94–1.00 (structurally isomorphic with both references); the aggregate is pulled down by complex CJK layouts and table-structure precision (neural territory). Axis-by-axis comparison, methodology, and honest caveats: [benchmark roundup](docs/testresults/2026-06-10-benchmark-roundup.md).

## Usage

```bash
cargo build --release
./target/release/docparse input.pdf -f json        # full IR (provenance + coordinates)
./target/release/docparse input.pdf -f markdown    # Markdown
./target/release/docparse input.pdf -f text -o out.txt
./target/release/docparse input.pdf -f chunks      # RAG chunks (page + bbox + breadcrumbs)
./target/release/docparse scan.pdf --ocr           # OCR scans (needs models/ppocr; free for digital pages)
./target/release/docparse input.pdf --quality --profile --route-plan   # quality / per-page profile / routing (JSON on stderr)

./target/release/docparse mcp                      # MCP stdio server (direct agent integration)
./target/release/docparse serve --port 8642        # REST: POST /parse (multipart) + GET /healthz
```

```bash
# Claude Code integration:
claude mcp add docparse -- /path/to/docparse mcp
# Tools: parse_document(path, format, ocr) / get_chunks(path, ocr) / locate(path, page, x, y)

# REST:
curl -F "file=@doc.pdf" "http://127.0.0.1:8642/parse?format=chunks&ocr=true"
```

OCR models (optional, three files, ~16 MB, Apache-2.0) go in `models/ppocr/`: `ch_PP-OCRv4_det_infer.onnx` + `ch_PP-OCRv4_rec_infer.onnx` (HuggingFace `SWHL/RapidOCR`) + `ppocr_keys_v1.txt` (PaddleOCR repo).

```bash
cargo test          # 82 unit tests (CMap / matrix / XY-cut / tables / chunking / MCP / limits / OCR decode …)
```

## Architecture

A Cargo workspace with six crates:

| crate | Responsibility | Key deps |
|---|---|---|
| [`docparse-core`](crates/docparse-core) | Format-agnostic core: versioned IR + provenance, the `DocumentParser` trait, XY-cut reading order, layout (paragraphs / running headers-footers), four table detectors, RAG chunking with `locate` reverse lookup, quality scoring / profiling, the `Enhancer` boundary, resource guards, JSON/MD/Text output | serde |
| [`docparse-pdf`](crates/docparse-pdf) | Pure-Rust PDF backend: lopdf parsing + a **self-built content-stream interpreter** (matrix stack + operator state machine + hidden-text detection + image-XObject extraction) + a **font layer** (ToUnicode CMap / AFM / Encoding, independently implemented with veraPDF as the algorithmic reference) + per-page rayon parallelism | lopdf, rayon |
| [`docparse-docx`](crates/docparse-docx) | DOCX backend: docx-rs structure → synthetic coordinates into the same IR; zip-bomb pre-check | docx-rs |
| [`docparse-html`](crates/docparse-html) | HTML backend: DOM pre-order walk → headings / paragraphs / lists / tables | scraper |
| [`docparse-ocr`](crates/docparse-ocr) | ONNX-embedded OCR enhancer: PP-OCRv4 det+rec on `tract`, pure-Rust inference (DBNet post-processing / CTC decoding self-built), implementing `core::enhance::Enhancer` | tract-onnx, zune-jpeg |
| [`docparse-cli`](crates/docparse-cli) | The `docparse` CLI + an **MCP stdio server** (hand-written JSON-RPC, no SDK dependency) + **REST** (axum) | clap, axum, tokio |

**Why this layering**: `core` depends on no PDF library — reading order and output are format-agnostic. Adding a format means implementing the `DocumentParser` trait plus one registry line in the CLI; models never enter the core and attach per page through the `Enhancer` boundary.

### The content-stream interpreter (the heart of the project)

This is the layer opendataloader-pdf delegates to veraPDF, implemented here in Rust: lopdf yields the parsed operator list, and [`interpreter.rs`](crates/docparse-pdf/src/interpreter.rs) maintains the graphics/text matrix stack and emits positioned chunks from text-showing operators. **Nothing is ever rasterized** — even OCR only extracts the raw bytes of the raster image *already embedded* in a scanned page.

Handled operators: `q Q cm` · `BT ET` · `Tf TL Tc Tw Tz Tr Td TD Tm T*` · `Tj ' TJ` · paths `m l re c v y h S f B n` (table-rule extraction) · `Do` (image XObjects).

### The font layer (independently implemented, veraPDF as reference)

Show strings of embedded subset CID fonts are glyph indices — unreadable without font data. Independently implemented with veraPDF as the algorithmic reference: ToUnicode CMaps (`bfchar`/`bfrange`, variable-length codespace splitting), Standard-14 AFM metrics, simple-font Encoding/Differences + AGL, and glyph widths (`Widths` / `W` / `DW`). True glyph widths make x-coordinates exact, which lets the output layer reconstruct word boundaries from geometric gaps.

## Documentation map

- [docs/roadmap.md](docs/roadmap.md) — strategy: vision, the four identity constraints, ten modules, four battlefields vs Docling;
- [docs/plans/next-iteration.md](docs/plans/next-iteration.md) — near-term milestones N1–N6 (all complete) with acceptance records;
- [docs/testresults/](docs/testresults/) — scoreboards and evaluations ([benchmark roundup](docs/testresults/2026-06-10-benchmark-roundup.md) is the entry point);
- [docs/devlogs/](docs/devlogs/) — per-milestone process, decisions, and lessons. (Most documents are in Chinese.)

## Progress

- [x] **M1–M7**: text fidelity (AFM / Encoding / CMap / spacing operators), the IR spine (versioning + provenance + quality), readable layout, bordered tables, DOCX/HTML, RAG chunking + citations, quality routing + the enhancer boundary.
- [x] **N1 evaluation**: NID/TEDS/MHS against ODL and Docling (table above); automated differentiation metrics (`scripts/metrics.sh`).
- [x] **N2 serving**: MCP stdio + REST; all four interfaces byte-identical.
- [x] **N3 real enhancer**: ONNX-embedded OCR (PP-OCRv4 × `tract`, pure Rust) — `chinese_scan` goes from 0 text to **14/14 lines correct** with bbox citations; MCP/REST pass-through; digital pages stay model-free.
- [x] **N4 (bulk)**: four table detectors (bordered → ruled → cluster → borderless), heading levels, word spacing.
- [x] **N5 security & profiling**: hidden-text filtering (Tr 3/7 / off-page / tiny fonts → flagged + excluded + auditable), zip-bomb / page-count guards, per-page complexity profile (`--profile`).
- [ ] **Phase 4 (planned)**: closing the axes where Docling clearly leads — layout/table-structure ONNX enhancers (hard-page routing), XLSX/PPTX backends, region-level OCR / form streams, semantic enrichment (code blocks / formulas / pictures / charts / full-page VLM via a deterministic→HTTP→embedded ladder), LangChain/LlamaIndex integrations, corpus-scale stress testing. See the [iteration plan](docs/plans/closing-docling-gaps.md) (Chinese).

## License

Apache-2.0. This is an independent implementation containing no veraPDF code (veraPDF is GPLv3+/MPLv2; its algorithms are referenced with attribution in the sources). The OCR models (PP-OCR) are Apache-2.0 and distributed as external files.
