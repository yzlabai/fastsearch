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


def main():
    tests = [
        test_hit_to_document,
        test_hits_to_documents,
        test_hit_to_llama_node,
        test_retriever_invoke_and_param_passthrough,
        test_retriever_kwargs_bind_real_client_signature,
        test_search_injects_collection_filter,
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
