"""fastsearch Python 客户端（零依赖，仅标准库 urllib）。

封装 fastsearch-server 的 REST API：index / search。ACL 由服务端按 API Key 强制，
客户端无法越权。对接 docparse：把 `docparse -f chunks` 的 chunk 直接喂 index()。

用法::

    from fastsearch_client import FastsearchClient
    c = FastsearchClient("http://127.0.0.1:8642", api_key="dev")
    c.index("kb", "report.pdf", chunks)         # chunks: docparse chunk dict 列表
    hits = c.search("kb", "毛利率", top_k=10)
    for h in hits:
        print(h["citation_id"], h["page"], h["bbox"])   # 引用溯源
"""

from __future__ import annotations

import json
import urllib.error
import urllib.request
from typing import Any, Optional


class FastsearchError(Exception):
    """HTTP / 协议错误。"""


class FastsearchClient:
    def __init__(self, base_url: str, api_key: str, timeout: float = 30.0) -> None:
        self.base_url = base_url.rstrip("/")
        self.api_key = api_key
        self.timeout = timeout

    def _post(self, path: str, body: dict) -> Any:
        data = json.dumps(body).encode("utf-8")
        req = urllib.request.Request(
            self.base_url + path,
            data=data,
            method="POST",
            headers={
                "Content-Type": "application/json",
                "X-API-Key": self.api_key,
            },
        )
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as resp:
                return json.loads(resp.read().decode("utf-8"))
        except urllib.error.HTTPError as e:
            detail = e.read().decode("utf-8", "replace")
            raise FastsearchError(f"HTTP {e.code}: {detail}") from e
        except urllib.error.URLError as e:
            raise FastsearchError(f"request failed: {e.reason}") from e

    def index(self, collection: str, doc_id: str, chunks: list[dict]) -> int:
        """灌入一个 doc 的 chunks（doc 级替换）。返回灌入条数。

        chunks 为 docparse chunk dict（含 id/kind/text/page/bbox/...）；本方法补上
        doc_id 并映射 id→chunk_id，acl 默认 ['public']。
        """
        mapped = []
        for ch in chunks:
            c = dict(ch)
            c.setdefault("doc_id", doc_id)
            if "chunk_id" not in c and "id" in c:
                c["chunk_id"] = c.pop("id")
            c.setdefault("acl", ["public"])
            mapped.append(c)
        out = self._post(
            "/v1/index",
            {"collection": collection, "doc_id": doc_id, "chunks": mapped},
        )
        return int(out.get("indexed", 0))

    def search(
        self,
        collection: str,
        query: str,
        *,
        mode: str = "hybrid",
        top_k: int = 20,
        filter: Optional[dict] = None,
        vector: Optional[list[float]] = None,
        highlight: bool = False,
    ) -> list[dict]:
        """检索。返回命中列表，每条含 citation_id/score/page/bbox/heading_path/...

        `highlight=True` 让服务端回高亮片段（进入命中的 `highlight` 字段，供 RAG 取片段）。
        `collection` 作用域**强制注入**为过滤子句（与 CLI 一致），多集合 server 上只返回本集合命中；
        与用户 `filter` 用 `and` 合并。ACL 由服务端按 API Key 强制，与此无关。
        """
        body: dict = {"query": query, "mode": mode, "top_k": top_k}
        # collection 作用域：注入 Eq(collection) 过滤，与用户 filter `and` 合并（M23）。
        coll_filter = {"eq": ["collection", collection]}
        body["filter"] = {"and": [coll_filter, filter]} if filter is not None else coll_filter
        if vector is not None:
            body["vector"] = vector
        if highlight:
            body["highlight"] = True
        out = self._post("/v1/search", body)
        return out.get("hits", [])


__all__ = ["FastsearchClient", "FastsearchError"]
