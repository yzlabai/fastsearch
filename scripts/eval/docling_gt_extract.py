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
out = {"reading_order": [], "tables": [], "headings": [], "tables_cells": []}


def idx(ref):
    return int(ref.split("/")[-1])


def table_cells(grid):
    """Span-aware anchors for TEDS_X (H5): Docling's grid replicates a merged
    cell across every covered position; the anchor is where the start offsets
    equal the position itself."""
    cells = []
    for r, row in enumerate(grid):
        for c, cell in enumerate(row):
            if cell.get("start_row_offset_idx", r) != r or cell.get("start_col_offset_idx", c) != c:
                continue
            cells.append([r, c,
                          cell.get("row_span", 1), cell.get("col_span", 1),
                          cell.get("text", "")])
    return {
        "rows": len(grid),
        "cols": max((len(r) for r in grid), default=0),
        "cells": cells,
    }


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
                out["tables_cells"].append(table_cells(grid))
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
