#!/usr/bin/env python3
"""Score docparse-rs against OpenDataLoader-PDF (ODL) on born-digital PDFs.

ODL is a deterministic structure extractor (Java/veraPDF wcag-algs) — the same
class as docparse-rs — so this is an *achievable* target. Reference = ODL JSON
output (run separately). Metrics: NID/TEDS/MHS via score.py.

Usage: compare_odl.py [pdf_dir] [odl_out_dir]
"""
import subprocess, json, sys, glob, os

here = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, here)
import score as S

PDFDIR = sys.argv[1] if len(sys.argv) > 1 else "tmp/refer/docling/tests/data/pdf"
ODLDIR = sys.argv[2] if len(sys.argv) > 2 else "/tmp/odl_out"
BIN = "target/release/docparse"


def run(cmd, stdin=None):
    return subprocess.run(cmd, input=stdin, capture_output=True, text=True)


def our_pred(pdf):
    r = run([BIN, pdf, "-f", "chunks"])
    if r.returncode != 0 or not r.stdout.strip():
        return None
    # Second pass for span-aware tables (TEDS_X / H5): -f json keeps Cell
    # row_span/col_span that the flat chunks rendering throws away.
    import tempfile
    args = []
    rj = run([BIN, pdf, "-f", "json"])
    tmp = None
    if rj.returncode == 0 and rj.stdout.strip():
        tmp = tempfile.NamedTemporaryFile("w", suffix=".json", delete=False)
        tmp.write(rj.stdout)
        tmp.close()
        args = [tmp.name]
    try:
        return json.loads(run(["python3", f"{here}/extract.py", *args], stdin=r.stdout).stdout)
    finally:
        if tmp:
            os.unlink(tmp.name)


def odl_ref(j):
    return json.loads(run(["python3", f"{here}/odl_extract.py", j]).stdout)


rows = []
for pdf in sorted(glob.glob(f"{PDFDIR}/*.pdf")):
    name = os.path.basename(pdf)[:-4]
    odl = f"{ODLDIR}/{name}.json"
    if not os.path.exists(odl):
        continue
    pred = our_pred(pdf)
    ref = odl_ref(odl)
    rtl = name.startswith("right_to_left")
    rt = len(ref.get("tables", []))
    rows.append({"name": name, "rtl": rtl, "ref_tables": rt,
                 "pred_tables": len(pred.get("tables", [])) if pred else 0,
                 "s": S.score_doc(pred, ref) if pred else None})


def mean(xs):
    xs = list(xs)
    return sum(xs) / len(xs) if xs else 0.0


ltr = [r for r in rows if not r["rtl"] and r["s"]]
tabled = [r for r in rows if not r["rtl"] and r["s"] and r["ref_tables"] > 0]

print("# 测试结果 · 与 OpenDataLoader (ODL) 同台（born-digital）\n")
print(f"> 日期：2026-06-09 · 来源：`scripts/eval/compare_odl.py` · 参照：ODL JSON 输出（{ODLDIR}）\n")
print("> ODL 是**确定性**结构抽取器（Java/veraPDF wcag-algs），与本项目同类——其水平**确定性可达**。"
      "NID=阅读顺序(词级)，TEDS=表格结构代理，MHS=标题文本集 F1。\n")
print("| 文档 | NID | TEDS | TEDS_X | MHS | 备注 |")
print("|---|---|---|---|---|---|")
for r in rows:
    s = r["s"]
    note = "RTL" if r["rtl"] else (f"表 我方{r['pred_tables']}/ODL{r['ref_tables']}" if r["ref_tables"] else "")
    if s is None:
        print(f"| {r['name']} | — | — | — | — | 解析失败 {note} |")
    else:
        print(f"| {r['name']} | {s['NID']:.3f} | {s['TEDS']:.3f} | {s['TEDS_X']:.3f} | {s['MHS']:.3f} | {note} |")

print("\n## 汇总（去 RTL）\n")
print(f"- LTR {len(ltr)} 份：NID **{mean(r['s']['NID'] for r in ltr):.3f}**、MHS **{mean(r['s']['MHS'] for r in ltr):.3f}**")
print(f"- 含表 {len(tabled)} 份：TEDS **{mean(r['s']['TEDS'] for r in tabled):.3f}**、TEDS_X **{mean(r['s']['TEDS_X'] for r in tabled):.3f}**（精确树编辑距离，H5）")
print(f"- 评测 {len(rows)} 份（有 ODL 输出者）。")
