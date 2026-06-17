# docparse-rs CLI reference (for the skill)

Companion to [SKILL.md](SKILL.md): the full flag surface, model layout, and the
decision matrix in detail. Authoritative source is always the code
(`crates/docparse-cli/src/main.rs`) — when this drifts, the code wins.

## Invocation shape

```
docparse <INPUT> [-f json|markdown|text|chunks] [-o OUT] [enhancement flags] [diagnostic flags]
docparse mcp   [--ocr-models DIR] [--layout-model PATH] [--unirec-models DIR] [--vlm-url URL --vlm-model NAME]
docparse serve [--host 127.0.0.1] [--port 8642] [same model flags as mcp]
```

- Exactly one positional `INPUT`; backend chosen by extension.
- Default format is `json`. Output goes to stdout unless `-o/--out`.
- Diagnostic reports (`--quality`, `--profile`, `--route-plan`) print JSON to
  **stderr**; the document still goes to stdout/`-o`.

## Supported input formats (12 backends)

PDF · DOCX · HTML · XLSX · PPTX · Markdown · CSV · SRT · TeX · EML · image
(PNG/JPG/…) · AsciiDoc. `core` is format-agnostic — reading order and the four
output formats are shared by every backend.

## Output formats

| `-f` | Description |
|---|---|
| `json` (default) | Full IR: `pages[] → elements[]` (text / table / image) with `bbox`, font size, `tag`, table-cell `span`, and `source` provenance. |
| `markdown` | Linearized: heading levels, pipe tables, lists, code fences, image references. |
| `text` | Plain text in reading order. |
| `chunks` | RAG retrieval chunks — see schema in SKILL.md §3. `--table-format markdown` renders tables inside chunk text as pipe tables (default `tab`). |

## Enhancement flags (all opt-in; default off; digital docs touch no model)

| Flag | Effect | Notes |
|---|---|---|
| `--ocr` | OCR quality-flagged (scanned) pages with embedded PP-OCRv6 tiny (tract). | Digital pages never touch the model. Auto-downloads ~7 MB on first use (TTY confirm / `DOCPARSE_OCR_DOWNLOAD=1`; non-TTY errors). |
| `--ocr-models DIR` | OCR model dir. | Default `models/ppocr-v6`. Pass `models/ppocr` for PP-OCRv4. Any generation: `*det*.onnx` / `*rec*.onnx` + dict. |
| `--layout` | Re-derive macro reading order with the layout model (renders each page on demand, pure Rust). | **PDF only.** Heavier: ~2.4 s/page. |
| `--layout-model PATH` | Layout ONNX. Backend auto-detected by ONNX input count. | Default `models/layout/doclayout_yolo.onnx` (DocLayout-YOLO). Pass `models/layout-ppv2/PP-DoclayoutV2_simp.onnx` for PP-DocLayoutV2 (25-class + native reading order; ≈3× YOLO on messy-layout table **detection**). |
| `--table-model DIR` | Re-extract detected tables' structure with embedded UniRec-0.1B (renders each table region). | **PDF only.** Resolves merged cells / multi-row headers in-process, no service. `source: table:unirec-0.1b`; failures keep the geometric grid. |
| `--formula-model DIR` | Display formulas → LaTeX with UniRec-0.1B. | **PDF only.** Formula regions from the layout model. `source: formula:unirec-0.1b`. |
| `--transcribe-model DIR` | Re-recognize whole pages with UniRec (layout regions read in order, replacing page text). | **PDF only.** The route for design/CJK layouts geometry can't order; trades away line-level positions (chunks carry region bboxes). |
| `--vlm-describe` | Caption sizable figures with a VLM (renders figure regions). | **PDF only.** Needs `--vlm-url` + `--vlm-model`. Captions injected as positioned text, `source: vlm:<model>`. |
| `--vlm-tables` | Re-extract table structure with a VLM. | **PDF only.** Needs `--vlm-url` + `--vlm-model`. Often best on the hardest tables; failures keep the deterministic grid. |
| `--vlm-url URL` | OpenAI-compatible base URL (vLLM / LM Studio / cloud), e.g. `http://127.0.0.1:8000`. | Required by `--vlm-*`. |
| `--vlm-model NAME` | Vision model name as the service knows it. | Required by `--vlm-*`. |
| `--vlm-api-key TOKEN` | Bearer token, if the service requires one. | Optional. |
| `--image-embed` | Embed image payloads as base64 in JSON (`data_base64` + `data_media_type`). | Decodes embedded images ≥16 px a side. |
| `--image-dir DIR` | Export embedded raster images (≥16 px) to `DIR` as JPEG/PNG; JSON gains a `file` path, Markdown references them. | **PDF only.** |

