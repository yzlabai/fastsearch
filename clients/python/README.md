# fastsearch-client (Python)

零依赖（仅标准库）的 fastsearch REST 客户端。

```python
from fastsearch_client import FastsearchClient

c = FastsearchClient("http://127.0.0.1:8642", api_key="dev")

# 灌入 docparse chunks（docparse -f chunks 的输出）
import json, subprocess
chunks = json.loads(subprocess.check_output(["docparse", "report.pdf", "-f", "chunks"]))
c.index("kb", "report.pdf", chunks)

# 检索（带 page+bbox 引用溯源）
for h in c.search("kb", "毛利率为什么下降", top_k=10, highlight=True):
    print(h["citation_id"], "p", h["page"], h["bbox"])

# 分面计数 / 相似检索 / 深分页 / 资产解析 / 删除（与 TS SDK 同一 API 面）
out = c.search_with_facets("kb", "毛利率", facets=["doc_id"])   # {"hits": [...], "facets": {...}}
more = c.similar("kb:report.pdf:3", top_k=5)                    # more_like_this
for page in c.paginate("kb", "毛利率", top_k=50, max_pages=10): # cursor 深分页逐页扫读
    ...
assets = c.resolve_assets(["kb:report.pdf:3"])                  # citation_id → 短时 URL / 跳原文
c.delete_doc("kb", "report.pdf")                                # 真源 PG + 派生索引一起删

# 可选：瞬态错误自动重试（429/5xx/网络，指数退避；默认 0 不重试）
c = FastsearchClient("http://127.0.0.1:8642", api_key="dev", retries=2)
```

## LangChain / LlamaIndex

`fastsearch_client.integrations` 提供两个生态的检索适配（依赖可选，未装则回退本地等价对象）：

```python
from fastsearch_client import FastsearchClient
from fastsearch_client.integrations import FastsearchRetriever, hits_to_llama_nodes

c = FastsearchClient("http://127.0.0.1:8642", api_key="dev")

# LangChain：鸭子兼容 get_relevant_documents/invoke，可直接进 LCEL 管道
retriever = FastsearchRetriever(c, "kb", mode="hybrid", top_k=8, highlight=True)
docs = retriever.invoke("毛利率为什么下降")   # -> list[Document]

# LlamaIndex：命中 -> NodeWithScore
nodes = hits_to_llama_nodes(c.search("kb", "毛利率", top_k=8, highlight=True))
```

注：`/v1/search` 不回整段正文（载荷精简），`page_content` 取高亮片段（`highlight=True`），
完整正文/深链靠 `metadata["citation_id"]` 经答案层 `resolve_citation` 解析。

ACL 由服务端按 API Key 强制，客户端无法越权。许可 Apache-2.0。
