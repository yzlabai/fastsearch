"""LangChain / LlamaIndex 适配器（薄封装，依赖可选）。

把 fastsearch 的检索命中映射成两个生态的标准检索对象，便于直接接入 RAG 链：

- LangChain：`FastsearchRetriever`（鸭子兼容 `get_relevant_documents`/`invoke`），命中→`Document`
- LlamaIndex：`hits_to_llama_nodes`，命中→`NodeWithScore`

**依赖可选**：未装 langchain / llama-index 时回退到本地等价 `Document`/节点对象（同样有
`page_content`/`metadata`），故本模块零硬依赖、可单测。

**page_content 取舍（诚实说明）**：`/v1/search` 出于载荷精简**不回整段正文**，仅在
`highlight=True` 时回高亮片段。故 `page_content` = 高亮片段（无则空串）；**完整正文/溯源
靠 metadata 里的 `citation_id`**（答案层 `resolve_citation` 解析 page/bbox 深链）。
检索时传 `highlight=True` 可让片段进入 `page_content`。
"""

from __future__ import annotations

from typing import Any, Optional

# ---- LangChain Document（可选依赖，回退本地等价物）----
try:  # pragma: no cover - 取决于环境是否装 langchain
    from langchain_core.documents import Document as _LCDocument

    _HAS_LANGCHAIN = True
except Exception:  # noqa: BLE001 - 任何导入失败都回退
    _LCDocument = None
    _HAS_LANGCHAIN = False


class _FallbackDocument:
    """langchain 未安装时的等价物：与 `langchain_core.documents.Document` 同形。"""

    def __init__(self, page_content: str, metadata: dict) -> None:
        self.page_content = page_content
        self.metadata = metadata

    def __repr__(self) -> str:
        cid = self.metadata.get("citation_id")
        return f"Document(citation_id={cid!r}, len={len(self.page_content)})"

    def __eq__(self, other: object) -> bool:
        return (
            getattr(other, "page_content", None) == self.page_content
            and getattr(other, "metadata", None) == self.metadata
        )


def _make_document(page_content: str, metadata: dict) -> Any:
    if _HAS_LANGCHAIN:
        return _LCDocument(page_content=page_content, metadata=metadata)
    return _FallbackDocument(page_content=page_content, metadata=metadata)


# 进 metadata 的命中字段（溯源/打分；page_content 单独取自 highlight）。
_METADATA_KEYS = (
    "citation_id",
    "score",
    "bm25",
    "vector",
    "rerank",
    "doc_id",
    "chunk_id",
    "page",
    "bbox",
    "heading_path",
    "section_id",
    "merged_chunk_ids",
    "time",
    "media",
)


def hit_to_document(hit: dict) -> Any:
    """单条命中 → LangChain `Document`（或回退等价物）。

    `page_content` 取高亮片段（`highlight`，无则空串）；其余字段入 `metadata`。
    """
    page_content = hit.get("highlight") or ""
    metadata = {k: hit[k] for k in _METADATA_KEYS if k in hit}
    return _make_document(page_content, metadata)


def hits_to_documents(hits: list[dict]) -> list[Any]:
    """命中列表 → `Document` 列表。"""
    return [hit_to_document(h) for h in hits]


class FastsearchRetriever:
    """LangChain 风格检索器（鸭子兼容：`get_relevant_documents` + `invoke`）。

    包一个 `FastsearchClient` + 固定 `collection` 与检索参数；`invoke(query)` 返回
    `Document` 列表，可直接放进 LCEL 管道（`retriever | prompt | llm`）。ACL 由服务端按
    API Key 强制，检索器无法越权。

    用法::

        from fastsearch_client import FastsearchClient
        from fastsearch_client.integrations import FastsearchRetriever
        r = FastsearchRetriever(FastsearchClient(url, api_key="dev"), "kb",
                                mode="hybrid", top_k=8, highlight=True)
        docs = r.invoke("毛利率")
    """

    def __init__(
        self,
        client: Any,
        collection: str,
        *,
        mode: str = "hybrid",
        top_k: int = 10,
        filter: Optional[dict] = None,
        highlight: bool = True,
        **search_kwargs: Any,
    ) -> None:
        self.client = client
        self.collection = collection
        self.mode = mode
        self.top_k = top_k
        self.filter = filter
        self.highlight = highlight
        self.search_kwargs = search_kwargs

    def get_relevant_documents(self, query: str) -> list[Any]:
        kw = dict(self.search_kwargs)
        # 透传 highlight（服务端支持时让片段进 page_content）。
        kw.setdefault("highlight", self.highlight)
        hits = self.client.search(
            self.collection,
            query,
            mode=self.mode,
            top_k=self.top_k,
            filter=self.filter,
            **kw,
        )
        return hits_to_documents(hits)

    # LangChain 0.1+ Runnable 别名。
    def invoke(self, query: str, config: Any = None) -> list[Any]:
        return self.get_relevant_documents(query)


# ---- LlamaIndex 节点（可选依赖，回退本地等价物）----
try:  # pragma: no cover
    from llama_index.core.schema import NodeWithScore as _LINodeWithScore
    from llama_index.core.schema import TextNode as _LITextNode

    _HAS_LLAMA = True
except Exception:  # noqa: BLE001
    _LINodeWithScore = None
    _LITextNode = None
    _HAS_LLAMA = False


class _FallbackNodeWithScore:
    """llama-index 未安装时的等价物：暴露 `.text`/`.metadata`/`.score`。"""

    def __init__(self, text: str, metadata: dict, score: Optional[float]) -> None:
        self.text = text
        self.metadata = metadata
        self.score = score

    def __repr__(self) -> str:
        return f"NodeWithScore(citation_id={self.metadata.get('citation_id')!r}, score={self.score})"

    def __eq__(self, other: object) -> bool:
        return (
            getattr(other, "text", None) == self.text
            and getattr(other, "metadata", None) == self.metadata
            and getattr(other, "score", None) == self.score
        )


def hit_to_llama_node(hit: dict) -> Any:
    """单条命中 → LlamaIndex `NodeWithScore`（或回退等价物）。

    `id_` 取 `citation_id`，`score` 取融合分；正文取高亮片段（同 `page_content` 取舍）。
    """
    text = hit.get("highlight") or ""
    metadata = {k: hit[k] for k in _METADATA_KEYS if k in hit}
    score = hit.get("score")
    cid = hit.get("citation_id")
    if _HAS_LLAMA:
        node = _LITextNode(text=text, id_=cid, metadata=metadata)
        return _LINodeWithScore(node=node, score=score)
    return _FallbackNodeWithScore(text=text, metadata=metadata, score=score)


def hits_to_llama_nodes(hits: list[dict]) -> list[Any]:
    """命中列表 → `NodeWithScore` 列表。"""
    return [hit_to_llama_node(h) for h in hits]


__all__ = [
    "FastsearchRetriever",
    "hit_to_document",
    "hits_to_documents",
    "hit_to_llama_node",
    "hits_to_llama_nodes",
]
