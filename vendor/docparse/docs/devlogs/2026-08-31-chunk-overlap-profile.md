# Geometry-safe chunk overlap development log

> Date: 2026-08-31
> Status: complete
> Plan: [chunk-overlap-profile](../plans/2026-08-31-chunk-overlap-profile.md)

## Change

`ChunkOptions` now carries `overlap_chars` with a compatibility default of zero. `ParaBuf` retains the complete source block parts that formed a paragraph chunk. On a target-triggered flush, it can rebuild a trailing buffer from complete parts whose joined Unicode character count does not exceed the overlap limit.

The rebuilt buffer derives its text, bounding-box union, character count, page, heading path and section from those real parts. Heading, table, image, code, list and page flushes pass an overlap of zero, so evidence never leaks across citation boundaries.

## Validation

- `paragraph_overlap_reuses_complete_blocks_with_exact_source_union` verifies text, exact bbox unions, page, section and char count.
- `paragraph_overlap_does_not_cross_heading_barrier` verifies heading and section isolation.
- Existing default/options compatibility tests remain green.
- Measured command results: core test suite 93 passed (1.51s compile/test command wall output); core clippy completed with zero warnings (1.63s command wall output). The implementation and integration were completed in the continuous FS-204 session ending 2026-08-31 22:59 CST.

No dependency, public chunk schema, parser output, CLI option or REST/MCP surface was added in docparse itself; FastSearch owns the profile/provenance surface.

This is intentionally a **library seam**, analogous to the existing `ChunkOptions.target_chars`: the
docparse CLI/MCP/REST contracts have no new user-visible option and continue to call the default-zero path.
The repository's “four faces” rule applies when a docparse product capability is advertised on those faces;
adding a FastSearch-consumed library knob to all four would expand the agreed FS-204 scope and expose a knob
that those interfaces do not yet model as a profile.

## Validation results

| Gate | Result |
|---|---|
| `cargo fmt --all -- --check` | PASS |
| `cargo test -p docparse-core` | PASS, 93 unit tests |
| `cargo clippy -p docparse-core --all-targets -- -D warnings` | PASS, zero warnings |
| `cargo test` (full vendor workspace) | PASS, 309 tests |
| `cargo clippy --all-targets -- -D warnings` | PASS, full workspace, zero warnings |
| `lorem.pdf` real CLI regression | PASS, first text `Lorem Ipsum`, 2 chunks |
| `1901.03003.pdf` real CLI regression | PASS, first text `MORAN: A Multi-Object Rectified Attention Network`, 120 chunks |
| `issue-336-conto-economico-bialetti.pdf` real CLI regression | PASS, accented text present, 5 chunks |

The first real-sample command used a path relative to the FastSearch root while running inside
`vendor/docparse`, so its pipelines produced no data. It was rejected as invalid evidence and rerun with
`set -o pipefail` plus the correct `../../../opendataloader-pdf/samples/pdf` path; only the successful rerun
is counted above.

## 实际耗时

- 起止：2026-08-31 22:38 ~ 2026-08-31 23:11 CST（自然 33m，专注约 29m）。
- 分布：现状审计 + plan/testcases 约 6m / 实施约 10m / 调试与活入口约 10m / 文档与复审约 7m。
- 偏离最大的环节：root clippy 发现过宽的纯文本 flush helper，促成私有状态机重构（约 2m，未超过 30%）；经验已沉淀到 [private-state-machine-over-wide-helper](../lessonlearned/2026-08-31-private-state-machine-over-wide-helper.md)。
