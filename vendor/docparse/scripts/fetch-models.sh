#!/usr/bin/env bash
# Fetch the optional neural models docparse-rs can use.
#
# The core binary needs NO models — born-digital PDFs and every other format
# parse with zero downloads. Models are opt-in, per feature, and live under
# models/ (gitignored). All are Apache-2.0; we redistribute nothing — this
# script pulls from the original HuggingFace repos.
#
#   ./scripts/fetch-models.sh ppocr-v6   # --ocr (default)     (~7 MB, no prep)
#   ./scripts/fetch-models.sh ocr        # --ocr v4 fallback   (~16 MB)
#   ./scripts/fetch-models.sh layout     # --layout (default)  (~75 MB)
#   ./scripts/fetch-models.sh unirec     # --table/formula/transcribe-model (~700 MB)
#   ./scripts/fetch-models.sh ppv2       # --layout-model ppv2 (~210 MB + local prep)
#   ./scripts/fetch-models.sh all        # everything
#
# Needs the HuggingFace CLI: `pip install -U huggingface_hub` (provides `hf`,
# or the older `huggingface-cli`). The `ppv2` tier additionally needs a Python
# venv with `onnx` + `onnxsim` to static-ize the graph for tract (see below).
set -euo pipefail

cd "$(dirname "$0")/.."
MODELS="models"

# --- pick a HuggingFace downloader -------------------------------------------
if command -v hf >/dev/null 2>&1; then
  HF=(hf download)
elif command -v huggingface-cli >/dev/null 2>&1; then
  HF=(huggingface-cli download)
else
  echo "error: need the HuggingFace CLI — pip install -U huggingface_hub" >&2
  exit 1
fi

# dl_file REPO GLOB DEST_PATH
#   Download the single file matching GLOB from REPO (glob survives repo
#   reorganization), then move it to DEST_PATH under the exact name the loader
#   expects (crates/docparse-ocr find_file).
dl_file() {
  local repo="$1" glob="$2" dest="$3"
  local tmp
  tmp="$(mktemp -d)"
  "${HF[@]}" "$repo" --include "$glob" --local-dir "$tmp" >/dev/null
  local found
  found="$(find "$tmp" -type f -name "$(basename "$glob")" | head -1)"
  if [ -z "$found" ]; then
    echo "error: $glob not found in $repo (repo may have moved; check huggingface.co/$repo)" >&2
    rm -rf "$tmp"; exit 1
  fi
  mkdir -p "$(dirname "$dest")"
  mv "$found" "$dest"
  rm -rf "$tmp"
  echo "  ✓ $dest"
}

fetch_ocr() {
  echo "OCR (PP-OCRv4, SWHL/RapidOCR, Apache-2.0) → $MODELS/ppocr/"
  dl_file SWHL/RapidOCR "**/ch_PP-OCRv4_det_infer.onnx"         "$MODELS/ppocr/ch_PP-OCRv4_det_infer.onnx"
  dl_file SWHL/RapidOCR "**/ch_PP-OCRv4_rec_infer.onnx"         "$MODELS/ppocr/ch_PP-OCRv4_rec_infer.onnx"
  dl_file SWHL/RapidOCR "**/ch_ppocr_mobile_v2.0_cls_infer.onnx" "$MODELS/ppocr/ch_ppocr_mobile_v2.0_cls_infer.onnx"
  dl_file SWHL/RapidOCR "**/ppocr_keys_v1.txt"                  "$MODELS/ppocr/ppocr_keys_v1.txt"
}

fetch_layout() {
  echo "Layout (DocLayout-YOLO, DocStructBench, Apache-2.0) → $MODELS/layout/"
  dl_file wybxc/DocLayout-YOLO-DocStructBench-onnx "**/*.onnx" "$MODELS/layout/doclayout_yolo.onnx"
}

fetch_unirec() {
  echo "UniRec-0.1B (topdu/unirec_0_1b_onnx, Apache-2.0) → $MODELS/unirec/"
  # find_file matches by substring+ext, so the repo's own names are fine —
  # pull the whole repo into the dir.
  "${HF[@]}" topdu/unirec_0_1b_onnx --local-dir "$MODELS/unirec" >/dev/null
  echo "  ✓ $MODELS/unirec/"
}

fetch_ppv2() {
  echo "PP-DocLayoutV2 (topdu/PP_DoclayoutV2_onnx, Apache-2.0) → $MODELS/layout-ppv2/"
  dl_file topdu/PP_DoclayoutV2_onnx "**/PP-DoclayoutV2.onnx" "$MODELS/layout-ppv2/PP-DoclayoutV2.onnx"
  echo ""
  echo "  PP-DocLayoutV2's official export has a dynamic graph tract can't shape-infer."
  echo "  Static-ize it once (needs a venv with onnx + onnxsim):"
  echo ""
  echo "      pip install onnx onnxsim"
  echo "      python scripts/spike/ppv2/prepare.py"
  echo ""
  echo "  → produces $MODELS/layout-ppv2/PP-DoclayoutV2_simp.onnx, then run with"
  echo "      --layout --layout-model $MODELS/layout-ppv2/PP-DoclayoutV2_simp.onnx"
}

fetch_ppocr_v6() {
  echo "OCR v6 (PP-OCRv6 tiny, PaddlePaddle, Apache-2.0) → $MODELS/ppocr-v6/"
  # The loader handles PaddleOCR's dynamic-graph export directly (tract with
  # ignore_value_info) and reads the char dict out of the rec yml — so we just
  # drop the raw HuggingFace files in under loader-matchable names. No prep step.
  dl_file PaddlePaddle/PP-OCRv6_tiny_det_onnx "**/inference.onnx" "$MODELS/ppocr-v6/PP-OCRv6_tiny_det.onnx"
  dl_file PaddlePaddle/PP-OCRv6_tiny_rec_onnx "**/inference.onnx" "$MODELS/ppocr-v6/PP-OCRv6_tiny_rec.onnx"
  dl_file PaddlePaddle/PP-OCRv6_tiny_rec_onnx "**/inference.yml"  "$MODELS/ppocr-v6/PP-OCRv6_tiny_rec.yml"
  # v6 ships no new orientation classifier — reuse v4's (optional 0/180 cls).
  dl_file SWHL/RapidOCR "**/ch_ppocr_mobile_v2.0_cls_infer.onnx" "$MODELS/ppocr-v6/ch_ppocr_mobile_v2.0_cls_infer.onnx"
  echo "  → run with  --ocr  (default)  or  --ocr-models $MODELS/ppocr-v6"
}

case "${1:-}" in
  ocr)      fetch_ocr ;;
  ppocr-v6) fetch_ppocr_v6 ;;
  layout)   fetch_layout ;;
  unirec)   fetch_unirec ;;
  ppv2)     fetch_ppv2 ;;
  all)      fetch_ppocr_v6; fetch_ocr; fetch_layout; fetch_unirec; fetch_ppv2 ;;
  *)
    echo "usage: $0 {ocr|ppocr-v6|layout|unirec|ppv2|all}" >&2
    exit 1 ;;
esac

echo "done."
