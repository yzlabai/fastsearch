#!/usr/bin/env python3
"""Quality scoring for the born-digital eval set (roadmap §6 quality scoreboard).

Computes the three metrics the OpenDataLoader benchmark uses, so docparse-rs can
be scored on the SAME axes as Docling (composite 0.882) once a labeled subset
exists:

  NID  — reading-order accuracy (normalized indel distance over the linearized
         block-text sequence; 1.0 = identical order/content).
  TEDS — table-structure similarity. NOTE: this is a *structural proxy* (grid
         shape + cell-content alignment), not full tree-edit-distance TEDS;
         swap in APTED once the annotation format is fixed.  # TODO
  MHS  — title-hierarchy: F1 over (level, normalized-text) heading pairs.

Input format (pred.json / gt.json): one document, or a list of documents:
  { "reading_order": ["block text", ...],
    "tables": [ [["a","b"],["c","d"]], ... ],   # row-major cell text per table
    "headings": [ [1,"Intro"], [2,"Methods"], ... ] }

Usage:
  score.py pred.json gt.json        # print NID/TEDS/MHS + composite
  score.py --selftest               # run synthetic assertions
"""
import sys, json, re, unicodedata
from difflib import SequenceMatcher


def _norm(s):
    # NFKC folds typographic ligatures (ﬁ→fi, ﬂ→fl) and compatibility forms so
    # a system that expands them (ours) and one that keeps the codepoint (ODL)
    # compare equal. Applied to both sides — pure measurement hygiene.
    s = unicodedata.normalize("NFKC", str(s))
    return re.sub(r"\s+", " ", s.strip()).lower()


def _words(seq):
    """Flatten a list of block texts to a normalized word sequence — robust to
    how each system segments blocks (NID compares reading order + content)."""
    return " ".join(_norm(x) for x in seq).split()


def nid(pred, gt):
    """Reading-order + content agreement: order-sensitive word-sequence
    similarity (difflib ratio in [0,1]). Robust to block segmentation."""
    a, b = _words(pred.get("reading_order", [])), _words(gt.get("reading_order", []))
    if not a and not b:
        return 1.0
    return SequenceMatcher(None, a, b, autojunk=False).ratio()


def _row_sim(pr_row, gt_row, cols):
    """Fraction of column-aligned cells with equal text, over `cols`. Cells
    empty on BOTH sides don't count as agreement (no content to compare)."""
    match = 0
    for j in range(cols):
        p = _norm(pr_row[j]) if j < len(pr_row) else ""
        g = _norm(gt_row[j]) if j < len(gt_row) else ""
        if p and p == g:
            match += 1
    return match / cols if cols else 0.0


def _teds_one(pt, gt):
    """Structural proxy for one table: shape similarity + cell-content match
    under a monotonic ROW ALIGNMENT (DP over row pairs, mirroring the row
    insert/delete edits of real tree-edit-distance TEDS). Rigid index pairing
    made the score collapse when one side emits a single extra header row —
    every following data row misaligned cascade-style, scoring 0 despite
    identical content. Alignment is symmetric: it can only recover genuinely
    equal rows, never invent agreement; unmatched rows still dilute via the
    max-rows denominator."""
    pr, gr = len(pt), len(gt)
    pc = max((len(r) for r in pt), default=0)
    gc = max((len(r) for r in gt), default=0)
    if pr == 0 and gr == 0:
        return 1.0
    shape = (1 - abs(pr - gr) / max(pr, gr, 1)) * (1 - abs(pc - gc) / max(pc, gc, 1))
    rows = max(pr, gr)
    cols = max(pc, gc)
    total = rows * cols
    if total == 0:
        return shape
    # DP: best monotonic pairing of pred rows to gt rows by cell-match score.
    best = [[0.0] * (gr + 1) for _ in range(pr + 1)]
    for i in range(1, pr + 1):
        for j in range(1, gr + 1):
            pair = best[i - 1][j - 1] + _row_sim(pt[i - 1], gt[j - 1], cols)
            best[i][j] = max(pair, best[i - 1][j], best[i][j - 1])
    content = best[pr][gr] * cols / total
    return 0.3 * shape + 0.7 * content


