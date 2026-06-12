#!/usr/bin/env python3
"""OmniDocBench end-to-end TEXT eval: how well our OCR pipeline extracts a page
image's body text in reading order, vs the human GT. Complements the table
TEDS picture (table_eval/e2e_table_eval) — together they profile what we do on
image documents. Honest: our OCR is the lightweight PP-OCRv4 mobile, so this
exercises that on OmniDocBench's diverse layouts (papers/books/exams/news/...).

Per page: page image → wrapped PDF → `docparse --ocr --layout -f text` → our
text; GT = readable blocks concatenated in `order`. Score = difflib ratio
(order-sensitive word-sequence similarity, same as the project's NID).

Usage: python3 scripts/eval/omnidocbench/e2e_text_eval.py [N]   (default 10)
"""
import json, os, sys, subprocess, unicodedata, re
from difflib import SequenceMatcher

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
sys.path.insert(0, HERE)
from e2e_table_eval import wrap_pdf
from table_eval import JSON

BIN = os.path.join(ROOT, "target/release/docparse")
import os as _os  # ROOT used above

# Body categories worth comparing (exclude furniture: headers/footers/page
# numbers/captions/figures/tables/equations/masks/abandon).
READABLE = {"text_block", "title", "list_group", "code_txt", "reference"}


def norm(s):
    s = unicodedata.normalize("NFKC", str(s))
    return re.sub(r"\s+", " ", s.strip()).lower()


def gt_text(page):
    blocks = [d for d in page["layout_dets"] if d["category_type"] in READABLE and d.get("text")]
    blocks.sort(key=lambda d: d.get("order", 0))
    return " ".join(norm(d["text"]) for d in blocks)


def main():
    n = int(sys.argv[1]) if len(sys.argv) > 1 else 10
    data = json.load(open(JSON))
    # pages with a meaningful amount of body text
    doctype = os.environ.get("OMNIDOC_DOCTYPE")
    cand = [p for p in data
            if (not doctype or (p["page_info"].get("page_attribute", {}) or {}).get("data_source") == doctype)
            and sum(len(d.get("text", "")) for d in p["layout_dets"] if d["category_type"] in READABLE) > 200]
    cand = cand[:n]
    scores = []
    for i, page in enumerate(cand):
        ip = page["page_info"]["image_path"]
        gt = gt_text(page)
        pdf = wrap_pdf(ip)
        if not pdf or not gt:
            continue
        mode = os.environ.get("OMNIDOC_TEXT_MODE", "ocr")
        args = ([BIN, pdf, "--transcribe-model", os.path.join(ROOT, "models/unirec"), "-f", "text"]
                if mode == "transcribe"
                else [BIN, pdf, "--ocr", "--layout", "-f", "text"])
        r = subprocess.run(args, capture_output=True, text=True)
        ours = norm(r.stdout)
        # CHARACTER-level similarity: CJK has no spaces, so word-level split()
        # collapses a whole Chinese page into one token and breaks the metric.
        # OmniDocBench's text metric is normalized edit distance (char-level).
        sim = SequenceMatcher(None, gt, ours, autojunk=False).ratio()
        scores.append(sim)
        dt = page["page_info"].get("page_attribute", {}).get("data_source", "?")
        print(f"[{i+1}/{len(cand)}] text sim {sim:.3f}  ({dt[:18]})")
    if scores:
        print(f"\n== end-to-end text similarity on OmniDocBench: "
              f"mean {sum(scores)/len(scores):.3f} over {len(scores)} pages ==")


if __name__ == "__main__":
    main()
