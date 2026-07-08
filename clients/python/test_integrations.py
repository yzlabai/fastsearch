"""integrations 适配器自测（零依赖、无网络、无 langchain/llama-index）。

跑：`python3 test_integrations.py`（退出码 0=通过）。用假 client 喂固定命中，
校验 Document/Node 的 page_content/metadata/score 映射正确，且 ACL 参数透传不被改写。
"""

import sys

from fastsearch_client.integrations import (
    FastsearchRetriever,
    hit_to_document,
    hit_to_llama_node,
    hits_to_documents,
    hits_to_llama_nodes,
)

SAMPLE_HITS = [
    {
        "citation_id": "kb:rep.pdf:3",
        "score": 1.42,
        "bm25": 0.9,
        "vector": 0.7,
        "doc_id": "rep.pdf",
        "chunk_id": 3,
        "page": 7,
        "bbox": {"x0": 1.0, "y0": 2.0, "x1": 3.0, "y1": 4.0},
        "heading_path": ["第3章", "财务"],
        "section_id": 17,
        "highlight": "本季度<b>毛利率</b>提升至 42%",
        "merged_chunk_ids": [4, 5],
        "time": None,
        "media": None,
    },
    {
        "citation_id": "kb:rep.pdf:9",
        "score": 0.88,
        "doc_id": "rep.pdf",
        "chunk_id": 9,
        "page": 12,
        "bbox": {"x0": 0.0, "y0": 0.0, "x1": 1.0, "y1": 1.0},
        "heading_path": [],
        "section_id": 20,
        # 无 highlight → page_content 应为空串
    },
]


def test_hit_to_document():
    doc = hit_to_document(SAMPLE_HITS[0])
    assert doc.page_content == "本季度<b>毛利率</b>提升至 42%", doc.page_content
    assert doc.metadata["citation_id"] == "kb:rep.pdf:3"
    assert doc.metadata["page"] == 7
    assert doc.metadata["bbox"]["x1"] == 3.0
    assert doc.metadata["score"] == 1.42
    assert doc.metadata["heading_path"] == ["第3章", "财务"]
    # 无正文的命中 → 空 page_content（完整正文靠 citation_id 解析）
    doc2 = hit_to_document(SAMPLE_HITS[1])
    assert doc2.page_content == ""
    assert doc2.metadata["citation_id"] == "kb:rep.pdf:9"
    # 缺失字段不应混入 metadata
    assert "highlight" not in doc2.metadata


def test_hits_to_documents():
    docs = hits_to_documents(SAMPLE_HITS)
    assert len(docs) == 2
    assert [d.metadata["chunk_id"] for d in docs] == [3, 9]


def test_hit_to_llama_node():
    node = hit_to_llama_node(SAMPLE_HITS[0])
    assert node.text == "本季度<b>毛利率</b>提升至 42%"
    assert node.score == 1.42
    assert node.metadata["citation_id"] == "kb:rep.pdf:3"
    nodes = hits_to_llama_nodes(SAMPLE_HITS)
    assert len(nodes) == 2
    assert nodes[1].score == 0.88


class _FakeClient:
    """记录 search 调用参数的假 client。

    **镜像真实 `FastsearchClient.search` 的签名（无 `**kw` 兜底）**：若 retriever 透传了真实
    client 不接受的参数会立即 TypeError，测试即能抓住——这正是 H6 的教训（旧 stub 用 `**kw`
    吞掉 `highlight`，遮蔽了真实 client 缺该参数导致 `retriever.invoke()` 必崩的 bug）。
    """

    def __init__(self, hits):
        self.hits = hits
        self.calls = []

    def search(
        self, collection, query, *, mode="hybrid", top_k=20, filter=None, vector=None, highlight=False
    ):
        self.calls.append(
            {
                "collection": collection,
                "query": query,
                "mode": mode,
                "top_k": top_k,
                "filter": filter,
                "vector": vector,
                "highlight": highlight,
            }
        )
        return self.hits