def _is_table(t):
    """A table needs 2-D structure: ≥2 rows AND ≥2 columns. A 1×N / N×1 'table'
    is a list or a stray figure fragment, not a grid — applied symmetrically to
    predicted and reference so neither side is credited/penalized for degenerate
    detections (e.g. ODL emits 1×2 page-number fragments and chart-axis rows as
    'tables' on 2203). A grid whose every cell is EMPTY is equally degenerate:
    it is line-art inside a figure with no extractable content (ODL emits 6 such
    on 2305), so there is nothing for a content-weighted metric to compare —
    also filtered symmetrically."""
    return (
        len(t) >= 2
        and max((len(r) for r in t), default=0) >= 2
        and any(str(c).strip() for r in t for c in r)
    )


def teds(pred, gt):
    pts = [t for t in pred.get("tables", []) if _is_table(t)]
    gts = [t for t in gt.get("tables", []) if _is_table(t)]
    if not pts and not gts:
        return 1.0
    # Match tables by best content overlap, NOT by emission index: two systems
    # emit tables in different orders and detect different subsets, so index
    # pairing compares unrelated tables and understates a correct extraction
    # (e.g. redp5110: we extract the right "Special register"/"Global variable"
    # tables but at shifted indices). Greedy max-similarity assignment; each
    # table used once; unmatched predicted/gt tables score 0 (spurious/missed),
    # keeping detection recall honest. Denominator = max count.
    pairs = sorted(
        ((_teds_one(p, g), i, j) for i, p in enumerate(pts) for j, g in enumerate(gts)),
        reverse=True,
    )
    used_p, used_g, matched = set(), set(), 0.0
    for s, i, j in pairs:
        if i in used_p or j in used_g:
            continue
        used_p.add(i)
        used_g.add(j)
        matched += s
    n = max(len(pts), len(gts))
    return matched / n if n else 1.0


# ---------------------------------------------------------------------------
# Exact TEDS (H5): true tree-edit distance over <table><tr><td> trees with the
# PubTabNet cost model — insert/delete = 1; renaming a <td> with EQUAL spans
# costs the normalized text difference, anything else costs 1. The distance is
# computed exactly with the Zhang-Shasha algorithm (the same distance APTED
# computes; ZS is ~80 lines of stdlib Python, so no eval-side dependency).
# Input: per-table cell lists `{"rows": R, "cols": C, "cells": [[r, c, rs, cs,
# text], ...]}` (ANCHORS only — covered span positions are not repeated), so
# span structure is finally scored instead of being flattened away.
# ---------------------------------------------------------------------------


class _N:
    __slots__ = ("label", "text", "children")

    def __init__(self, label, text=""):
        self.label, self.text, self.children = label, text, []


def _table_tree(tc):
    """Cells table → tree: table → tr per row index → td:{rs}x{cs} leaves."""
    rows = {}
    for r, c, rs, cs, txt in tc.get("cells", []):
        rows.setdefault(r, []).append((c, rs, cs, txt))
    root = _N("table")
    for r in sorted(rows):
        tr = _N("tr")
        for c, rs, cs, txt in sorted(rows[r]):
            tr.children.append(_N(f"td:{rs}x{cs}", _norm(txt)))
        root.children.append(tr)
    return root


def _postorder(root):
    """Post-order node list + leftmost-leaf-descendant index per node."""
    nodes, lld = [], []

    def go(n):
        first = None
        for ch in n.children:
            f = go(ch)
            if first is None:
                first = f
        nodes.append(n)
        i = len(nodes) - 1
        lld.append(i if first is None else first)
        return lld[i]

    go(root)
    return nodes, lld


def _keyroots(lld):
    seen, roots = set(), []
    for i in range(len(lld) - 1, -1, -1):
        if lld[i] not in seen:
            roots.append(i)
            seen.add(lld[i])
    return sorted(roots)


