# docparse-rs

**English** | [中文](README.zh.md)

A fast, pure-Rust **multi-format document parsing system**: extracts **positioned, structured content** from PDF/DOCX/HTML/XLSX/PPTX/Markdown/CSV/SRT·VTT/LaTeX/EML/PNG·JPEG/AsciiDoc (text / layout / reading order / tables → unified IR → JSON / Markdown / Text / RAG chunks) via the "structure extraction, not rasterization" fast path. Built for agents and RAG: results are **deterministic, reproducible, and citable** (every chunk carries page + bbox, with bidirectional lookup).

> The design was motivated by an architecture analysis of [opendataloader-pdf](https://github.com/opendataloader-project/opendataloader-pdf): it is fast because it never renders pages to pixels — it interprets content streams for coordinates and runs layout analysis per page in parallel. docparse-rs re-implements and extends that fast path in pure Rust — no JVM, no C++, no Python, one binary.

## Highlights

- **26.5 MB single binary, zero runtime dependencies** (incl. two pure-Rust inference stacks + on-demand renderer): <10 ms warm parse, 700 pages/s, byte-identical output for identical input;
- **Four faces, one output**: CLI / library / MCP (stdio, direct agent integration) / REST — **byte-identical across interfaces**, OCR path included;
- **RAG as a first-class citizen**: structured chunks with page + bbox + heading breadcrumbs, `locate(x, y)` reverse lookup, 100% citation coverage;
- **Security pre-checks built in**: hidden-text filtering (anti prompt-injection — flagged and auditable, never silently dropped), zip-bomb / page-count resource guards, per-page complexity profiling;
- **Scanned-document OCR without breaking the pure-Rust identity**: `--ocr` runs in-process ONNX inference on `tract` (PP-OCRv4, the de-facto standard models for Chinese; ~16 MB external model files). The page image is the embedded raster's *original bytes* — nothing is rendered. Scan encodings covered: JPEG/Flate/**CCITT G3·G4 fax/JBIG2** (JPX is position-only for now). Quality-routed per page: digital pages never touch the model;
- **Embedded semantic models (opt-in, no service)**: table structure (merged cells / multi-row headers → rowspan/colspan in the IR), formula→LaTeX, and full-page transcription — UniRec-0.1B on in-process `tract` inference (~700MB external model files);
- **Pluggable AI boundary**: the deterministic pipeline stands alone; models trigger only on pages the quality score flags as hard, and their output carries a `source` tag and capped confidence (in-process tract or an OpenAI-compatible service).

## Status & scoreboards

All ten roadmap modules are closed (IR / PDF / layout / semantics / multi-format / RAG output / quality routing / AI boundary / security / serving).

### Quality on OmniDocBench (human ground truth)

[OmniDocBench](https://github.com/opendatalab/OmniDocBench) (CVPR 2025, opendatalab) scores against **human-annotated** truth — tables as HTML/LaTeX with real merged-cell spans, formulas as LaTeX, text and reading order. It is the benchmark **OpenDoc-0.1B / UniRec — the model we embed — reports 90.57% on**. We score it with our own metrics on data subsets (exact-tree-edit TEDS_X for tables, LaTeX char-similarity for formulas, char-level similarity for text — Chinese has no word spaces):

| Dimension | Path | Score |
|---|---|---|
| **Text recognition** | `--transcribe-model` (UniRec), academic papers | **0.872** |
| **Formula → LaTeX** | UniRec, academic papers (full set 0.708) | **0.874** |
| **Table structure** | UniRec single-module, 80 tables | **0.810** (median **0.895**) |
| Table, end-to-end | detect + recognize, single-table pages | 0.827 |
| Table, end-to-end | academic papers (hardest class) | 0.517 |
| Text, light tier | `--ocr` PP-OCRv4 mobile (16 MB) | 0.42–0.44 |

**Read plainly:**
- **Strong** — with the embedded UniRec models, **text and formula both reach ~0.87 (near paper-level)**; clean table regions score **0.810, with half the tables near-perfect (median 0.895)**.
- **Weak** — **academic tables are the biggest gap** (0.52 end-to-end: multi-row headers + dense numbers + embedded LaTeX). The light `--ocr` tier (mobile, 16 MB) is only ~0.44 — built for "zero cost on digital pages, scan fallback", not heavy image OCR; use `--transcribe-model` when quality matters.

**Where we land overall.** Plugging the paper-subset scores into OmniDocBench's official Overall formula `((1−text_edit)·100 + table_TEDS + formula_CDM)/3` gives **≈75** — a rough proxy (our own metrics, subsets, single-module table/formula), **not directly comparable** to the official numbers below:

| System | Type | Overall |
|---|---|---|
| MinerU2.5-Pro / GLM-OCR | dedicated VLM | ~95 |
| **OpenDoc-0.1B** (the UniRec we embed) | dedicated VLM | **90.67** |
| GPT-4o / GOT-OCR | general / expert VLM | ~86 |
| Docling | pipeline tool | ~80–85 |
| Marker | pipeline tool | 78.44 |
| **docparse-rs** (paper subset, proxy) | pipeline tool | **~75** |

Honest positioning: **text and formula are no longer the bottleneck (~0.87 each); we trail the leaderboard top (90+) mainly on hard academic tables plus end-to-end pipeline polish.** We embed UniRec but are not the full OpenDoc system (it runs PP-DocLayoutV2 + UniRec end-to-end; we use DocLayout-YOLO + per-task re-extraction + our own stitching), and we are **born-digital first** — image documents are a complementary domain. Full method and caveats: [OmniDocBench eval](docs/testresults/2026-06-12-omnidocbench.md).

### Agreement scoreboard (vs ODL / Docling, born-digital)

A second, complementary reference: **agreement** with peer systems' output on born-digital docs — not human truth. It tracks the deterministic fast path, but compares against *flattened* table truth, which penalizes real merged-cell structure (the same UniRec table reads 0.526 here vs 0.810 on OmniDocBench's span truth):

| Reference | NID (reading order) | MHS (headings) | TEDS_X |
|---|---|---|---|
| vs OpenDataLoader (15 docs) | 0.792 | 0.687 | 0.477 |
| vs Docling (13 docs) | 0.822 | 0.645 | 0.526 |

Details: [benchmark roundup](docs/testresults/2026-06-10-benchmark-roundup.md).

## Comparison with related tools

> Honest framing: these tools have different missions; the table aligns them on the "agent/RAG consuming documents" axis, and credits where others win. Detailed analysis: [docs/refer/docling-objective-comparison.md](docs/refer/docling-objective-comparison.md) (Chinese).

| Dimension | **docparse-rs** | Docling | OpenDataLoader-PDF | MarkItDown |
|---|---|---|---|---|
| Implementation / deploy | **pure-Rust single ~26.5MB binary, zero runtime deps** | Python + neural models (GB-scale env, cold start) | Java/JVM (veraPDF lineage) | Python, lightweight |
| Determinism | **default path byte-identical for identical input** | neural pipeline not strictly deterministic | deterministic | deterministic |
| Citations | **page+bbox both ways (chunk↔coordinate `locate`), 100% coverage** | element-level provenance | element coordinates | none (markdown-first) |
| Formats | 12 | **15+** | PDF-focused | **20+** |
| Table structure (merged cells) | four deterministic detectors + **embedded 0.1B model** (rowspan/colspan in the IR, opt-in) | TableFormer (neural, built-in) | deterministic (flat grid) | basic |
| Formula → LaTeX | `--formula-model` (embedded) | yes (model) | — | — |
| OCR | in-process `tract` (PP-OCR), **zero model cost on digital pages**; full-page transcription tier | multi-engine, model on every page | hybrid mode (external) | plugin |
| VLM/LLM enrichment | OpenAI-compatible services (vLLM 等), per-task opt-in | built-in + serve ecosystem | hybrid (docling backend) | optional LLM captions |
| Agent surfaces | **CLI/lib/MCP/REST byte-identical** + Python client + LangChain/LlamaIndex loaders | SDK + mature ecosystem | CLI/Java/Python | CLI/lib |
| Born-digital speed | **<10ms warm parse, ~700 pages/s** | seconds/page | fast | fast |
| License | Apache-2.0 (models included) | MIT (some model licenses separate) | Apache-2.0 | MIT |

**Where others still win — stated plainly**: Docling's neural layout has a higher quality ceiling on the hardest layouts, broader formats, and a more mature ecosystem; MarkItDown covers more long-tail formats; we deliberately ship no GPU pipeline, and RTL / Korean (and other non-zh/en OCR domains) are not covered yet (scored as 0 in our eval, honestly). The scoreboard above measures agreement with reference systems, not human ground truth — see the [benchmark roundup](docs/testresults/2026-06-10-benchmark-roundup.md).

## Usage

```bash
cargo build --release
./target/release/docparse input.pdf -f json        # full IR (provenance + coordinates)
./target/release/docparse input.pdf -f markdown    # Markdown
./target/release/docparse input.pdf -f text -o out.txt
./target/release/docparse input.pdf -f chunks      # RAG chunks (page + bbox + breadcrumbs)
./target/release/docparse scan.pdf --ocr           # OCR scans (needs models/ppocr; free for digital pages)
./target/release/docparse hard.pdf --layout        # layout-model macro reading order (needs models/layout, opt-in)
./target/release/docparse doc.pdf --vlm-describe --vlm-url http://127.0.0.1:8000 --vlm-model <vision-model>   # VLM figure captions
./target/release/docparse doc.pdf --vlm-tables --vlm-url http://127.0.0.1:8000 --vlm-model <vision-model>     # VLM table re-extraction (merged cells / multi-row headers); failures keep the deterministic grid
./target/release/docparse doc.pdf --table-model models/unirec   # embedded UniRec-0.1B table re-extraction (merged cells), in-process, no service
./target/release/docparse doc.pdf --formula-model models/unirec # formula -> LaTeX (YOLO finds formula regions + UniRec reads them; needs models/layout)
./target/release/docparse doc.pdf --transcribe-model models/unirec # full-page transcription (high-quality tier for zh/en hard layouts & scans; region-level positions)
./target/release/docparse doc.pdf --image-dir imgs/   # export embedded images (JPEG/PNG); JSON gains "file", Markdown gains ![]() refs
./target/release/docparse doc.pdf --image-embed       # embed images as base64 in JSON (data_base64 + data_media_type)
./target/release/docparse input.pdf --quality --profile --route-plan   # quality / per-page profile / routing (JSON on stderr)

./target/release/docparse mcp                      # MCP stdio server (direct agent integration)
./target/release/docparse serve --port 8642        # REST: POST /parse (multipart) + GET /healthz
```

```bash
# Claude Code integration:
claude mcp add docparse -- /path/to/docparse mcp
# Tools: parse_document / get_chunks / locate — args ocr/layout/table_model/formula_model/vlm_*
# (configure models at server start: docparse mcp --unirec-models models/unirec --vlm-url ...)

# REST:
curl -F "file=@doc.pdf" "http://127.0.0.1:8642/parse?format=chunks&ocr=true&table_model=true"

# Python / LangChain (clients/python, zero-dependency thin client):
#   from docparse_client.langchain import DocparseLoader
#   docs = DocparseLoader("paper.pdf").load()   # one Document per chunk, page+bbox metadata
```

Optional model files (all Apache-2.0, shipped as external files, never baked into the binary):

| Directory | Model | Origin | Powers |
|---|---|---|---|
| `models/ppocr/` (~16 MB) | PP-OCRv4 det+rec + dict; optional cls orientation classifier (~0.6 MB; absent → rotation correction disabled) | PaddleOCR (HuggingFace `SWHL/RapidOCR` conversions; cls at `PP-OCRv1/ch_ppocr_mobile_v2.0_cls_infer.onnx`) | `--ocr` scanned text + auto-deskew of rotated scans (0/90/180/270) |
| `models/layout/` (~75 MB) | DocLayout-YOLO | [opendatalab/DocLayout-YOLO](https://github.com/opendatalab/DocLayout-YOLO) (DocStructBench) | `--layout` regions, formula-region detection |
| `models/unirec/` (~700 MB) | **UniRec-0.1B** (unified text/formula/table recognition) | [OpenOCR](https://github.com/Topdu/OpenOCR) (FVL Lab; [paper arXiv 2512.21095](https://arxiv.org/abs/2512.21095)) — the recognizer of their **OpenDoc-0.1B** document-parsing system; official ONNX: `huggingface-cli download topdu/unirec_0_1b_onnx --local-dir models/unirec` | `--table-model` merged-cell tables / `--formula-model` formula→LaTeX / `--transcribe-model` full-page transcription (zh/en) |

> How UniRec is integrated: we run its official encoder/decoder ONNX on pure-Rust `tract`, driving the autoregressive loop and KV cache on the Rust host — OpenOCR's own OpenDoc pipeline is Python/ONNX Runtime; we reuse the models and tokenizer mapping and independently implement inference plus HTML/LaTeX result parsing (selection rationale and spike measurements: [docs/refer/openocr-0.1b-evaluation.md](docs/refer/openocr-0.1b-evaluation.md), Chinese).

```bash
cargo test          # 142 unit tests (CMap / matrix / XY-cut / tables / chunking / MCP / limits / OCR decode / 1-bit & rotation / format backends …)
```

## Architecture

A Cargo workspace with seventeen crates:

| crate | Responsibility | Key deps |
|---|---|---|
| [`docparse-core`](crates/docparse-core) | Format-agnostic core: versioned IR + provenance, the `DocumentParser` trait, XY-cut reading order, layout (paragraphs / running headers-footers), four table detectors, RAG chunking with `locate` reverse lookup, quality scoring / profiling, the `Enhancer` boundary, resource guards, JSON/MD/Text output | serde |
| [`docparse-pdf`](crates/docparse-pdf) | Pure-Rust PDF backend: lopdf parsing + a **self-built content-stream interpreter** (matrix stack + operator state machine + hidden-text detection + image-XObject extraction) + a **font layer** (ToUnicode CMap / AFM / Encoding, independently implemented with veraPDF as the algorithmic reference) + per-page rayon parallelism | lopdf, rayon |
| [`docparse-docx`](crates/docparse-docx) | DOCX backend: docx-rs structure → synthetic coordinates into the same IR; zip-bomb pre-check | docx-rs |
| [`docparse-html`](crates/docparse-html) | HTML backend: DOM pre-order walk → headings / paragraphs / lists / tables | scraper |
| `docparse-{xlsx,pptx,md,csv,srt,tex}` | Thin backends: XLSX (calamine) / PPTX (one page per slide) / Markdown / CSV (hand-rolled RFC-4180 subset) / SRT·WebVTT subtitles (one timestamped paragraph per cue) / LaTeX source subset (sections/lists/tabular→Table) / EML email (headers/body/attachment listing) / PNG·JPEG images-as-documents (riding the OCR route) / AsciiDoc subset — all flow into the same IR via the synthetic layout | calamine, quick-xml, pulldown-cmark, mail-parser, zune-png |
| [`docparse-ocr`](crates/docparse-ocr) | ONNX-embedded enhancers: OCR (PP-OCRv4 det+rec, self-built DBNet post-processing / CTC decoding) and layout (DocLayout-YOLO regions → reading groups), both on `tract` pure-Rust inference | tract-onnx, zune-jpeg |
| [`docparse-raster`](crates/docparse-raster) | On-demand hard-page rendering (pure-Rust `hayro`, ~100ms/page) — the main pipeline never renders; enhancer-routed pages only, opt-in, with a broken-render guard | hayro |
| [`docparse-vlm`](crates/docparse-vlm) | VLM enhancer: picture description & friends over OpenAI-compatible services (vLLM/LM Studio 等), minimal built-in PNG encoder, graceful degradation | ureq, base64 |
| [`docparse-cli`](crates/docparse-cli) | The `docparse` CLI + an **MCP stdio server** (hand-written JSON-RPC, no SDK dependency) + **REST** (axum) | clap, axum, tokio |

**Why this layering**: `core` depends on no PDF library — reading order and output are format-agnostic. Adding a format means implementing the `DocumentParser` trait plus one registry line in the CLI; models never enter the core and attach per page through the `Enhancer` boundary.

### The content-stream interpreter (the heart of the project)

This is the layer opendataloader-pdf delegates to veraPDF, implemented here in Rust: lopdf yields the parsed operator list, and [`interpreter.rs`](crates/docparse-pdf/src/interpreter.rs) maintains the graphics/text matrix stack and emits positioned chunks from text-showing operators. **The main pipeline never rasterizes** (that is where the speed comes from) — OCR only extracts the raw bytes of the raster *already embedded* in a scanned page; only when a hard page is routed to a neural enhancer is that single page rendered on demand by a pure-Rust renderer (opt-in, off by default).

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
- [x] **Phase 4 (main body, 2026-06-11)**: format parity 3→12 (XLSX/PPTX/MD/CSV/SRT·VTT/LaTeX/EML/PNG·JPEG images-as-documents/AsciiDoc), the full G9 structure layer (TEDS gate passed), **embedded table/formula models** (`--table-model`/`--formula-model`, UniRec-0.1B on tract — in-process merged-cell semantics and formula→LaTeX), VLM service tasks (`--vlm-describe/--vlm-tables`, OpenAI-compatible, fully optional), image export/embed (`--image-dir`/`--image-embed`), full MCP/REST enhancement passthrough, Python client + LangChain/LlamaIndex loaders (five-line acceptance verified), stress + fuzzing (1847 inputs + ~10.2M executions, zero panics), IR 0.7.0 (cell span semantics). See the [iteration plan](docs/plans/closing-docling-gaps.md) (Chinese).
- [x] **Phase 5 (robustness depth, 2026-06-12)**: CCITT G3/G4 + JBIG2 scan decoding (fax-class scans now OCR), rotation correction (`/Rotate` baked into the base CTM + PP-OCR cls, 0/90/180/270 auto-deskew), two-column left-column reflow (column-aware `fill_x`, single-column byte-identical), exact tree-edit-distance TEDS, HTML `<meta charset>` honoring + CMYK-JPEG handling. **Two concluded negatives, both done to the point of proof**: deterministic `--layout` auto-routing closed (a 4th geometric criterion still not separable), and same-color hidden-text detection found to be an *architectural limit* — a full background model still can't reach zero false positives without rendering, which conflicts with the no-rasterize identity. See [hardening-iteration.md](docs/plans/hardening-iteration.md) (Chinese).

## License

Apache-2.0. This is an independent implementation containing no veraPDF code (veraPDF is GPLv3+/MPLv2; its algorithms are referenced with attribution in the sources). All external model files are Apache-2.0: PP-OCR (PaddleOCR), DocLayout-YOLO (opendatalab), and UniRec-0.1B ([OpenOCR](https://github.com/Topdu/OpenOCR) / FVL Lab — with thanks for open-sourcing the OpenDoc-0.1B document-parsing system and the official ONNX export).
