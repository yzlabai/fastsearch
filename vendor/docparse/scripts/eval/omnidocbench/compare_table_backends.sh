#!/usr/bin/env bash
# OmniDocBench A/B: embedded UniRec vs a served VLM, on the SAME pages with the
# SAME scorer. Sibling of compare_layout_backends.sh; the only variable here is
# the table recognition backend.
#
# Why a script instead of two manual runs: the acceptance criterion is a DELTA
# against a baseline re-measured in the same session, not a fixed pass mark.
# Historic numbers for "the same" set differ by config (docs/status.md records
# the academic-table figure as both 0.52 and 0.670 under different setups), so a
# number quoted from a doc is not a baseline. Running both arms here makes the
# comparison structurally honest instead of dependent on someone remembering.
#
# Usage:   scripts/eval/omnidocbench/compare_table_backends.sh [N]
#   N                  pages per eval (default 30)
#   OMNIDOC_VLM_URL    OpenAI-compatible endpoint (required)
#   OMNIDOC_VLM_MODEL  model name as the service knows it (required)
#   OMNIDOC_VLM_KEY    bearer token (optional)
#   OMNIDOC_DOCTYPE    data_source filter, e.g. academic_literature (optional)
#   OMNIDOC_LAYOUT_MODEL  layout backend (optional; keep it FIXED across arms)
#   PY                 python interpreter (default python3; needs Pillow)
#
# Prereqs (checked below):
#   - tmp/omnidocbench/OmniDocBench.json   (dataset; images auto-download/cache)
#   - models/unirec/                       (scripts/fetch-models.sh unirec)
#   - models/layout/doclayout_yolo.onnx    (scripts/fetch-models.sh layout)
#   - target/release/docparse              (cargo build --release -p docparse-cli)
#   - a reachable VLM endpoint             (see the run book in the fastsearch
#     repo: docs/plans/2026-07-27-OvisOCR2接入需求分析与功能设计.md §7.5)
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"
cd "$ROOT"
N="${1:-30}"
PY="${PY:-python3}"
BIN="target/release/docparse"
YOLO="${OMNIDOC_LAYOUT_MODEL:-models/layout/doclayout_yolo.onnx}"

miss=0
[ -f tmp/omnidocbench/OmniDocBench.json ] || { echo "✗ dataset missing: tmp/omnidocbench/OmniDocBench.json"; echo "    get OmniDocBench.json from https://github.com/opendatalab/OmniDocBench and place it there (images auto-cache)."; miss=1; }
[ -d models/unirec ] || { echo "✗ UniRec missing: models/unirec/"; echo "    fetch: ./scripts/fetch-models.sh unirec   (~700MB)"; miss=1; }
[ -f "$YOLO" ] || { echo "✗ layout model missing: $YOLO"; echo "    fetch: ./scripts/fetch-models.sh layout   (~75MB)"; miss=1; }
[ -x "$BIN" ] || { echo "✗ binary missing: $BIN"; echo "    build: cargo build --release -p docparse-cli"; miss=1; }
[ -n "${OMNIDOC_VLM_URL:-}" ] || { echo "✗ OMNIDOC_VLM_URL not set (OpenAI-compatible endpoint)"; miss=1; }
[ -n "${OMNIDOC_VLM_MODEL:-}" ] || { echo "✗ OMNIDOC_VLM_MODEL not set"; miss=1; }
[ "$miss" = 0 ] || { echo; echo "Resolve the above, then re-run."; exit 1; }

if [ -n "${OMNIDOC_VLM_URL:-}" ]; then
  curl -sf --max-time 10 -o /dev/null "$OMNIDOC_VLM_URL/v1/models" \
    || echo "! warning: $OMNIDOC_VLM_URL/v1/models did not answer — the VLM arm will score 0 if the service is down"
fi

run() {  # $1 = OMNIDOC_TABLE_BACKEND value → prints the mean line
  OMNIDOC_TABLE_BACKEND="$1" OMNIDOC_LAYOUT_MODEL="$YOLO" \
    "$PY" "$HERE/e2e_table_eval.py" "$N" 2>/dev/null | grep -E "mean|end-to-end" | tail -1
}

echo "== OmniDocBench table-backend A/B (N=$N pages${OMNIDOC_DOCTYPE:+, doctype=$OMNIDOC_DOCTYPE}) =="
echo "   layout backend held fixed at: $YOLO"
echo "   VLM: $OMNIDOC_VLM_MODEL @ $OMNIDOC_VLM_URL"
echo
echo "--- end-to-end TABLE (TEDS-X, higher better) ---"
echo "  UniRec (embedded) : $(run unirec)"
echo "  VLM    (served)   : $(run vlm)"
echo
echo "Acceptance (gate 2): VLM must beat the UniRec line measured in THIS run by"
echo "+0.10 TEDS-X to count as breaking the fixed-resolution ceiling; under +0.03"
echo "is no progress. Do not compare against numbers quoted from docs — they were"
echo "measured under a different configuration."
echo
echo "Isolate the recorded weak spot with:"
echo "  OMNIDOC_DOCTYPE=academic_literature $0 $N"
