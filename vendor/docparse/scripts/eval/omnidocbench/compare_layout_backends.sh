#!/usr/bin/env bash
# OmniDocBench A/B: DocLayout-YOLO vs PP-DocLayoutV2 on the SAME pages, same
# scorer (e2e text + table). One command when the dataset is present.
#
# Usage:   scripts/eval/omnidocbench/compare_layout_backends.sh [N]
#   N        pages per eval (default 30)
#   PY       python interpreter (default python3; needs Pillow for wrap_pdf)
#   OMNIDOC_DOCTYPE   optional data_source filter (e.g. academic_literature)
#
# Prereqs (the script checks and tells you what's missing):
#   - tmp/omnidocbench/OmniDocBench.json  (the dataset; images auto-download/cache)
#   - models/layout-ppv2/PP-DoclayoutV2_simp.onnx  (scripts/spike/ppv2/prepare.py)
#   - target/release/docparse              (cargo build --release -p docparse-cli)
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"
cd "$ROOT"
N="${1:-30}"
PY="${PY:-python3}"
YOLO="models/layout/doclayout_yolo.onnx"
PPV2="models/layout-ppv2/PP-DoclayoutV2_simp.onnx"
BIN="target/release/docparse"

miss=0
[ -f tmp/omnidocbench/OmniDocBench.json ] || { echo "✗ dataset missing: tmp/omnidocbench/OmniDocBench.json"; echo "    get OmniDocBench.json from https://github.com/opendatalab/OmniDocBench and place it there (images auto-cache)."; miss=1; }
[ -f "$PPV2" ] || { echo "✗ PPV2 model missing: $PPV2"; echo "    generate: $PY scripts/spike/ppv2/prepare.py  (needs onnx+onnxsim; from official weights in models/layout-ppv2/PP-DoclayoutV2.onnx)"; miss=1; }
[ -f "$YOLO" ] || { echo "✗ YOLO model missing: $YOLO"; miss=1; }
[ -x "$BIN" ] || { echo "✗ binary missing: $BIN"; echo "    build: cargo build --release -p docparse-cli"; miss=1; }
[ "$miss" = 0 ] || { echo; echo "Resolve the above, then re-run."; exit 1; }

run() {  # $1=eval script  $2=layout-model path  → prints the mean line
  OMNIDOC_LAYOUT_MODEL="$2" "$PY" "$HERE/$1" "$N" 2>/dev/null | grep -E "mean|end-to-end" | tail -1
}

echo "== OmniDocBench layout-backend A/B (N=$N pages${OMNIDOC_DOCTYPE:+, doctype=$OMNIDOC_DOCTYPE}) =="
echo
echo "--- end-to-end TEXT (difflib ratio, higher better) ---"
echo "  YOLO : $(run e2e_text_eval.py "$YOLO")"
echo "  PPV2 : $(run e2e_text_eval.py "$PPV2")"
echo
echo "--- end-to-end TABLE (TEDS-X, higher better) ---"
echo "  YOLO : $(run e2e_table_eval.py "$YOLO")"
echo "  PPV2 : $(run e2e_table_eval.py "$PPV2")"
echo
echo "Note: same pages/scorer; the only variable is the layout backend (region"
echo "detection + reading order). Run with OMNIDOC_DOCTYPE=academic_literature to"
echo "isolate the hard multi-column subset where PPV2's native order should help most."
