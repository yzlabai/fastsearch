# Worked example: parse → self-check → refine → chunk

A concrete run of the loop in [SKILL.md](SKILL.md). Assumes `docparse` is on PATH
(or substitute `./target/release/docparse`), run from the repo root.

## 1. Parse on the fast path and self-check first

Never reach for a model before you know you need one. Parse once and read the
quality report.

```bash
docparse contract.pdf -f text --quality 2>/tmp/q.json >/dev/null
cat /tmp/q.json
```

```jsonc
{
  "pages": 8, "text_pages": 2, "coverage": 0.25,
  "total_chars": 1190, "garbled_chars": 0, "garbled_ratio": 0.0,
  "hidden_chunks": 0,
  "flags": ["PartialTextCoverage"]
}
```

`coverage` 0.25 with `PartialTextCoverage` ⇒ most pages have no extractable text:
a mixed scan. Confirm with the per-page profile:

```bash
docparse contract.pdf -f text --profile 2>/tmp/p.json >/dev/null
```

Pages 3–8 come back `"kind": "scanned"`, `"image_coverage"` near 1.0 — they're
images.

## 2. Apply one enhancement

The matrix says scanned/low-coverage → `--ocr`. Born-digital pages are routed
around the model automatically, so this is safe for the mixed document.

```bash
# Pre-confirm the one-time ~7 MB model download (or run it in a real terminal)
DOCPARSE_OCR_DOWNLOAD=1 docparse contract.pdf -f text --ocr --quality 2>/tmp/q2.json >/dev/null
cat /tmp/q2.json
```

```jsonc
{ "pages": 8, "text_pages": 8, "coverage": 1.0, "garbled_ratio": 0.0, "flags": [] }
```

Clean: `flags: []`, full coverage. Stop — no second iteration needed.

## 3. Produce the deliverable

For RAG ingestion, emit chunks (each carries source page + bbox + heading path):

```bash
docparse contract.pdf -f chunks --ocr -o /tmp/contract.chunks.json
```

```jsonc
[
  {
    "id": 0, "kind": "heading", "text": "Master Services Agreement",
    "page": 1, "bbox": { "x0": 72.0, "y0": 720.4, "x1": 410.1, "y1": 738.0 },
    "heading_path": [], "char_len": 25
  },
  {
    "id": 1, "kind": "paragraph", "text": "This Agreement is entered into …",
    "page": 1, "bbox": { "x0": 72.0, "y0": 690.1, "x1": 523.4, "y1": 705.8 },
    "heading_path": ["Master Services Agreement"], "char_len": 612
  }
]
```

## 4. Report back

> Parsed **contract.pdf** (8 pages). The first pass flagged `PartialTextCoverage`
> — pages 3–8 were scanned images (coverage 0.25). Re-running with `--ocr`
> (PP-OCRv6 tiny) brought coverage to 1.0 with no garble. Emitted **chunks** to
> `/tmp/contract.chunks.json` (N chunks: H headings, P paragraphs, T tables),
> each with page + bbox + heading breadcrumb for citation.

## Variations

- **Merged-cell tables wrong** after OCR: add `--table-model models/unirec` (or
  `--vlm-tables --vlm-url … --vlm-model …` for the hardest ones).
- **Tables missed entirely on a messy layout**: detection is the bottleneck →
  `--layout --layout-model models/layout-ppv2/PP-DoclayoutV2_simp.onnx`.
- **Multi-column misordered** (`reading_order_anomaly` high in `--profile`):
  `--layout`.
- **Just want a readable copy**: `docparse contract.pdf -f markdown -o /tmp/contract.md`.
