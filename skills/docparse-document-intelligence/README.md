# docparse-rs agent skill

An **[Agent Skill](https://agentskills.io/specification)**-style bundle that lets
an AI coding assistant (Claude Code, Cursor, and compatible runtimes) drive
**docparse-rs** to parse, convert, chunk, and analyze documents.

Everything runs through the **`docparse` CLI** — pure Rust, single binary, no
Python, no services, no GPU. The skill leans on docparse's *built-in* quality
diagnostics (`--quality` / `--profile` / `--route-plan`) for a
**parse → self-check → refine** loop, so it needs no extra evaluator script.

## Contents

| Path | Purpose |
|------|---------|
| [`SKILL.md`](SKILL.md) | Full skill instructions (formats, output, OCR, table/formula enhancers, self-check loop) |
| [`reference.md`](reference.md) | Complete flag list, model layout, quality-flag semantics, symptom→flag matrix |
| [`EXAMPLE.md`](EXAMPLE.md) | A worked end-to-end run (parse → quality check → refine → chunk) |

## Install

This bundle lives in the repo at `skills/docparse-document-intelligence/`.
Claude Code discovers skills under `.claude/skills/` (per-project) or
`~/.claude/skills/` (global), both of which are typically git-ignored — so
**symlink or copy** this directory into one of them:

```bash
# Per-project (run from the repo root)
mkdir -p .claude/skills
ln -s "$(pwd)/skills/docparse-document-intelligence" .claude/skills/

# Or globally for all projects
ln -s "$(pwd)/skills/docparse-document-intelligence" ~/.claude/skills/
```

For Cursor, copy it into `~/.cursor/skills/` instead.

## Prerequisites

```bash
cargo build --release        # produces ./target/release/docparse
```

Add `./target/release/docparse` to PATH as `docparse`, or let the skill call the
built binary directly. Optional model files (OCR / layout / table / formula) are
**opt-in** — only fetched when you use an enhancement flag (`./scripts/fetch-models.sh …`,
or `--ocr` auto-downloads PP-OCRv6 on first interactive use).

## Quick start

```bash
docparse report.pdf -f markdown -o /tmp/report.md     # convert to Markdown
docparse report.pdf -f chunks   -o /tmp/chunks.json   # RAG chunks (page + bbox + heading path)
docparse report.pdf -f text --quality 2>/tmp/q.json >/dev/null   # self-check report (stderr)
docparse scan.pdf   -f markdown --ocr                 # OCR a scanned PDF (PP-OCRv6 tiny)
```

See [SKILL.md](SKILL.md) for the decision matrix and the full enhancer set.

## License

Apache-2.0 (aligned with docparse-rs).
