#!/usr/bin/env python3
"""对一个跑起来的 fastsearch-server 做 index→search 冒烟测试。

用法：
    FASTSEARCH_URL=http://127.0.0.1:8642 FASTSEARCH_KEY=dev python smoke_test.py
退出码 0 = 通过。
"""

import os
import sys

sys.path.insert(0, os.path.dirname(__file__))
from fastsearch_client import FastsearchClient  # noqa: E402


def main() -> int:
    url = os.environ.get("FASTSEARCH_URL", "http://127.0.0.1:8642")
    key = os.environ.get("FASTSEARCH_KEY", "dev")
    c = FastsearchClient(url, api_key=key)

    chunks = [
        {
            "id": 1,
            "kind": "table",
            "text": "本季度毛利率因成本上升而下降",
            "page": 23,
            "bbox": {"x0": 12, "y0": 15, "x1": 149, "y1": 328},
            "heading_path": ["第3章 财务", "3.2 毛利分析"],
            "section_id": 17,
            "char_len": 14,
        }
    ]
    n = c.index("kb", "report.pdf", chunks)
    assert n == 1, f"expected indexed 1, got {n}"

    hits = c.search("kb", "毛利率", top_k=10)
    assert len(hits) == 1, f"expected 1 hit, got {len(hits)}"
    h = hits[0]
    assert h["citation_id"] == "kb:report.pdf:1", h["citation_id"]
    assert h["page"] == 23, h["page"]
    assert h["bbox"]["x1"] == 149.0, h["bbox"]
    print("OK:", h["citation_id"], "page", h["page"], "bbox", h["bbox"])
    return 0


if __name__ == "__main__":
    sys.exit(main())