def test_retriever_invoke_and_param_passthrough():
    fake = _FakeClient(SAMPLE_HITS)
    r = FastsearchRetriever(
        fake, "kb", mode="hybrid", top_k=8, filter={"kind": "table"}, highlight=True
    )
    docs = r.invoke("毛利率")
    assert len(docs) == 2
    # H6 回归：真实 client 签名下 invoke 不再 TypeError，且 highlight 命中进 page_content
    assert docs[0].page_content == "本季度<b>毛利率</b>提升至 42%"
    # get_relevant_documents 是 invoke 的别名，结果一致
    assert r.get_relevant_documents("毛利率")[0].metadata["citation_id"] == "kb:rep.pdf:3"
    # 透传：collection/mode/top_k/filter/highlight 原样进入 client.search
    call = fake.calls[0]
    assert call["collection"] == "kb"
    assert call["mode"] == "hybrid"
    assert call["top_k"] == 8
    assert call["filter"] == {"kind": "table"}
    assert call["highlight"] is True


def test_retriever_kwargs_bind_real_client_signature():
    """H6 直接回归（不依赖 live server）：retriever 透传给 client.search 的参数必须能绑定
    **真实** FastsearchClient.search 的签名。旧代码 highlight 无法绑定 → `sig.bind(...)`
    抛 TypeError，即 `retriever.invoke()` 用真实 client 必崩。"""
    import inspect

    from fastsearch_client import FastsearchClient

    sig = inspect.signature(FastsearchClient.search)
    # 模拟 get_relevant_documents 的调用实参（self, collection, query, **kw）
    sig.bind(
        None,  # self
        "kb",  # collection
        "毛利率",  # query
        mode="hybrid",
        top_k=8,
        filter=None,
        highlight=True,
    )  # 不抛 TypeError 即通过


def test_search_injects_collection_filter():
    """M23：真实 client.search 把 collection 作用域注入为 Eq 过滤，与用户 filter `and` 合并。"""
    from fastsearch_client import FastsearchClient

    c = FastsearchClient("http://x", api_key="dev")
    captured = []

    def fake_post(path, body):
        captured.append(body)
        return {"hits": []}

    c._post = fake_post
    # 无用户 filter → 单 Eq(collection)
    c.search("kb", "q")
    assert captured[-1]["filter"] == {"eq": ["collection", "kb"]}
    # 有用户 filter → and 合并
    c.search("kb", "q", filter={"eq": ["kind", "table"]})
    assert captured[-1]["filter"] == {
        "and": [{"eq": ["collection", "kb"]}, {"eq": ["kind", "table"]}]
    }


# ---- M24：SDK 补齐（与 TS client 对齐的方法面，零网络 stub） -------------------


def _stub_client(responses=None):
    """真实 client + 录制型 `_post` stub（沿用 test_search_injects_collection_filter 模式）。"""
    from fastsearch_client import FastsearchClient

    c = FastsearchClient("http://x", api_key="dev")
    calls = []

    def fake_post(path, body):
        calls.append({"path": path, "body": body})
        if callable(responses):
            return responses(path, body)
        return responses if responses is not None else {"hits": []}

    c._post = fake_post
    return c, calls


def test_search_full_options_body():
    """新增检索参数逐一映射进请求体（snake_case，与 server 契约一致）。"""
    c, calls = _stub_client()
    c.search(
        "kb",
        "毛利率",
        mode="hybrid",
        top_k=5,
        fusion={"rrf": {"k": 60}},
        embedder="bge",
        candidates=100,
        rerank=True,
        auto_merge=True,
        collapse="doc_id",
        search_after="cur-1",
        highlight=True,
        facets=["doc_id"],
        explain=True,
    )
    body = calls[-1]["body"]
    assert body["top_k"] == 5
    assert body["fusion"] == {"rrf": {"k": 60}}
    assert body["embedder"] == "bge"
    assert body["candidates"] == 100
    assert body["rerank"] is True
    assert body["auto_merge"] is True
    assert body["collapse"] == "doc_id"
    assert body["search_after"] == "cur-1"
    assert body["highlight"] is True
    assert body["facets"] == ["doc_id"]
    assert body["explain"] is True
    # 未显式给的可选参数不进请求体（服务端走默认）
    c.search("kb", "q")
    assert "rerank" not in calls[-1]["body"]
    assert "search_after" not in calls[-1]["body"]


