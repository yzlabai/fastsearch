#!/usr/bin/env python3
"""Convert docparse-rs `-f chunks` JSON into score.py's eval input format.

  docparse <file> -f chunks | extract.py [ir.json] > pred.json

Then: score.py pred.json gt.json
Heading level is proxied by breadcrumb depth (heading_path length + 1).

The optional `ir.json` argument (the same document via `-f json`) adds
span-aware `tables_cells` for the exact-TED TEDS_X column (H5): IR Cell
carries row_span/col_span on the merged region's anchor and `merged: true`
on covered positions — structure the flat chunks rendering throws away.
"""
import sys, json

chunks = json.load(sys.stdin)
pred = {"reading_order": [], "tables": [], "headings": []}
for c in chunks:
    kind = c.get("kind")
    if kind == "table":
        rows = [line.split("\t") for line in c.get("text", "").split("\n") if line]
        pred["tables"].append(rows)
    elif kind == "heading":
        lvl = len(c.get("heading_path", [])) + 1
        pred["headings"].append([lvl, c.get("text", "")])
        pred["reading_order"].append(c.get("text", ""))
    else:
        pred["reading_order"].append(c.get("text", ""))

if len(sys.argv) > 1:
    ir = json.load(open(sys.argv[1]))
    cells_tables = []
    for page in ir.get("pages", []):
        for e in page.get("elements", []):
            if e.get("type") != "table":
                continue
            rows = e.get("rows", [])
            cells = []
            for r, row in enumerate(rows):
                for c, cell in enumerate(row):
                    if cell.get("merged"):
                        continue  # covered position; the anchor carries it
                    cells.append([r, c,
                                  cell.get("row_span", 1), cell.get("col_span", 1),
                                  cell.get("text", "")])
            cells_tables.append({
                "rows": len(rows),
                "cols": max((len(r) for r in rows), default=0),
                "cells": cells,
            })
    pred["tables_cells"] = cells_tables

json.dump(pred, sys.stdout, ensure_ascii=False, indent=2)