> PDF-only enhancers are silently skipped for non-PDF inputs.

## Diagnostic flags (JSON → stderr)

| Flag | Reports |
|---|---|
| `--quality` | `{pages, text_pages, coverage, total_chars, garbled_chars, garbled_ratio, hidden_chunks, flags[]}` |
| `--profile` | Per-page `{page, kind, text_chars, image_count, image_coverage, tables, enhanced_chunks, reading_order_anomaly}` |
| `--route-plan` | `{hard_pages, total_pages, routes}` — which pages a model *would* be escalated to (empty on clean digital docs) |

### Quality flags

| Flag | Trigger | Fix |
|---|---|---|
| `ScannedNoText` | A page has (almost) no extractable text | `--ocr` |
| `PartialTextCoverage` | Coverage below threshold across pages (mixed scan/digital) | `--ocr` |
| `HighGarble` | `garbled_ratio` > 0.1 (replacement/control chars — bad decode) | `--ocr`, else `--transcribe-model` |
| `HiddenTextPresent` | An invisible text layer is present | Usually informational |

`reading_order_anomaly` (from `--profile`) high on a page ⇒ the deterministic
geometry may have misordered it → try `--layout`.

## Symptom → flag, in detail

- **Clean born-digital PDF** (`flags: []`, `coverage` ≈ 1.0): stay on the fast
  path. No model. This is the common case and the design goal.
- **Scanned / image-only** (`ScannedNoText`, low `coverage`, high
  `image_coverage`): `--ocr`. Mixed docs are fine — born-digital pages are routed
  around the model.
- **Garbled text** (`HighGarble`): broken font/CMap decode. `--ocr` rasterizes and
  re-reads; for a digital PDF whose fonts are simply undecodable, `--transcribe-model`.
- **Misordered multi-column / complex layout** (`reading_order_anomaly` high):
  `--layout` reranks via the layout model. For CJK / design-heavy pages the
  geometry can't order, `--transcribe-model`.
- **Wrong tables** (merged cells, multi-row headers): `--table-model` (local,
  deterministic-ish, no service) first; `--vlm-tables` (service) when local isn't
  enough — it leads on the hardest tables.
- **Tables missed entirely on a messy layout**: the bottleneck is *detection*, not
  structure — switch the layout backend to PP-DocLayoutV2
  (`--layout-model …PP-DoclayoutV2_simp.onnx`), which detects ≈3× the tables of
  YOLO on messy documents.
- **Formulas as glyph-soup**: `--formula-model` → one LaTeX chunk per region,
  tagged `Formula`.

## Model directory layout (typical)

```
models/
  ppocr-v6/        # PP-OCRv6 tiny: *det*.onnx, *rec*.onnx, rec inference.yml (dict)  ← --ocr default
  ppocr/           # PP-OCRv4 fallback                                                ← --ocr-models models/ppocr
  layout/doclayout_yolo.onnx                    # DocLayout-YOLO (default --layout backend)
  layout-ppv2/PP-DoclayoutV2_simp.onnx          # PP-DocLayoutV2 (richer; generated by scripts/spike/ppv2/prepare.py)
  unirec/          # UniRec-0.1B encoder/decoder ONNX + tokenizer  ← --table-model / --formula-model / --transcribe-model
```

Fetch helper: `./scripts/fetch-models.sh ppocr-v6` (and friends). Model files are
gitignored — they download on demand.

## Provenance (`source` tags in `-f json`)

`ocr:ppocr` · `table:unirec-0.1b` · `formula:unirec-0.1b` · `vlm:<model>` ·
`layout:<model>`. Elements without a `source` are deterministic structure
extraction. Provenance is always visible so a downstream system can trust or
re-derive any model-touched element.

## Invariants worth quoting to users

- **Coordinate system:** PDF user space — origin bottom-left, y up, unit pt.
- **Determinism:** same input + same format ⇒ byte-identical output across CLI /
  MCP / REST.
- **Fast path is model-free:** a digital document is parsed without rendering
  pixels or loading any model; `--route-plan` proves how few pages (often zero)
  are hard.
