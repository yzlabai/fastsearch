"""fastsearch Python 客户端（零依赖，仅标准库 urllib）。

封装 fastsearch-server 的 REST API：index / search / similar / assets / delete。
ACL 由服务端按 API Key 强制，客户端无法越权。对接 docparse：把
`docparse -f chunks` 的 chunk 直接喂 index()。

用法::

    from fastsearch_client import FastsearchClient
    c = FastsearchClient("http://127.0.0.1:8642", api_key="dev")
    c.index("kb", "report.pdf", chunks)         # chunks: docparse chunk dict 列表
    hits = c.search("kb", "毛利率", top_k=10, highlight=True)
    for h in hits:
        print(h["citation_id"], h["page"], h["bbox"])   # 引用溯源
    more = c.similar(hits[0]["citation_id"], top_k=5)   # more_like_this
"""

from __future__ import annotations

import json
import time
import urllib.error
import urllib.parse
import urllib.request
from typing import Any, Iterator, Optional


class FastsearchError(Exception):
    """HTTP / 协议错误。`status` 为 HTTP 状态码（传输层错误为 0），`detail` 为服务端错误体。"""

    def __init__(self, message: str, status: int = 0, detail: str = "") -> None:
        super().__init__(message)
        self.status = status
        self.detail = detail


