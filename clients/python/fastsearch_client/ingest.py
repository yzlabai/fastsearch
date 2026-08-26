"""文档摄取适配层：把**别的工具产出的东西**变成 fastsearch 能收的 chunk。

为什么需要它：fastsearch 的契约是"调用方拥有解析与分块，引擎拥有检索"（见仓内
docs/governance 的职责边界 ADR）。既然把解析推给调用方，就得把**契约**交出去——
否则调用方只能靠读 Rust 适配器源码来拼 JSON。本模块就是那份契约的可执行版本。

    from fastsearch_client import FastsearchClient, chunks_from_docparse, chunk_text

    # ① 用任意解析器（如 docparse 的 REST 服务）拿到 chunks，再适配：
    chunks = chunks_from_docparse(docparse_chunks, doc_id="report.pdf")
    client.index("kb", "report.pdf", chunks)
    # ② 纯文本 / markdown：直接切
    client.index("kb", "notes.md", chunk_text(text, doc_id="notes.md"))
"""

from __future__ import annotations

import base64
import re
from typing import Any, Dict, List, Optional, Sequence

_ZERO_BBOX = {"x0": 0.0, "y0": 0.0, "x1": 0.0, "y1": 0.0}
_HEADING = re.compile(r"^(#{1,6})\s+(.*\S)\s*$")


def _b64_to_bytes(b64: str) -> List[int]:
    """base64 → **字节数组**。不是可选步骤，见 `chunks_from_docparse` 的说明。"""
    raw = b64.rsplit(",", 1)[-1].strip()  # 允许 data URL 形态
    return list(base64.b64decode(raw))


def chunks_from_docparse(
    chunks: Sequence[Dict[str, Any]],
    *,
    doc_id: str,
    acl: Optional[Sequence[str]] = None,
    keep_structure: bool = True,
) -> List[Dict[str, Any]]:
    """docparse chunk → fastsearch chunk。

    两侧 schema 刻意同构，**只有三处要动**：

    1. ``id`` → ``chunk_id``（唯一的重命名）；
    2. ``image`` → ``media`` 三态 —— ``data_base64`` ⇒ ``{"kind":"inline"}`` + ``media_bytes``；
       ``file`` ⇒ ``{"kind":"object","uri":…}``；两者皆无 ⇒ ``{"kind":"doc_region",…}``；
    3. 注入 ``doc_id``（docparse 的 chunk 不带它）。

    .. warning::
       **``media_bytes`` 是字节数组，不是 base64 字符串。** 这是实测过的线缆契约：
       传 base64 字符串会被服务端以 ``invalid type: string …, expected a sequence`` 拒绝。
       docparse 给的是 base64，所以这一步解码**必须做**——手写 JSON 最容易踩的就是这个坑。
    """
    out: List[Dict[str, Any]] = []
    for d in chunks:
        text = d.get("text", "")
        c: Dict[str, Any] = {
            "doc_id": doc_id,
            "chunk_id": d["id"],
            "kind": d.get("kind", "paragraph"),
            "text": text,
            "page": d.get("page", 1),
            "bbox": d.get("bbox", _ZERO_BBOX),
            "char_len": d.get("char_len", len(text)),
        }
        if keep_structure:
            if d.get("heading_path") is not None:
                c["heading_path"] = d["heading_path"]
            if d.get("section_id") is not None:
                c["section_id"] = d["section_id"]
        if acl is not None:
            c["acl"] = list(acl)
        img = d.get("image")
        if img:
            media: Dict[str, Any] = {
                "media_type": img.get("media_type"),
                "region": c["bbox"],
                "caption_source": img.get("caption_source"),
            }
            if img.get("data_base64"):
                media["asset"] = {"kind": "inline"}
                c["media_bytes"] = _b64_to_bytes(img["data_base64"])
            elif img.get("file"):
                media["asset"] = {"kind": "object", "uri": img["file"]}
            else:
                # 没有字节也没有对象：只能指回原文位置（读者仍可跳到该页那一块）。
                media["asset"] = {
                    "kind": "doc_region",
                    "page": c["page"],
                    "bbox": c["bbox"],
                }
            c["media"] = media
        out.append(c)
    return out


def chunk_text(
    text: str,
    *,
    doc_id: str,
    target_chars: int = 900,
    overlap: int = 0,
    acl: Optional[Sequence[str]] = None,
) -> List[Dict[str, Any]]:
    """纯文本 / markdown 切块：空行分段聚合到目标长度；markdown 标题维护 ``heading_path``。

    .. warning::
       **坐标是占位的**：``page=1``、``bbox`` 全 0 —— 纯文本没有版面。要真正的 page/bbox
       （从而让 ``resolve_citation`` 能在原文里高亮），得用真正的解析器，见
       :func:`chunks_from_docparse`。不说清这点，调用方会以为自己拿到了可溯源的坐标。
    """
    chunks: List[Dict[str, Any]] = []
    path: List[tuple] = []
    buf: List[str] = []

    def push(kind: str, body: str) -> None:
        if not body:
            return
        c: Dict[str, Any] = {
            "doc_id": doc_id,
            "chunk_id": len(chunks),
            "kind": kind,
            "text": body,
            "page": 1,
            "bbox": dict(_ZERO_BBOX),
            "heading_path": [t for _, t in path],
            "char_len": len(body),
        }
        if acl is not None:
            c["acl"] = list(acl)
        chunks.append(c)

    def flush() -> None:
        nonlocal buf
        body = "\n".join(buf).strip()
        buf = []
        if not body:
            return
        push("paragraph", body)
        if overlap > 0:
            tail = body[-overlap:]
            if tail:
                buf.append(tail)

    for line in text.splitlines():
        m = _HEADING.match(line)
        if m:
            flush()
            level, title = len(m.group(1)), m.group(2)
            while path and path[-1][0] >= level:
                path.pop()
            path.append((level, title))
            push("heading", title)
            continue
        if not line.strip():
            if len("\n".join(buf).strip()) >= target_chars:
                flush()
            elif buf:
                buf.append("")
            continue
        buf.append(line)
        if len("\n".join(buf)) >= target_chars:
            flush()
    flush()
    return [c for c in chunks if c["text"].strip()]
