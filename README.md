<div align="center">

# 📄 docparse-rs

**A fast, pure-Rust document parser built for agents & RAG.**

Extract positioned, structured content from 12+ formats — every chunk citable with page + bbox.

[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
![Rust](https://img.shields.io/badge/built%20with-pure%20Rust-orange?logo=rust)
![Single binary](https://img.shields.io/badge/deploy-single%20binary%20~29MB-brightgreen)
![Platforms](https://img.shields.io/badge/platforms-macOS%20%7C%20Linux%20%7C%20Windows-lightgrey)
![Tests](https://img.shields.io/badge/tests-142%20passing-success)

English | [中文](README.zh.md)

</div>

---

docparse-rs turns **PDF · DOCX · HTML · XLSX · PPTX · Markdown · CSV · SRT/VTT · LaTeX · EML · PNG/JPEG · AsciiDoc** into a unified intermediate representation, then emits **JSON / Markdown / Text / RAG chunks**. It takes the *structure-extraction* fast path — interpreting PDF content streams for coordinates instead of rendering pages to pixels — so a warm parse is **<10 ms (~700 pages/s)** and output is **byte-identical** for identical input. One ~29 MB binary, no JVM / C++ / Python, zero runtime dependencies.

## ✨ Features

- 🦀 **One pure-Rust binary** — ~29 MB, zero runtime deps, <10 ms warm parse (~700 pages/s)
- 🔌 **Four faces, one output** — CLI / library / MCP (stdio) / REST, **byte-identical across all**
- 📍 **RAG-native citations** — every chunk carries page + bbox + heading breadcrumbs; `locate(x, y)` reverse lookup, 100% coverage
- 🔍 **In-process OCR** — `--ocr` runs ONNX on `tract` (PP-OCRv6 tiny by default; offers to fetch ~7 MB on first use); digital pages never touch a model; CCITT G3/G4 fax + JBIG2 scans covered
- 🧠 **Embedded models, opt-in** — merged-cell table structure, formula→LaTeX, full-page transcription (UniRec-0.1B), plus PP-DocLayoutV2 / DocLayout-YOLO layout
- 🛡️ **Security pre-checks** — hidden-text filtering (flagged & auditable, never silently dropped), zip-bomb & page-count guards, per-page complexity profiling
- 🧩 **Pluggable AI boundary** — the deterministic core stands alone; models trigger only on hard pages and carry a `source` tag + capped confidence

## 🚀 Quick start

```bash
cargo build --release
D=./target/release/docparse

$D input.pdf -f json       # full IR: provenance + coordinates
$D input.pdf -f markdown   # Markdown
$D input.pdf -f chunks     # RAG chunks (page + bbox + breadcrumbs)
$D scan.pdf  --ocr         # OCR scans (free for digital pages; offers to fetch models/ppocr-v6 on first use)
```

<details>
<summary><b>More commands — layout · tables · formulas · VLM</b></summary>

```bash
$D hard.pdf --layout                                   # layout-model reading order (DocLayout-YOLO; needs models/layout)
$D hard.pdf --layout --layout-model models/layout-ppv2/PP-DoclayoutV2_simp.onnx   # PP-DocLayoutV2 backend (~3x YOLO on messy tables)
$D doc.pdf  --table-model models/unirec                # merged-cell table structure (in-process, no service)
$D doc.pdf  --formula-model models/unirec              # formula → LaTeX
$D doc.pdf  --transcribe-model models/unirec           # full-page transcription (zh/en hard layouts & scans)
$D doc.pdf  --vlm-describe --vlm-url URL --vlm-model M # figure captions via an OpenAI-compatible VLM
$D doc.pdf  --vlm-tables   --vlm-url URL --vlm-model M # VLM table re-extraction (failures keep the deterministic grid)
$D doc.pdf  --image-dir imgs/                          # export embedded images (JSON "file" / Markdown ![]())
$D input.pdf --quality --profile --route-plan          # quality / per-page profile / routing (JSON on stderr)
```
</details>

### Plug into an agent

```bash
claude mcp add docparse -- /path/to/docparse mcp     # MCP tools: parse_document / get_chunks / locate
$D serve --port 8642                                  # REST: POST /parse (multipart) + GET /healthz
curl -F "file=@doc.pdf" "http://127.0.0.1:8642/parse?format=chunks&ocr=true"
```

```python
# Python / LangChain (clients/python — zero-dependency thin client)
from docparse_client.langchain import DocparseLoader
docs = DocparseLoader("paper.pdf").load()   # one Document per chunk, page + bbox metadata
```

## 📊 Quality

Scored on **[OmniDocBench](https://github.com/opendatalab/OmniDocBench)** (CVPR 2025) against **human ground truth**, using the embedded UniRec models:

| Dimension | Path | Score |
|---|---|---|
| Text recognition | `--transcribe-model`, papers | **0.872** |
| Formula → LaTeX | `--formula-model`, papers | **0.874** |
| Table structure | `--table-model`, clean tables | **0.810** (median 0.895) |

**Text and formula are near paper-level (~0.87).** The remaining gap is hard academic tables (multi-row headers + dense numbers + embedded LaTeX). A proxy "Overall" ≈ 75 puts us in the pipeline-tool tier (Marker 78, Docling ~80–85; dedicated VLMs 90+) — see the [full method, caveats & leaderboard →](docs/testresults/2026-06-12-omnidocbench.md).

## 🆚 vs related tools

| | **docparse-rs** | Docling | OpenDataLoader | MarkItDown |
|---|---|---|---|---|
| Deploy | **pure-Rust ~29 MB binary** | Python + models (GB env) | Java / JVM | Python |
| Determinism | **byte-identical default path** | not strictly | deterministic | deterministic |
| Citations | **page+bbox both ways, 100%** | element-level | coordinates | none |
| Formats | 12 | 15+ | PDF-focused | 20+ |
| Speed (born-digital) | **<10 ms / ~700 pg/s** | seconds/page | fast | fast |

Where others win: Docling's neural layout has a higher ceiling on the hardest layouts and a more mature ecosystem; MarkItDown covers more long-tail formats; we ship no GPU pipeline, and non-zh/en OCR (RTL / Korean …) isn't covered yet. [Detailed comparison →](docs/refer/docling-objective-comparison.md)

## 🏗️ Architecture

A Cargo workspace of **17 crates**. The key invariant: **`core` depends on no PDF library** — reading order and output are format-agnostic, so adding a format means implementing the `DocumentParser` trait plus one registry line.

The heart of the project is a self-built **PDF content-stream interpreter** (graphics/text matrix state machine emitting positioned chunks — the layer ODL delegates to veraPDF) and a **font layer** (ToUnicode CMap / AFM / Encoding, independently implemented with veraPDF as the *algorithmic* reference). Neural models never enter the core — they attach per page through an `Enhancer` boundary, and only a hard page routed to a model is ever rendered (on demand, pure-Rust). See the [crates](crates/) and [roadmap →](docs/roadmap.md).

## 📦 Optional models

All Apache-2.0, fetched from their original repos as external files — never baked into the binary. The core needs **none of them**: born-digital PDFs and every other format parse with zero downloads. Pull a tier only when you want the feature:

```bash
# --ocr's default models are also auto-offered on first use — this is just the explicit path:
./scripts/fetch-models.sh ppocr-v6   # --ocr (default)     (~7 MB)
./scripts/fetch-models.sh ocr        # --ocr v4 fallback   (~16 MB)
./scripts/fetch-models.sh layout     # --layout (default)  (~75 MB)
./scripts/fetch-models.sh unirec     # --table/formula/transcribe-model (~700 MB)
./scripts/fetch-models.sh ppv2       # --layout-model ppv2 (~210 MB + a local prep step)
./scripts/fetch-models.sh all
```

Needs the HuggingFace CLI (`pip install -U huggingface_hub`); `ppv2` additionally needs `onnx`+`onnxsim` to static-ize its graph for `tract` (the script prints the one-liner). The `ppocr-v6` default needs no prep — the loader reads PaddleOCR's raw ONNX directly (tract's `ignore_value_info` handles its dynamic graph) and parses the char dict out of the rec yml.

| Tier | Model (source) | Powers |
|---|---|---|
| `ppocr-v6` → `models/ppocr-v6/` (~7 MB) | PP-OCRv6 tiny det+rec (`PaddlePaddle/PP-OCRv6_tiny_*_onnx`) | `--ocr` scanned text (**default**), auto-deskew |
| `ocr` → `models/ppocr/` (~16 MB) | PP-OCRv4 det+rec+cls (`SWHL/RapidOCR`) | `--ocr` v4 fallback |
| `layout` → `models/layout/` (~75 MB) | DocLayout-YOLO (`wybxc/DocLayout-YOLO-DocStructBench-onnx`) | `--layout` regions (default), formula detection |
| `ppv2` → `models/layout-ppv2/` (~210 MB) | PP-DocLayoutV2 (`topdu/PP_DoclayoutV2_onnx`) | richer layout + native reading order ([A/B](docs/testresults/2026-06-15-ppv2-vs-yolo-omnidocbench.md)) |
| `unirec` → `models/unirec/` (~700 MB) | UniRec-0.1B (`topdu/unirec_0_1b_onnx`) | `--table-model` / `--formula-model` / `--transcribe-model` |

> **PP-OCRv6** (PaddleOCR, 2026-06) is the default OCR tier: on a real Chinese scan it's more accurate than the previous PP-OCRv4 mobile (e.g. fixes a 顿号 `、` misread), ~2× faster, and ~half the size — at 1.5 M params. Same DB-detection + CTC-recognition interface as v4/v5, so it drops into the existing pipeline; tract reads the raw export directly. [Evaluation →](docs/refer/ppocr-v6-evaluation.md)
>
> UniRec and PP-DocLayoutV2 are the two halves of [OpenOCR](https://github.com/Topdu/OpenOCR)'s **OpenDoc-0.1B**; we run their official ONNX on pure-Rust `tract` and stitch them with our own deterministic core. [Selection rationale →](docs/refer/openocr-0.1b-evaluation.md)

## 📄 License

**Apache-2.0** — an independent implementation containing no veraPDF code (veraPDF is GPLv3+/MPLv2; its algorithms are referenced with attribution in the sources). All external model files are Apache-2.0. The build carries two minimal, attributed [tract patches](vendor/PATCHES.md) ([vendored on `main` by design](vendor/README.md)) needed to run PP-DocLayoutV2 on `tract`.
