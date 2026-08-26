"""摄取适配层单测（零网络）。跑：python3 -m pytest test_ingest.py -q（或 python3 test_ingest.py）。"""

from fastsearch_client import chunk_text, chunks_from_docparse

BBOX = {"x0": 0.0, "y0": 0.0, "x1": 10.0, "y1": 10.0}


def test_id_renamed_and_doc_id_injected():
    out = chunks_from_docparse(
        [{"id": 7, "kind": "paragraph", "text": "毛利率提升", "page": 3,
          "bbox": BBOX, "heading_path": ["财务"], "section_id": 2}],
        doc_id="r.pdf",
    )
    assert out[0]["chunk_id"] == 7, "id → chunk_id 是唯一的重命名"
    assert out[0]["doc_id"] == "r.pdf", "doc_id 由适配器注入（docparse 不带）"
    assert out[0]["heading_path"] == ["财务"]
    assert out[0]["char_len"] == len("毛利率提升")


def test_image_tri_state_and_base64_becomes_byte_array():
    base = {"kind": "image", "text": "图1", "page": 2, "bbox": BBOX}
    inline, obj, region = chunks_from_docparse(
        [
            {**base, "id": 0, "image": {"data_base64": "iVBORw==", "media_type": "image/png"}},
            {**base, "id": 1, "image": {"file": "s3://b/k.png"}},
            {**base, "id": 2, "image": {"caption": "只有图注"}},
        ],
        doc_id="d",
    )
    assert inline["media"]["asset"]["kind"] == "inline"
    # **字节数组，不是 base64 字符串**：服务端只收 sequence，传字符串会 400。
    assert isinstance(inline["media_bytes"], list)
    assert inline["media_bytes"][:4] == [0x89, 0x50, 0x4E, 0x47]
    assert obj["media"]["asset"] == {"kind": "object", "uri": "s3://b/k.png"}
    assert "media_bytes" not in obj
    assert region["media"]["asset"]["kind"] == "doc_region"
    assert region["media"]["asset"]["page"] == 2


def test_chunk_text_headings_and_ids():
    out = chunk_text("# 年报\n\n第一段。\n\n## 财务\n\n毛利率 42%。", doc_id="n.md", target_chars=10)
    assert [c["text"] for c in out if c["kind"] == "heading"] == ["年报", "财务"]
    assert out[-1]["heading_path"] == ["年报", "财务"], "二级标题下的正文带完整面包屑"
    # chunk_id 连续、从 0 起（GlobalId 的一部分，必须稳定）。
    assert [c["chunk_id"] for c in out] == list(range(len(out)))


def test_chunk_text_coordinates_are_placeholders():
    """纯文本没有版面——坐标是占位的，不能假装可高亮。"""
    out = chunk_text("只有一段。", doc_id="n.md")
    assert out[0]["page"] == 1
    assert out[0]["bbox"] == {"x0": 0.0, "y0": 0.0, "x1": 0.0, "y1": 0.0}


def test_chunk_text_overlap():
    out = chunk_text("甲" * 50 + "\n\n" + "乙" * 50, doc_id="n.md", target_chars=40, overlap=5)
    assert len(out) >= 2
    assert out[1]["text"].startswith("甲"), "第二块以上一块的尾部起头"


if __name__ == "__main__":
    for name, fn in sorted(globals().items()):
        if name.startswith("test_"):
            fn()
            print(f"ok  {name}")
    print("all passed")
