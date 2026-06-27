#!/usr/bin/env python3
"""OmniDocBench END-TO-END table eval: unlike table_eval.py (which feeds the GT
table crop straight to UniRec), this runs the WHOLE image-document pipeline —
page image → wrapped PDF → `docparse --ocr --layout --table-model` → the table
the pipeline itself detected+recognized — and scores it against GT with TEDS_X.
It measures the cost of detection (layout finding the table region) on top of
recognition. Single-table pages only, to avoid pred↔GT table matching.

Usage: python3 scripts/eval/omnidocbench/e2e_table_eval.py [N]   (default 8)
"""
import json, os, sys, io, subprocess

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
sys.path.insert(0, os.path.join(ROOT, "scripts", "eval"))
sys.path.insert(0, HERE)
import score as S
from table_eval import html_to_cells, strip_math, IMGDIR, HF, JSON

BIN = os.path.join(ROOT, "target/release/docparse")
MODEL = os.path.join(ROOT, "models/unirec")


def wrap_pdf(image_path):
    from PIL import Image

    img = os.path.join(IMGDIR, image_path)
    if not os.path.exists(img) or os.path.getsize(img) == 0:
        os.makedirs(IMGDIR, exist_ok=True)
        for _ in range(4):
            r = subprocess.run(["curl", "-sL", "--retry", "3", "--max-time", "180",
                                HF + image_path, "-o", img])
            if r.returncode == 0 and os.path.getsize(img) > 0:
                break
        else:
            return None
    im = Image.open(img).convert("RGB")
    w, h = im.size
    buf = io.BytesIO(); im.save(buf, "JPEG", quality=90); jpg = buf.getvalue()
    pw, ph = w * 72 / 96, h * 72 / 96
    objs = [b"<< /Type /Catalog /Pages 2 0 R >>",
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            (f"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {pw:.1f} {ph:.1f}] "
             f"/Resources << /XObject << /Im0 4 0 R >> >> /Contents 5 0 R >>").encode(),
            (f"<< /Type /XObject /Subtype /Image /Width {w} /Height {h} /ColorSpace /DeviceRGB "
             f"/BitsPerComponent 8 /Filter /DCTDecode /Length {len(jpg)} >>").encode()
            + b"\nstream\n" + jpg + b"\nendstream",
            (f"q {pw:.1f} 0 0 {ph:.1f} 0 0 cm /Im0 Do Q").encode()]
    objs[4] = b"<< /Length %d >>\nstream\n" % len(objs[4]) + objs[4] + b"\nendstream"
    out = bytearray(b"%PDF-1.4\n"); offs = []
    for i, o in enumerate(objs, 1):
        offs.append(len(out)); out += b"%d 0 obj\n" % i + o + b"\nendobj\n"
    x = len(out); out += b"xref\n0 %d\n0000000000 65535 f \n" % (len(objs) + 1)
    for o in offs:
        out += b"%010d 00000 n \n" % o
    out += b"trailer\n<< /Size %d /Root 1 0 R >>\nstartxref\n%d\n%%%%EOF\n" % (len(objs) + 1, x)
    p = "/tmp/odb_e2e.pdf"; open(p, "wb").write(bytes(out))
    return p


def ir_table_cells(t):
    # strip_math mirrors the GT side (html_to_cells): UniRec keeps inline math
    # delimiters (\(...\)) in cells while the GT had them stripped, so without
    # this every math-containing cell falsely mismatches (eval asymmetry, B1).
    rows = t["rows"]
    cells = [[r, c, cell.get("row_span", 1), cell.get("col_span", 1),
              S._norm(strip_math(cell.get("text", "")))]
             for r, row in enumerate(rows) for c, cell in enumerate(row) if not cell.get("merged")]
    return {"rows": len(rows), "cols": max((len(r) for r in rows), default=0), "cells": cells}


def main():
    n = int(sys.argv[1]) if len(sys.argv) > 1 else 8
    data = json.load(open(JSON))
    # single-table pages (exactly one table block on the page)
    singles = []
    for page in data:
        tabs = [d for d in page["layout_dets"] if d["category_type"] == "table" and d.get("html")]
        if len(tabs) == 1:
            singles.append((page["page_info"]["image_path"], tabs[0]["html"]))
    singles = singles[:n]
    scores = []
    for i, (ip, gt_html) in enumerate(singles):
        pdf = wrap_pdf(ip)
        gt = html_to_cells(gt_html)
        if not pdf or not gt:
            continue
        cmd = [BIN, pdf, "--ocr", "--layout", "--table-model", MODEL, "-f", "json"]
        # Layout-backend A/B: OMNIDOC_LAYOUT_MODEL selects YOLO vs PP-DocLayoutV2.
        lm = os.environ.get("OMNIDOC_LAYOUT_MODEL")
        if lm:
            cmd += ["--layout-model", lm]
        r = subprocess.run(cmd, capture_output=True, text=True)
        pred = None
        if r.returncode == 0 and r.stdout.strip():
            doc = json.loads(r.stdout)
            for p in doc["pages"]:
                for e in p["elements"]:
                    if e.get("type") == "table" and e.get("rows"):
                        pred = ir_table_cells(e); break
        teds = S.teds_x({"tables_cells": [pred]}, {"tables_cells": [gt]}) if pred else 0.0
        scores.append(teds)
        print(f"[{i+1}/{len(singles)}] e2e TEDS {teds:.3f}  ({ip[:24]}{' no-table' if not pred else ''})")
    if scores:
        print(f"\n== end-to-end (detect+recognize) table TEDS_X: "
              f"mean {sum(scores)/len(scores):.3f} over {len(scores)} single-table pages ==")


if __name__ == "__main__":
    main()
