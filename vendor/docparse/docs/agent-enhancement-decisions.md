# docparse — enhancement decision guide (for agents)

> When to flip an enhancement on, read from the parse's own quality signals.
> The default parse uses **no model** — digital documents never invoke one.
> Turn an enhancement on only when a signal says the deterministic parse fell
> short. This guide is the MCP resource `docparse://guide/enhancement-decisions.md`
> and mirrors the CLI skill's self-check loop.

## The loop (max 3 passes)

1. Parse with everything off: MCP `get_chunks {path}` (or `parse_document`).
   The `get_chunks` envelope carries `quality` and `profile` — read them.
2. If a `quality.flags` entry or an obvious defect shows up, flip **one**
   enhancement from the table below and re-call. Don't stack flags blindly.
3. Stop when `quality.flags` is empty and coverage is high, or the visible
   defect is gone — or after 3 passes. Then say what was wrong and which flag
   fixed it.

## quality.flags → action

| Flag | Meaning | Do |
|---|---|---|
| `scanned_no_text` | Pages have ~no extractable text — a scan / image-only PDF | Re-call with `ocr: true` |
| `partial_text_coverage` | Some pages have text, others are empty (mixed scan + digital) | `ocr: true` — digital pages still skip the model |
| `high_garble` | >10% replacement/control chars — broken font decode (CMap/encoding) | Try `ocr: true`; if it's a digital PDF with broken fonts, a transcription model |
| `hidden_text_present` | Invisible text layer found (excluded from output; prompt-injection vector) | Usually informational — report it |
| *(empty)* + coverage ≈ 1.0 | Clean born-digital parse | Done — no models needed |

## Symptom → enhancement (MCP tool argument)

| Symptom | First move |
|---|---|
| Clean digital PDF, just want text/markdown/chunks | none — the fast path |
| Scan / photo / image-only PDF | `ocr: true` |
| Garbled / wrong reading order on a complex or multi-column / CJK layout | `layout: true` (PDF only; server needs `--layout-model`) |
| Tables with merged cells / multi-row headers come out wrong | `table_model: true` (server needs `--unirec-models`) |
| Display equations lost or mangled | `formula_model: true` (needs `--unirec-models` + layout model) |
| Need figures captioned / tables re-read by a vision model | `vlm_describe: true` / `vlm_tables: true` (server needs `--vlm-url`/`--vlm-model`) |

## Cost & determinism notes

- Each flag has a startup-flag prerequisite on the server (`docparse mcp
  --ocr-models / --layout-model / --unirec-models / --vlm-url …`). If a flag is
  requested but its model wasn't configured, the tool returns a structured error
  naming the missing flag — it does not crash.
- Enhancements are PDF-only and are documented no-ops on other formats.
- With all flags off the output is deterministic and reproducible (same input ⇒
  byte-identical across the CLI, MCP, and REST faces). Model-driven passes
  (OCR/VLM) are the only non-deterministic part, and only when you opt in.

## Navigating long documents

Prefer structure over a bag of chunks: `outline {path, max_depth: 1}` lists the
top-level sections; `outline {path, id}` drills into one; a section's `id`
matches `get_chunks`' `section_id`, so you can fetch just that section's chunks.
For delivery into a knowledge base, `export_okf {path}` returns a git-native,
citable Markdown bundle mirroring the same tree.