def _sub_cost(a, b):
    if a.label != b.label:
        return 1.0
    if a.label.startswith("td") and (a.text or b.text):
        return 1.0 - SequenceMatcher(None, a.text, b.text, autojunk=False).ratio()
    return 0.0


def _tree_edit_distance(t1, t2):
    """Zhang & Shasha 1989: exact TED, unit insert/delete + _sub_cost rename."""
    n1, l1 = _postorder(t1)
    n2, l2 = _postorder(t2)
    td = [[0.0] * len(n2) for _ in range(len(n1))]
    for i in _keyroots(l1):
        for j in _keyroots(l2):
            li, lj = l1[i], l2[j]
            m, n = i - li + 2, j - lj + 2
            fd = [[0.0] * n for _ in range(m)]
            for x in range(1, m):
                fd[x][0] = fd[x - 1][0] + 1
            for y in range(1, n):
                fd[0][y] = fd[0][y - 1] + 1
            for x in range(1, m):
                for y in range(1, n):
                    if l1[li + x - 1] == li and l2[lj + y - 1] == lj:
                        fd[x][y] = min(
                            fd[x - 1][y] + 1,
                            fd[x][y - 1] + 1,
                            fd[x - 1][y - 1] + _sub_cost(n1[li + x - 1], n2[lj + y - 1]),
                        )
                        td[li + x - 1][lj + y - 1] = fd[x][y]
                    else:
                        px = l1[li + x - 1] - li
                        py = l2[lj + y - 1] - lj
                        fd[x][y] = min(
                            fd[x - 1][y] + 1,
                            fd[x][y - 1] + 1,
                            fd[px][py] + td[li + x - 1][lj + y - 1],
                        )
    return td[len(n1) - 1][len(n2) - 1]


def _cells_from_grid(grid):
    """Fallback for span-less sources: every grid position is a 1×1 cell."""
    return {
        "rows": len(grid),
        "cols": max((len(r) for r in grid), default=0),
        "cells": [
            [r, c, 1, 1, str(t)] for r, row in enumerate(grid) for c, t in enumerate(row)
        ],
    }


def _materialize(tc):
    """Cells → replicated grid (to reuse the shared degenerate-table filter)."""
    rows, cols = tc.get("rows", 0), tc.get("cols", 0)
    g = [["" for _ in range(cols)] for _ in range(rows)]
    for r, c, rs, cs, txt in tc.get("cells", []):
        for dr in range(rs):
            for dc in range(cs):
                if r + dr < rows and c + dc < cols:
                    g[r + dr][c + dc] = txt
    return g


def _teds_x_one(pt, gt_):
    a, b = _table_tree(pt), _table_tree(gt_)
    na, _ = _postorder(a)
    nb, _ = _postorder(b)
    return 1.0 - _tree_edit_distance(a, b) / max(len(na), len(nb), 1)


def teds_x(pred, gt):
    """Exact-TED TEDS over span-aware tables. Sources without `tables_cells`
    fall back to 1×1 cells per grid position (same table, no span credit).
    Same greedy best-match pairing and degenerate filter as the proxy."""
    pts = pred.get("tables_cells") or [_cells_from_grid(t) for t in pred.get("tables", [])]
    gts = gt.get("tables_cells") or [_cells_from_grid(t) for t in gt.get("tables", [])]
    pts = [t for t in pts if _is_table(_materialize(t))]
    gts = [t for t in gts if _is_table(_materialize(t))]
    if not pts and not gts:
        return 1.0
    pairs = sorted(
        ((_teds_x_one(p, g), i, j) for i, p in enumerate(pts) for j, g in enumerate(gts)),
        key=lambda x: x[0],
        reverse=True,
    )
    used_p, used_g, matched = set(), set(), 0.0
    for s, i, j in pairs:
        if i in used_p or j in used_g:
            continue
        used_p.add(i)
        used_g.add(j)
        matched += s
    n = max(len(pts), len(gts))
    return matched / n if n else 1.0