def test_search_with_facets():
    c, calls = _stub_client({"hits": SAMPLE_HITS, "facets": {"doc_id": {"rep.pdf": 2}}})
    out = c.search_with_facets("kb", "毛利率", top_k=8, facets=["doc_id"])
    assert out["hits"] == SAMPLE_HITS
    assert out["facets"] == {"doc_id": {"rep.pdf": 2}}
    assert calls[-1]["body"]["facets"] == ["doc_id"]
    # collection 作用域注入同 search（M23）
    assert calls[-1]["body"]["filter"] == {"eq": ["collection", "kb"]}


def test_paginate_follows_cursor_and_stops():
    """paginate：按末条 cursor 续取；不足一页停；游标未推进停（防死循环）。"""

    def hit(cid, cursor):
        return {"citation_id": cid, "cursor": cursor}

    pages = {
        None: [hit("kb:d:1", "c1"), hit("kb:d:2", "c2")],
        "c2": [hit("kb:d:3", "c3"), hit("kb:d:4", "c4")],
        "c4": [hit("kb:d:5", "c5")],  # 不足一页 → 最后一页
    }

    def respond(path, body):
        return {"hits": pages[body.get("search_after")]}

    c, calls = _stub_client(respond)
    got = list(c.paginate("kb", "q", top_k=2))
    assert [len(p) for p in got] == [2, 2, 1]
    assert [call["body"].get("search_after") for call in calls] == [None, "c2", "c4"]

    # 游标未推进：第二页末条 cursor 与当前相同 → 停在两页，不无限循环
    def respond_stuck(path, body):
        return {"hits": [hit("kb:d:1", "cX"), hit("kb:d:2", "cX")]}

    c2, _ = _stub_client(respond_stuck)
    got2 = list(c2.paginate("kb", "q", top_k=2, max_pages=10))
    assert len(got2) == 2

    # max_pages 截断
    c3, _ = _stub_client(respond_stuck)
    assert len(list(c3.paginate("kb", "q", top_k=2, max_pages=1))) == 1


def test_similar_posts_citation_id():
    c, calls = _stub_client({"hits": SAMPLE_HITS})
    hits = c.similar("kb:rep.pdf:3", top_k=4)
    assert len(hits) == 2
    assert calls[-1]["path"] == "/v1/similar"
    assert calls[-1]["body"] == {"citation_id": "kb:rep.pdf:3", "top_k": 4}


def test_resolve_assets_posts_ids():
    c, calls = _stub_client({"assets": [{"citation_id": "kb:rep.pdf:3", "url": "http://x/u"}]})
    assets = c.resolve_assets(["kb:rep.pdf:3", "kb:rep.pdf:9"])
    assert assets[0]["url"] == "http://x/u"
    assert calls[-1]["path"] == "/v1/assets/resolve"
    assert calls[-1]["body"] == {"ids": ["kb:rep.pdf:3", "kb:rep.pdf:9"]}


def test_delete_doc_issues_delete_with_slash_doc_id():
    """DELETE /v1/docs/{collection}/{doc_id}：doc_id 可含 `/`（server 通配段），不转义斜杠。"""
    from fastsearch_client import FastsearchClient

    c = FastsearchClient("http://x", api_key="dev")
    captured = {}

    def fake_request(method, path, body=None, **kw):
        captured.update({"method": method, "path": path})
        return {"deleted": True, "pg_deleted": 3}

    c._request = fake_request
    out = c.delete_doc("kb", "sub/d.md")
    assert out["deleted"] is True and out["pg_deleted"] == 3
    assert captured["method"] == "DELETE"
    assert captured["path"] == "/v1/docs/kb/sub/d.md"


