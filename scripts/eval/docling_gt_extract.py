#!/usr/bin/env python3
"""Convert a Docling groundtruth JSON (tests/data/groundtruth/docling_v2/*.json)
into score.py's eval format — the SAME format extract.py emits for docparse-rs,
so the two can be scored against each other.

NOTE: Docling's groundtruth is Docling's OWN output (regression baseline), so
scoring docparse-rs against it measures AGREEMENT WITH DOCLING on born-digital
structure, not accuracy vs human truth.

Usage: docling_gt_extract.py groundtruth.json > ref.json
"""
import sys, json

READABLE = {"title", "section_header", "text", "paragraph", "list_item",
            "caption", "footnote", "code", "formula"}

d = json.load(open(sys.argv[1]))
texts = d.get("texts", [])
tables = d.get("tables", [])
out = {"reading_order": [], "tables": [], "headings": []}


def idx(ref):
    return int(ref.split("/")[-1])


def walk(children):
    for ch in children:
        ref = ch.get("$ref", "")
        if ref.startswith("#/texts/"):
            t = texts[idx(ref)]
            lbl, txt = t.get("label"), t.get("text", "")
            if lbl in READABLE and txt.strip():
                out["reading_order"].append(txt)
            if lbl == "title":
                out["headings"].append([1, txt])
            elif lbl == "section_header":
                out["headings"].append([t.get("level", 2), txt])
        elif ref.startswith("#/tables/"):
            grid = tables[idx(ref)].get("data", {}).get("grid", [])
            rows = [[c.get("text", "") for c in row] for row in grid]
            if rows:
                out["tables"].append(rows)
        elif ref.startswith("#/groups/"):
            # Docling nests lists (and other groupings) as `group` nodes whose
            # children are the actual texts. Recurse, else every list item is
            # dropped from the reference reading order — artificially lowering
            # NID on every list-bearing doc.
            walk(groups[idx(ref)].get("children", []))
        # pictures are skipped for these three metrics.


groups = d.get("groups", [])
walk(d.get("body", {}).get("children", []))
json.dump(out, sys.stdout, ensure_ascii=False, indent=2)
