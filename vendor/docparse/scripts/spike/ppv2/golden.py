#!/usr/bin/env python3
"""Spike (eval-only): render a sample page, preprocess, dump raw tract inputs +
ONNX-Runtime golden boxes (the reference tract must match).

Usage: golden.py [pdf] [page]   (default: 1901.03003.pdf p0)
Writes to $SPIKE_DIR (default /tmp/ppv2_spike):
  in_image.f32 [1,3,800,800], in_imshape.f32 [1,2], in_scale.f32 [1,2]
  ort_boxes.npy [N,8]
"""
import os, sys, numpy as np, onnxruntime as ort, fitz
from PIL import Image

D = os.environ.get("SPIKE_DIR", "/tmp/ppv2_spike"); os.makedirs(D, exist_ok=True)
MODEL = "models/layout-ppv2/PP-DoclayoutV2_simp.onnx"
pdf = sys.argv[1] if len(sys.argv) > 1 else "../opendataloader-pdf/samples/pdf/1901.03003.pdf"
pg = int(sys.argv[2]) if len(sys.argv) > 2 else 0

d = fitz.open(pdf); pm = d[pg].get_pixmap(matrix=fitz.Matrix(2, 2), alpha=False)
img = Image.frombytes('RGB', [pm.width, pm.height], pm.samples); d.close()
ow, oh = img.size
rs = img.resize((800, 800), Image.BILINEAR)
a = (np.asarray(rs, dtype=np.float32) / 255.0).transpose(2, 0, 1)[None].copy()
sf = np.array([[800 / oh, 800 / ow]], dtype=np.float32)
sh = np.array([[800., 800.]], dtype=np.float32)
a.tofile(f"{D}/in_image.f32"); sf.tofile(f"{D}/in_scale.f32"); sh.tofile(f"{D}/in_imshape.f32")

s = ort.InferenceSession(MODEL, providers=['CPUExecutionProvider'])
out = s.run(None, {'image': a, 'im_shape': sh, 'scale_factor': sf})
b = out[0]
np.save(f"{D}/ort_boxes.npy", b)
keep = b[b[:, 1] > 0.5]; keep = keep[np.argsort(keep[:, 6])]
print(f"{pdf} p{pg} orig={ow}x{oh}  ORT boxes>0.5: {keep.shape[0]}")
for r in keep[:10]:
    print(f"  cls={int(r[0]):2d} score={r[1]:.3f} box=[{r[2]:.1f},{r[3]:.1f},{r[4]:.1f},{r[5]:.1f}] order={r[6]:.1f}")
