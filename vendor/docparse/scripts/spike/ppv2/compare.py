#!/usr/bin/env python3
"""Spike (eval-only): numeric diff tract output vs ORT golden for PP-DocLayoutV2.
Reads $SPIKE_DIR/ort_boxes.npy and $SPIKE_DIR/tract_boxes.bin (written by the
ppv2_run example), reports box-count / class / coord / order agreement.
"""
import os, sys, numpy as np
D = os.environ.get("SPIKE_DIR", "/tmp/ppv2_spike")
ort = np.load(f"{D}/ort_boxes.npy")
raw = open(f"{D}/tract_boxes.bin", "rb").read()
n = int.from_bytes(raw[0:4], "little"); k = int.from_bytes(raw[4:8], "little")
tr = np.frombuffer(raw[8:], dtype="<f4").reshape(n, k)

def keep(b, thr=0.5):
    b = b[b[:, 1] > thr]
    return b[np.argsort(b[:, 6])]

ok, ot = keep(ort), keep(tr)
print(f"boxes>0.5: ORT={len(ok)} tract={len(ot)}")
if len(ok) != len(ot):
    print("  ❌ count mismatch"); sys.exit(1)
cls_ok = int((ok[:, 0] == ot[:, 0]).sum())
score_d = float(np.abs(ok[:, 1] - ot[:, 1]).max())
box_d = float(np.abs(ok[:, 2:6] - ot[:, 2:6]).max())
order_ok = bool((ok[:, 6] == ot[:, 6]).all())
print(f"  class match : {cls_ok}/{len(ok)}")
print(f"  score maxΔ  : {score_d:.2e}")
print(f"  box   maxΔ  : {box_d:.4f} px")
print(f"  order match : {order_ok}")
verdict = cls_ok == len(ok) and score_d < 1e-3 and box_d < 1.0 and order_ok
print("  =>", "✅ MATCH" if verdict else "❌ DIFF")
sys.exit(0 if verdict else 1)
