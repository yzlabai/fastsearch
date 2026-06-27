# Contributing to docparse-rs

Thanks for your interest! docparse-rs is a pure-Rust, multi-format document
parser that prioritizes **speed and determinism**. This guide covers how to
build, test, and submit changes.

## Getting started

```bash
git clone https://github.com/yzlabai/docparse-rs
cd docparse-rs
cargo build
cargo test
```

The core binary needs **no models** — born-digital PDFs and every other format
parse with zero downloads. Optional neural features pull models on demand:

```bash
./scripts/fetch-models.sh ocr     # see README "Optional models" for all tiers
```

## Before you open a PR

All of these must pass — CI parity is enforced by convention:

```bash
cargo fmt              # default style, no config
cargo clippy --all-targets   # zero warnings is the bar
cargo test             # all unit tests green
```

### Font / decoding / output changes: run the regression triplet

Changes to fonts, text decoding, or output reconstruction are the easiest to
break silently (fix CID subsets, regress simple fonts). Always run the
cross-sample regression against real PDFs before submitting:

```bash
S=../opendataloader-pdf/samples/pdf      # not in this repo; point at your samples
for f in lorem 1901.03003 issue-336-conto-economico-bialetti; do
  ./target/debug/docparse $S/$f.pdf -f text 2>/dev/null | head -3
done
```

`lorem` (CID subset) + `bialetti` (simple font + accents) + `1901.03003`
(mixed) is the minimal triplet.

## Architecture & where changes go

The repo is a Cargo workspace of 17 crates. The key invariant: **`core` depends
on no PDF library** — reading order and output are format-agnostic. Adding a
format means implementing the `DocumentParser` trait plus one registry line in
`crates/docparse-cli/src/main.rs`.

See [CLAUDE.md](CLAUDE.md) §2 for a "what do I want to do → which file" map, and
[docs/roadmap.md](docs/roadmap.md) / [docs/status.md](docs/status.md) for
strategy and current state. Read `docs/status.md` before starting work.

## Invariants to uphold (across all format backends)

- **Coordinates**: PDF user space — origin bottom-left, y-up, units in pt.
- **Glyph widths / advance**: 1/1000 em (PDF glyph space).
- **Layering**: `core` never `use`s a PDF library; PDF-specific logic stays in
  `docparse-pdf`.
- **Robustness**: a page that fails to parse returns an empty `Page` — it never
  panics. `unwrap`/`expect` only where an invariant guarantees safety.
- **Flag your approximations**: any estimate or fallback (0.5em advance, US
  Letter default, ...) gets a `TODO` + its impact — never silent.

## Pull requests

- Keep PRs focused; one logical change per PR.
- Add unit tests for pure algorithms (CMap, matrix, XY-cut) in the same crate.
- Match the surrounding code's style, naming, and comment density.
- Describe what changed and why; link any relevant issue.

## License

By contributing, you agree your contributions are licensed under the project's
[Apache-2.0](LICENSE) license.
