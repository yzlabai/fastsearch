# Upstream PR drafts for the vendored tract patches

Two minimal fixes against **tract 0.23.1** that let RT-DETR–style detectors
(PP-DocLayoutV2) load and run. File these against https://github.com/sonos/tract ,
then once released, drop `vendor/` and return to a version dependency.

Both ship with a minimal repro and do not change behavior for existing op uses.

---

## PR 1 — Fix GatherNd output-shape inference (references indices instead of data)

**Crate/file**: `tract-hir/src/ops/array/gather_nd.rs` (`InferenceRulesOp::rules`).

**Bug**: The rule constraining the output's trailing dimensions binds them to
`inputs[1]` (the *indices*), but per the ONNX spec the output is
`indices.shape[:-1] ++ data.shape[batch_dims + n ..]` — the trailing dims come
from `inputs[0]` (the *data*), where `n = indices.shape[-1]`. The loop bound
also uses the indices rank instead of the data rank.

The typed-level `tract-core` `GatherNd::compute_shape` is already correct
(`shape.extend(data_shape[n + batch_dims..])`); only the HIR inference rule is
wrong. When the output dims are otherwise unconstrained the bug silently infers
a wrong shape; when downstream pins the correct dim it surfaces as an
unsatisfiable unification.

**Minimal repro** (analyse fails):
```
data    : f32[1, 13125, 4]
indices : i64[1, 300, 2]      # batch_dims = 0
GatherND -> expected f32[1, 300, 4]
# tract 0.23.1: "outputs[0].shape[2] == inputs[1].shape[0]: unify Val(4) with Val(1)"
```

**Fix** (rule B):
```rust
let batch_dims = self.batch_dims;
s.given_2(&inputs[1].shape[indices_rank - 1], &inputs[0].rank, move |s, n, data_rank| {
    if let Ok(n) = n.to_i64() {
        let n = n as usize;
        for i in 0..(data_rank as usize).saturating_sub(n + batch_dims) {
            s.equals(&outputs[0].shape[indices_rank - 1 + i],
                     &inputs[0].shape[n + batch_dims + i])?;
        }
    }
    Ok(())
})
```

---

## PR 2 — TopK: accept TDim input (concrete dims)

**Crate/file**: `tract-core/src/ops/array/topk.rs` (`EvalOp::eval`).

**Context**: `tract-onnx` maps ONNX `Cast(to=INT64)` to `TDim`
(`tract-onnx/src/ops/cast.rs`) for shape-arithmetic friendliness. In RT-DETR a
`GreaterOrEqual` mask is cast to int64 *data* and fed to TopK; arriving as TDim,
the `dispatch_numbers!` dispatch rejects it with `"TDim is not a number"`.

TDim at eval holds concrete integers, so TopK is well-defined: sort as i64.

**Minimal repro**: a model with `Cast(to=INT64)` whose result feeds `TopK`
(e.g. `TopK(Cast(GreaterOrEqual(x, t) -> int64), k)`) fails at eval.

**Fix**: when input dt is `TDim`, cast to i64 for the sort, then cast the values
output back to `TDim` so the declared output fact still matches (indices output
is always i64). Numeric inputs take the unchanged path.
```rust
let in_dt = input.datum_type();
let input = if in_dt == DatumType::TDim {
    input.cast_to::<i64>()?.into_owned().into_tvalue()
} else { input };
// ... existing sort ...
let output_values = if in_dt == DatumType::TDim {
    output_values.cast_to::<TDim>()?.into_owned()
} else { output_values };
```

Alternatively, upstream may prefer fixing this at the source (not mapping
`Cast(to=INT64)` to TDim when the result is consumed as data) — but the TopK-side
fix is the least invasive.
