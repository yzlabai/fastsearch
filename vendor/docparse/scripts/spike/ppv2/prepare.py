#!/usr/bin/env python3
"""Spike (eval-only): static-ize PP-DocLayoutV2 ONNX for tract.

Official export has dynamic batch + a hardcoded-shape mix that tract's shape
inference rejects. This pins batch=1, re-runs shape inference, then onnx-
simplifier (constant-folds the dynamic head: anchors=13125, queries=300).

Out: models/layout-ppv2/PP-DoclayoutV2_simp.onnx  (gitignored)

Needs the spike venv: onnx, onnxsim. NOT a project runtime dependency.
"""
import sys, onnx
from onnx import shape_inference
from onnx.tools import update_model_dims
from onnxsim import simplify

SRC = "models/layout-ppv2/PP-DoclayoutV2.onnx"
OUT = "models/layout-ppv2/PP-DoclayoutV2_simp.onnx"

m = onnx.load(SRC)
ins = {i.name: [d.dim_value if d.HasField('dim_value') else d.dim_param
               for d in i.type.tensor_type.shape.dim] for i in m.graph.input}
print("inputs (orig):", ins)
in_dims = {n: ([1] + v[1:]) for n, v in ins.items()}
out_dims = {o.name: [(d.dim_param or d.dim_value) for d in o.type.tensor_type.shape.dim]
            for o in m.graph.output}
m = update_model_dims.update_inputs_outputs_dims(m, in_dims, out_dims)
m = shape_inference.infer_shapes(m, strict_mode=False, data_prop=True)

overwrite = {n: ([1] + v[1:]) for n, v in ins.items()}
ms, ok = simplify(m, overwrite_input_shapes=overwrite)
assert ok, "onnxsim failed"
onnx.save(ms, OUT)
import collections
c = collections.Counter(n.op_type for n in ms.graph.node)
print("nodes after simp:", sum(c.values()))
print("dynamic-head ops:", {k: c[k] for k in
      ('GridSample', 'GatherND', 'TopK', 'ScatterND', 'GatherElements', 'Range') if k in c})
print("saved ->", OUT)
