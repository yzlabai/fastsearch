# Vendored tract patches (docparse-rs)

These crates are local copies of crates.io tract, injected via root
`Cargo.toml` `[patch.crates-io]`, carrying minimal fixes needed to run
PP-DocLayoutV2 (RT-DETR) on tract.

**Decision (2026-06-15): these patches stay vendored on `main` long-term; no
upstream PR is planned for now.** Rationale, maintenance-on-tract-bump, and
when-to-drop are in [vendor/README.md](README.md). Ready-to-file PR drafts are
kept in [UPSTREAM-PRS.md](UPSTREAM-PRS.md) in case we change our mind.

Decision & rationale: [docs/analysis/2026-06-14-vendored-tract-patch-on-main.md](../docs/analysis/2026-06-14-vendored-tract-patch-on-main.md).
Root cause analysis: [docs/analysis/2026-06-14-why-tract-cant-run-pp-doclayoutv2.md](../docs/analysis/2026-06-14-why-tract-cant-run-pp-doclayoutv2.md).

Each fix below is bracketed with a `docparse PATCH` comment at the call site.

## #1 — tract-hir 0.23.1 · GatherNd shape-inference bug — STATUS: ✅ verified

File: `tract-hir/src/ops/array/gather_nd.rs` (`InferenceRulesOp::rules`, rule B).

Upstream constrained the output's trailing dims against `inputs[1]` (indices);
ONNX GatherND output = `indices.shape[:-1] ++ data.shape[n + batch_dims..]`, so
the trailing dims come from `inputs[0]` (data). Symptom on PP-DocLayoutV2:
`GatherND.0: out.shape[2] == inputs[1].shape[0]: unify Val(4) with Val(1)`.
The typed-level `tract-core` `compute_shape` is already correct — only the HIR
inference rule was wrong. Fix references `inputs[0]` with offset `n+batch_dims`.

Effect: full simplified PP-DocLayoutV2 passes typecheck + optimize.
Upstream: not filed (decision above). Draft ready in UPSTREAM-PRS.md (clear bug, minimal repro available).

## #2 — tract-core 0.23.1 · TopK over TDim input — STATUS: ✅ verified

File: `tract-core/src/ops/array/topk.rs` (`EvalOp::eval`).

`tract-onnx`'s Cast maps ONNX `Cast(to=INT64)` → `TDim` (cast.rs:17-19, for
shape-arithmetic friendliness). In RT-DETR a `GreaterOrEqual` mask is cast to
int64 *data* and fed to TopK; arriving as TDim, `dispatch_numbers!` rejects it
(`"TDim is not a number"`). Fix: when the input dt is TDim, cast to i64 for the
sort (TDim holds concrete ints at eval), then cast the values output back to
TDim so the declared output fact still matches. Indices output is always i64.
Localized to TopK; semantics unchanged for numeric inputs.

Effect: PP-DocLayoutV2 evals **end-to-end** in tract; output matches ONNX
Runtime on 5 sample pages (class 100%, score Δ<1.3e-6, box Δ<5e-4 px, reading
order identical).
Upstream: not filed (decision above). Draft ready in UPSTREAM-PRS.md (TopK should accept TDim/concrete-dim input).

---
**Gate G (plan §0): only 2 non-trivial tract fixes needed (≤6 budget). No
further eval blockers after #2 — `GatherND×5 / TopK×4 / ScatterND×2 /
GatherElements×4 / GridSample×18` all run.**
