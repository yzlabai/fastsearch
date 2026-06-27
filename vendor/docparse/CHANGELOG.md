# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- `scripts/fetch-models.sh` — per-tier downloader for the optional neural models
  (`ocr` / `layout` / `unirec` / `ppv2`), pulled from their original Apache-2.0
  repos. Models are never bundled in the repo or binary.
- `LICENSE` (Apache-2.0) and `NOTICE` (third-party attributions: vendored tract
  patches, veraPDF algorithmic reference, optional models, Rust dependencies).
- `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`, and this changelog.

## [0.1.0]

First public release. Pure-Rust, multi-format document parser optimized for
speed and deterministic output.

### Added
- **Formats** — PDF, DOCX, HTML, XLSX, PPTX, Markdown, CSV, SRT/VTT, LaTeX,
  EML, PNG/JPEG, AsciiDoc. Each is a `DocumentParser` over a shared,
  format-agnostic core (reading order + output).
- **PDF engine** — self-built content-stream interpreter (graphics/text matrix
  state machine emitting positioned chunks) and font layer (ToUnicode CMap /
  AFM / Encoding), independently implemented with veraPDF as the *algorithmic*
  reference (no veraPDF code).
- **Layout** — paragraph aggregation, header/footer detection, XY-cut +
  multi-column reading order; bordered/ruled/cluster/borderless table detection.
- **Optional neural enhancers** (opt-in, external models) — `--ocr` (PP-OCRv4),
  `--layout` (DocLayout-YOLO default; PP-DocLayoutV2 second backend),
  `--table-model` / `--formula-model` / `--transcribe-model` (UniRec-0.1B), and
  VLM-based description over an OpenAI-compatible protocol.
- **Outputs** — JSON / Markdown / Text / RAG chunks with per-chunk provenance
  (bbox / page / confidence) and `locate(x,y)` reverse lookup.
- **Interfaces** — CLI, library, MCP stdio server, and REST server, all sharing
  one parse path with byte-identical output.
- **Vendored tract patches** — two minimal, attributed fixes (GatherNd shape
  inference + TopK TDim) that let PP-DocLayoutV2 run on pure-Rust tract; kept
  vendored on `main` by design (see `vendor/README.md`).

[Unreleased]: https://github.com/yzlabai/docparse-rs/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/yzlabai/docparse-rs/releases/tag/v0.1.0
