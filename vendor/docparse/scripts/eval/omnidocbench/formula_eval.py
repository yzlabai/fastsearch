#!/usr/bin/env python3
"""OmniDocBench formula single-module eval: UniRec raw formula recognition
(crop the GT equation region → UniRec → LaTeX) vs the human LaTeX ground truth,
scored with a normalized character-level similarity over normalized LaTeX (a
light proxy for the official CDM, which needs rendering). Mirrors table_eval.py.

Usage: python3 scripts/eval/omnidocbench/formula_eval.py [N]   (default 30)
"""
import json, os, sys, re
from difflib import SequenceMatcher

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
sys.path.insert(0, HERE)
from table_eval import recognize, JSON


def norm_latex(s):
    """Normalize LaTeX for comparison: strip math-mode wrappers and the
    `\\left`/`\\right` SIZING prefixes (only when followed by a delimiter, so
    `\\leftarrow`/`\\rightarrow` are NOT corrupted), then drop whitespace."""
    s = s.strip()
    for w in ["$$", "$", "\\(", "\\)", "\\[", "\\]"]:
        s = s.replace(w, "")
    # \left( \right] \left. etc. — only before an actual delimiter char.
    s = re.sub(r"\\left(?=[([{|.\\])", "", s)
    s = re.sub(r"\\right(?=[)\]}|.\\])", "", s)
    return re.sub(r"\s+", "", s)


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
            if det["category_type"] == "equation_isolated" and det.get("latex"):
                tasks.append((ip, det["poly"], det["latex"]))
    tasks = tasks[:n]
    scores = []
    for i, (ip, poly, gt_latex) in enumerate(tasks):
        pred = recognize(ip, poly)
        if not pred:
            scores.append(0.0)
            print(f"[{i+1}/{len(tasks)}] sim 0.000 (no prediction)")
            continue
        sim = SequenceMatcher(None, norm_latex(gt_latex), norm_latex(pred), autojunk=False).ratio()
        scores.append(sim)
        print(f"[{i+1}/{len(tasks)}] sim {sim:.3f}  ({ip[:24]})")
    if scores:
        print(f"\n== UniRec formula recognition on OmniDocBench: "
              f"mean LaTeX-sim {sum(scores)/len(scores):.3f} over {len(scores)} formulas ==")


if __name__ == "__main__":
    main()
