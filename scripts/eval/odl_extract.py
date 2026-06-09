#!/usr/bin/env python3
"""Convert an OpenDataLoader-PDF JSON output into score.py's eval format.

ODL is a *deterministic* structure extractor (Java/veraPDF wcag-algs) — the same
class of system as docparse-rs — so matching/beating it is an achievable target
(unlike Docling's neural table model). Output: typed `kids` tree
(heading/paragraph/caption/table); tables carry row/col spans.

Usage: odl_extract.py odl_output.json > ref.json
"""
import sys, json

READ = {"paragraph", "heading", "caption", "text", "title", "list", "list item",
        "footnote", "code", "formula"}


def table_grid(node):
    nr = node.get("number of rows", 0) or 0
    nc = node.get("number of columns", 0) or 0
    grid = [["" for _ in range(nc)] for _ in range(nr)]
    for row in node.get("rows", []) or []:
        for cell in row.get("cells", []) or []:
            r = (cell.get("row number", 1) or 1) - 1
            c = (cell.get("column number", 1) or 1) - 1
            rs = cell.get("row span", 1) or 1
            cs = cell.get("column span", 1) or 1
            txt = " ".join(
                k.get("content") or k.get("text","") for k in (cell.get("kids", []) or []) if (k.get("content") or k.get("text"))
            ).strip()
            for dr in range(rs):
                for dc in range(cs):
                    if 0 <= r + dr < nr and 0 <= c + dc < nc:
                        grid[r + dr][c + dc] = txt
    return grid


def main():
    d = json.load(open(sys.argv[1]))
    out = {"reading_order": [], "tables": [], "headings": []}

    def walk(node):
        t = node.get("type")
        if t == "table":
            g = table_grid(node)
            if g:
                out["tables"].append(g)
            return
        txt = node.get("content") or node.get("text")
        if t in READ and txt and txt.strip():
            out["reading_order"].append(txt)
            if t in ("heading", "title"):
                try:
                    lvl = int(node.get("level", 1) or 1)
                except (ValueError, TypeError):
                    lvl = 1
                out["headings"].append([lvl, txt])
        # ODL nests children under "kids", but `list` nodes put their entries
        # under "list items". Recurse into both, else every list's text is
        # silently dropped from the reference reading order — which made NID
        # artificially low on every list-bearing doc.
        for k in (node.get("kids", []) or []) + (node.get("list items", []) or []):
            walk(k)

    for k in (d.get("kids", []) or []) + (d.get("list items", []) or []):
        walk(k)
    json.dump(out, sys.stdout, ensure_ascii=False, indent=2)


main()