def mhs(pred, gt):
    """Heading-hierarchy agreement: F1 over normalized heading TEXT. Level
    numbers are ignored — two systems number levels differently, so we measure
    'are the same headings identified'. (Level-aware refinement is a TODO once
    a single annotation scheme is fixed.)"""
    ph = {_norm(t) for _, t in pred.get("headings", [])}
    gh = {_norm(t) for _, t in gt.get("headings", [])}
    if not ph and not gh:
        return 1.0
    tp = len(ph & gh)
    prec = tp / len(ph) if ph else 0.0
    rec = tp / len(gh) if gh else 0.0
    return 2 * prec * rec / (prec + rec) if (prec + rec) else 0.0


def score_doc(pred, gt):
    s = {"NID": nid(pred, gt), "TEDS": teds(pred, gt), "MHS": mhs(pred, gt)}
    # composite stays on the proxy column during the transition so historical
    # scoreboards remain comparable; TEDS_X is reported alongside (H5).
    s["composite"] = sum(s.values()) / 3
    s["TEDS_X"] = teds_x(pred, gt)
    return s


def _aslist(x):
    return x if isinstance(x, list) else [x]


def selftest():
    a = {"reading_order": ["A", "B", "C"],
         "tables": [[["x", "y"], ["1", "2"]]],
         "headings": [[1, "Intro"], [2, "Methods"]]}
    assert score_doc(a, a)["composite"] == 1.0, "identical → 1.0"
    b = {"reading_order": ["A", "C", "B"], "tables": [], "headings": []}
    assert 0.0 < nid(b, a) < 1.0, "reordered → partial"
    c = {"tables": [[["x", "y"], ["1", "9"]]]}  # one cell differs
    assert 0.0 < teds(c, a) < 1.0, "one wrong cell → partial"
    d = {"headings": [[1, "Intro"]]}  # half the headings
    assert abs(mhs(d, a) - (2 * 1 * 0.5 / 1.5)) < 1e-9, "half headings → F1"
    empty = {}
    assert score_doc(empty, empty)["composite"] == 1.0, "empty == empty"
    # --- exact TEDS (H5) ---
    span_gt = {"tables_cells": [{"rows": 2, "cols": 2,
                                 "cells": [[0, 0, 1, 2, "head"],
                                           [1, 0, 1, 1, "a"], [1, 1, 1, 1, "b"]]}]}
    assert teds_x(span_gt, span_gt) == 1.0, "identical spans → 1.0"
    flat = {"tables_cells": [{"rows": 2, "cols": 2,
                              "cells": [[0, 0, 1, 1, "head"], [0, 1, 1, 1, "head"],
                                        [1, 0, 1, 1, "a"], [1, 1, 1, 1, "b"]]}]}
    fx = teds_x(flat, span_gt)
    assert 0.0 < fx < 1.0, "flattened span must lose credit"
    grid_only = {"tables": [[["head", "head"], ["a", "b"]]]}
    assert abs(teds_x(grid_only, span_gt) - fx) < 1e-9, "grid fallback = 1×1 cells"
    wrong = {"tables_cells": [{"rows": 2, "cols": 2,
                               "cells": [[0, 0, 1, 2, "head"],
                                         [1, 0, 1, 1, "a"], [1, 1, 1, 1, "ZZZ"]]}]}
    wx = teds_x(wrong, span_gt)
    assert fx < wx < 1.0, "right structure + one wrong cell beats flattened"
    assert teds_x(a, a) == 1.0, "grid-vs-grid identical → 1.0"
    print("selftest OK")


if __name__ == "__main__":
    if len(sys.argv) == 2 and sys.argv[1] == "--selftest":
        selftest()
    elif len(sys.argv) == 3:
        pred = _aslist(json.load(open(sys.argv[1])))
        gt = _aslist(json.load(open(sys.argv[2])))
        per = [score_doc(p, g) for p, g in zip(pred, gt)]
        avg = {k: sum(d[k] for d in per) / len(per) for k in per[0]} if per else {}
        print(json.dumps({"per_doc": per, "average": avg}, indent=2, ensure_ascii=False))
    else:
        print(__doc__)
        sys.exit(1)