def test_fetch_asset_bytes_variants():
    """inline 字节 → (bytes, ct)；JSON（DocRender）→ None；404 → None；其余错误上抛。"""
    from fastsearch_client import FastsearchClient, FastsearchError

    def client_with(raw_result=None, error=None):
        c = FastsearchClient("http://x", api_key="dev")

        def fake_request(method, path, body=None, raw=False, **kw):
            assert method == "GET" and raw
            # citation_id 整体 percent-encode（含 `:`）
            assert path == "/v1/asset/kb%3Arep.pdf%3A3"
            if error is not None:
                raise error
            return raw_result

        c._request = fake_request
        return c

    got = client_with((b"\x89PNG", "image/png")).fetch_asset_bytes("kb:rep.pdf:3")
    assert got == (b"\x89PNG", "image/png")
    assert client_with((b"{}", "application/json")).fetch_asset_bytes("kb:rep.pdf:3") is None
    err404 = FastsearchError("HTTP 404: not found", 404, "not found")
    assert client_with(error=err404).fetch_asset_bytes("kb:rep.pdf:3") is None
    err500 = FastsearchError("HTTP 500: boom", 500, "boom")
    try:
        client_with(error=err500).fetch_asset_bytes("kb:rep.pdf:3")
        raise AssertionError("500 应上抛")
    except FastsearchError as e:
        assert e.status == 500


def test_request_retries_transient_then_succeeds():
    """retries>0：网络错先退避重试，成功即返回；默认 retries=0 保持原一击即抛。"""
    import io
    import urllib.error
    import urllib.request

    from fastsearch_client import FastsearchClient, FastsearchError

    attempts = []

    class FakeResp(io.BytesIO):
        headers = {"Content-Type": "application/json"}

        def __enter__(self):
            return self

        def __exit__(self, *a):
            return False

    real_urlopen = urllib.request.urlopen

    def flaky_urlopen(req, timeout=None):
        attempts.append(req.full_url)
        if len(attempts) == 1:
            raise urllib.error.URLError("connection refused")
        return FakeResp(b'{"hits": []}')

    urllib.request.urlopen = flaky_urlopen
    try:
        c = FastsearchClient("http://x", api_key="dev", retries=2)
        assert c.search("kb", "q") == []
        assert len(attempts) == 2  # 失败 1 次 + 成功 1 次
        # retries=0（默认）：不重试，直接抛
        attempts.clear()
        c0 = FastsearchClient("http://x", api_key="dev")
        try:
            c0.search("kb", "q")
            raise AssertionError("应抛 FastsearchError")
        except FastsearchError:
            pass
        assert len(attempts) == 1
    finally:
        urllib.request.urlopen = real_urlopen


def main():
    tests = [
        test_hit_to_document,
        test_hits_to_documents,
        test_hit_to_llama_node,
        test_retriever_invoke_and_param_passthrough,
        test_retriever_kwargs_bind_real_client_signature,
        test_search_injects_collection_filter,
        test_search_full_options_body,
        test_search_with_facets,
        test_paginate_follows_cursor_and_stops,
        test_similar_posts_citation_id,
        test_resolve_assets_posts_ids,
        test_delete_doc_issues_delete_with_slash_doc_id,
        test_fetch_asset_bytes_variants,
        test_request_retries_transient_then_succeeds,
    ]
    for t in tests:
        t()
        print(f"ok  {t.__name__}")
    print(f"\nall {len(tests)} integration tests passed")


if __name__ == "__main__":
    try:
        main()
    except AssertionError as e:
        print(f"FAIL: {e}", file=sys.stderr)
        sys.exit(1)
