#!/usr/bin/env python3
"""OmniDocBench table single-module eval: UniRec raw table recognition vs the
human HTML ground truth, scored with the project's exact Zhang-Shasha TEDS.

This is the "换个 benchmark" experiment (plan: docs/plans/omnidocbench-benchmark.md):
the vs-ODL/Docling scoreboard measures *agreement* against flattened-口径 truth,
which penalizes the model's true span structure. OmniDocBench gives HTML truth
with real spans, so the model's structure is finally scored fairly.

Pipeline per table: crop its poly from the page image (3× upscale, matching
refine_tables' RENDER_SCALE) → raw-RGB blob → the `odb_recognize` example runs
UniRec → predicted HTML → both HTML parsed to span-aware cell trees → TEDS_X.

Usage:
  python3 scripts/eval/omnidocbench/table_eval.py [N]   # first N tables (default 30)
Needs: tmp/omnidocbench/OmniDocBench.json, network for page images (cached),
       target/release/examples/odb_recognize, models/unirec.
"""
import json, os, sys, struct, subprocess, urllib.request
from html.parser import HTMLParser

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
sys.path.insert(0, os.path.join(ROOT, "scripts", "eval"))
import score as S  # reuse teds_x / _norm

JSON = os.path.join(ROOT, "tmp/omnidocbench/OmniDocBench.json")
IMGDIR = os.path.join(ROOT, "tmp/omnidocbench/images")
EXE = os.path.join(ROOT, "target/release/examples/odb_recognize")
MODEL = os.path.join(ROOT, "models/unirec")
HF = "https://huggingface.co/datasets/opendatalab/OmniDocBench/resolve/main/images/"


def strip_math(t):
    """Normalize math delimiters so `$d_w$` == `\\(d_w\\)` == `d_w`."""
    for a, b in [("$", "$"), ("\\(", "\\)"), ("\\[", "\\]")]:
        # crude but symmetric: drop the wrappers wherever they appear
        t = t.replace(a, "").replace(b, "")
    return t


class TableHTML(HTMLParser):
    """Parse <table> into span-aware anchor cells with hanging-grid bookkeeping
    (rowspan/colspan placed at the next free column, covered cells skipped)."""

    def __init__(self):
        super().__init__()
        self.rows = []  # list of list of (col, rs, cs, text)
        self.cur = None
        self.text = []
        self.rs = 1
        self.cs = 1
        self.in_cell = False

    def handle_starttag(self, tag, attrs):
        a = dict(attrs)
        if tag == "tr":
            self.cur = []
        elif tag in ("td", "th"):
            self.in_cell = True
            self.text = []
            self.rs = int(a.get("rowspan", 1) or 1)
            self.cs = int(a.get("colspan", 1) or 1)

    def handle_endtag(self, tag):
        if tag in ("td", "th") and self.in_cell:
            self.cur.append((self.rs, self.cs, "".join(self.text)))
            self.in_cell = False
        elif tag == "tr" and self.cur is not None:
            self.rows.append(self.cur)
            self.cur = None

    def handle_data(self, data):
        if self.in_cell:
            self.text.append(data)


def html_to_cells(html):
    p = TableHTML()
    try:
        p.feed(html)
    except Exception:
        return None
    # Assign columns honoring spans from earlier rows (hanging grid).
    occupied = {}  # (r,c) -> True for covered positions
    cells = []
    ncols = 0
    for r, row in enumerate(p.rows):
        c = 0
        for rs, cs, text in row:
            while occupied.get((r, c)):
                c += 1
            cells.append([r, c, rs, cs, strip_math(text)])
            for dr in range(rs):
                for dc in range(cs):
                    occupied[(r + dr, c + dc)] = True
            c += cs
            ncols = max(ncols, c)
    nrows = len(p.rows)
    if nrows < 1 or ncols < 1:
        return None
    return {"rows": nrows, "cols": ncols, "cells": cells}


def recognize(image_path, poly):
    """Crop the table region, run UniRec, return predicted HTML (or None)."""
    from PIL import Image

    img = os.path.join(IMGDIR, image_path)
    if not os.path.exists(img) or os.path.getsize(img) == 0:
        os.makedirs(IMGDIR, exist_ok=True)
        ok = False
        for _ in range(4):  # HF occasionally drops the TLS handshake (curl 35)
            r = subprocess.run(
                ["curl", "-sL", "--retry", "3", "--max-time", "180", HF + image_path, "-o", img]
            )
            if r.returncode == 0 and os.path.exists(img) and os.path.getsize(img) > 0:
                ok = True
                break
        if not ok:
            return None
    im = Image.open(img).convert("RGB")
    xs = poly[0::2]
    ys = poly[1::2]
    box = (int(min(xs)), int(min(ys)), int(max(xs)), int(max(ys)))
    crop = im.crop(box)
    if crop.width < 8 or crop.height < 8:
        return None
    crop = crop.resize((crop.width * 3, crop.height * 3), Image.LANCZOS)
    blob = struct.pack("<II", crop.width, crop.height) + crop.tobytes()
    tmp = "/tmp/odb_tbl.rgb"
    open(tmp, "wb").write(blob)
    r = subprocess.run([EXE, MODEL, tmp, "2000"], capture_output=True, text=True)
    return r.stdout if r.returncode == 0 and r.stdout.strip() else None


def main():
    n = int(sys.argv[1]) if len(sys.argv) > 1 else 30
    doctype = os.environ.get("OMNIDOC_DOCTYPE")  # e.g. academic_literature
    data = json.load(open(JSON))
    tasks = []
    for page in data:
        if doctype and (page["page_info"].get("page_attribute", {}) or {}).get("data_source") != doctype:
            continue
        ip = page["page_info"]["image_path"]
        for det in page["layout_dets"]:
            if det["category_type"] == "table" and det.get("html"):
                tasks.append((ip, det["poly"], det["html"]))
    tasks = tasks[:n]
    scores = []
    for i, (ip, poly, gt_html) in enumerate(tasks):
        gt = html_to_cells(gt_html)
        pred_html = recognize(ip, poly)
        pred = html_to_cells(pred_html) if pred_html else None
        if not gt:
            continue
        if not pred:
            scores.append(0.0)
            print(f"[{i+1}/{len(tasks)}] TEDS 0.000 (no prediction)")
            continue
        teds = S.teds_x({"tables_cells": [pred]}, {"tables_cells": [gt]})
        scores.append(teds)
        print(f"[{i+1}/{len(tasks)}] TEDS {teds:.3f}  ({ip[:24]})")
    if scores:
        print(f"\n== UniRec table recognition on OmniDocBench: "
              f"mean TEDS_X {sum(scores)/len(scores):.3f} over {len(scores)} tables ==")


if __name__ == "__main__":
    main()