class FastsearchClient:
    """fastsearch server 的 REST 客户端。线程安全、可复用单例。

    `retries`：可重试错误（429/5xx/网络）的自动重试次数（指数退避），默认 0（不重试）。
    """

    def __init__(
        self,
        base_url: str,
        api_key: str,
        timeout: float = 30.0,
        retries: int = 0,
    ) -> None:
        self.base_url = base_url.rstrip("/")
        self.api_key = api_key
        self.timeout = timeout
        self.retries = retries

    # ---- HTTP 内核：超时 + 可选重试 -----------------------------------------

    def _request(
        self,
        method: str,
        path: str,
        body: Optional[dict] = None,
        *,
        timeout: Optional[float] = None,
        raw: bool = False,
    ) -> Any:
        """发一次请求。`raw=True` 返回 `(bytes, content_type)`，否则解析 JSON。"""
        data = json.dumps(body).encode("utf-8") if body is not None else None
        headers = {"X-API-Key": self.api_key}
        if body is not None:
            headers["Content-Type"] = "application/json"
        last_err: Optional[FastsearchError] = None
        for attempt in range(self.retries + 1):
            req = urllib.request.Request(
                self.base_url + path, data=data, method=method, headers=headers
            )
            try:
                with urllib.request.urlopen(req, timeout=timeout or self.timeout) as resp:
                    payload = resp.read()
                    if raw:
                        ct = resp.headers.get("Content-Type", "application/octet-stream")
                        return payload, ct
                    return json.loads(payload.decode("utf-8"))
            except urllib.error.HTTPError as e:
                detail = e.read().decode("utf-8", "replace")
                err = FastsearchError(f"HTTP {e.code}: {detail}", e.code, detail)
                # 与 TS SDK 同分流：仅限流/5xx 可重试，4xx 语义错误立即抛。
                if e.code not in (429,) and e.code < 500:
                    raise err from e
                last_err = err
            except urllib.error.URLError as e:
                last_err = FastsearchError(f"request failed: {e.reason}")
            if attempt < self.retries:
                time.sleep(min(2.0, 0.1 * 2**attempt))
        assert last_err is not None
        raise last_err

    def _post(self, path: str, body: dict) -> Any:
        return self._request("POST", path, body)

    # ---- 检索 ----------------------------------------------------------------

    def _search_body(
        self,
        collection: str,
        query: str,
        *,
        mode: str,
        top_k: int,
        filter: Optional[dict],
        vector: Optional[list[float]],
        highlight: bool,
        fusion: Optional[dict] = None,
        query_image: Optional[list[int]] = None,
        embedder: Optional[str] = None,
        candidates: Optional[int] = None,
        rerank: Optional[bool] = None,
        auto_merge: Optional[bool] = None,
        collapse: Optional[str] = None,
        search_after: Optional[str] = None,
        facets: Optional[list[str]] = None,
        explain: bool = False,
    ) -> dict:
        body: dict = {"query": query, "mode": mode, "top_k": top_k}
        # collection 作用域：注入 Eq(collection) 过滤，与用户 filter `and` 合并（M23）。
        coll_filter = {"eq": ["collection", collection]}
        body["filter"] = {"and": [coll_filter, filter]} if filter is not None else coll_filter
        if fusion is not None:
            body["fusion"] = fusion
        if vector is not None:
            body["vector"] = vector
        if query_image is not None:
            body["query_image"] = query_image
        if embedder is not None:
            body["embedder"] = embedder
        if candidates is not None:
            body["candidates"] = candidates
        if rerank is not None:
            body["rerank"] = rerank
        if auto_merge is not None:
            body["auto_merge"] = auto_merge
        if collapse is not None:
            body["collapse"] = collapse
        if search_after is not None:
            body["search_after"] = search_after
        if highlight:
            body["highlight"] = True
        if facets is not None:
            body["facets"] = facets
        if explain:
            body["explain"] = True
        return body

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
        fusion: Optional[dict] = None,
        query_image: Optional[list[int]] = None,
        embedder: Optional[str] = None,
        candidates: Optional[int] = None,
        rerank: Optional[bool] = None,
        auto_merge: Optional[bool] = None,
        collapse: Optional[str] = None,
        search_after: Optional[str] = None,
        facets: Optional[list[str]] = None,
        explain: bool = False,
    ) -> list[dict]:
        """检索。返回命中列表，每条含 citation_id/score/page/bbox/heading_path/...

        `highlight=True` 让服务端回高亮片段（进入命中的 `highlight` 字段，供 RAG 取片段）。
        `collection` 作用域**强制注入**为过滤子句（与 CLI 一致），多集合 server 上只返回本集合命中；
        与用户 `filter` 用 `and` 合并。ACL 由服务端按 API Key 强制，与此无关。
        需要分面计数时用 :meth:`search_with_facets`。
        """
        body = self._search_body(
            collection,
            query,
            mode=mode,
            top_k=top_k,
            filter=filter,
            vector=vector,
            highlight=highlight,
            fusion=fusion,
            query_image=query_image,
            embedder=embedder,
            candidates=candidates,
            rerank=rerank,
            auto_merge=auto_merge,
            collapse=collapse,
            search_after=search_after,
            facets=facets,
            explain=explain,
        )
        out = self._post("/v1/search", body)
        return out.get("hits", [])

    def search_with_facets(self, collection: str, query: str, **opts: Any) -> dict:
        """同 :meth:`search`，但返回完整响应 `{"hits": [...], "facets": {...}}`。

        `opts` 与 search 的关键字参数一致；配合 `facets=["doc_id", ...]` 取分面计数。
        """
        body = self._search_body(
            collection,
            query,
            mode=opts.pop("mode", "hybrid"),
            top_k=opts.pop("top_k", 20),
            filter=opts.pop("filter", None),
            vector=opts.pop("vector", None),
            highlight=opts.pop("highlight", False),
            **opts,
        )
        out = self._post("/v1/search", body)
        return {"hits": out.get("hits", []), "facets": out.get("facets", {})}

    def paginate(
        self,
        collection: str,
        query: str,
        *,
        max_pages: Optional[int] = None,
        **opts: Any,
    ) -> Iterator[list[dict]]:
        """逐页遍历深分页：自动用上一页末条命中的 cursor 续取，直到不足一页或达 `max_pages`。

        适合 agent 做"全量扫读/汇总"。`opts` 与 :meth:`search` 的关键字参数一致。
        """
        page_size = opts.get("top_k", 20)
        cursor = opts.pop("search_after", None)
        page = 0
        while max_pages is None or page < max_pages:
            hits = self.search(collection, query, search_after=cursor, **opts)
            if not hits:
                return
            yield hits
            if len(hits) < page_size:
                return
            # 防死循环：末条无游标 / 游标未推进 → 停（否则会从第一页重头取）。
            nxt = hits[-1].get("cursor")
            if nxt is None or nxt == cursor:
                return
            cursor = nxt
            page += 1

    def similar(self, citation_id: str, *, top_k: int = 10) -> list[dict]:
        """more_like_this：按种子 `citation_id` 反查相似命中。"""
        out = self._post("/v1/similar", {"citation_id": citation_id, "top_k": top_k})
        return out.get("hits", [])

    # ---- 资产 / 引用解析 ------------------------------------------------------

    def resolve_assets(self, citation_ids: list[str]) -> list[dict]:
        """批量把 citation_id 解析成可直接用的短时 URL 或跳原文 JSON。

        ACL 强制：越权/不存在的 id 直接省略（不暴露存在性）。
        """
        out = self._post("/v1/assets/resolve", {"ids": citation_ids})
        return out.get("assets", [])

    def fetch_asset_bytes(self, citation_id: str) -> Optional[tuple[bytes, str]]:
        """取单个引用的 **inline 媒资字节**，返回 `(bytes, content_type)`。

        返回 `None` 表示：不可见/不存在（404），或该引用是 DocRender（跳原文 JSON）
        而非 inline 字节——那类请用 :meth:`resolve_assets`（拿 page+bbox 或短时 URL）。
        """
        path = "/v1/asset/" + urllib.parse.quote(citation_id, safe="")
        try:
            payload, ct = self._request("GET", path, raw=True)
        except FastsearchError as e:
            if e.status == 404:
                return None
            raise
        # DocRender 命中回的是 JSON（跳原文，非 inline 字节）——非本方法语义。
        if "application/json" in ct:
            return None
        return payload, ct

    # ---- 写入 / 删除 ----------------------------------------------------------

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

    def delete_doc(self, collection: str, doc_id: str) -> dict:
        """删除一个 doc（真源 PG + 派生索引 + 关联对象）。

        返回服务端响应 `{"deleted": true, "pg_deleted": n, ...}`。
        ACL 强制：不可见/不存在抛 404 FastsearchError（不暴露存在性）。
        """
        path = (
            "/v1/docs/"
            + urllib.parse.quote(collection, safe="")
            + "/"
            + urllib.parse.quote(doc_id, safe="/")  # doc_id 可含 `/`（server 通配段）
        )
        return self._request("DELETE", path)

    # ---- 健康 / 契约 ----------------------------------------------------------

    def health(self, *, timeout: float = 5.0) -> bool:
        """存活探针（无需鉴权）。server 在线返回 True，任何失败返回 False。"""
        try:
            self._request("GET", "/healthz", raw=True, timeout=timeout)
            return True
        except Exception:
            return False

    def openapi(self) -> dict:
        """取 OpenAPI 3.0 契约（手写、随 API 演进维护）。"""
        return self._request("GET", "/openapi.json")


__all__ = ["FastsearchClient", "FastsearchError"]
