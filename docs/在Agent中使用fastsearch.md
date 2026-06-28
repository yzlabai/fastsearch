# 在 Agent 开发中使用 fastsearch

> 🌐 English version: [using-fastsearch-in-an-agent.md](using-fastsearch-in-an-agent.md)

> 面向开发者的使用指南：把 fastsearch 作为 **AI Agent / RAG 的检索与 grounding 层**。
> 参考了 Meilisearch、Qdrant、pgvector、Exa/Tavily 等同类产品在 agentic-RAG 场景的做法
> （见文末「同类对比」），落到 fastsearch 的**真实 API**（本文示例均可直接跑）。

---

## 0. 一句话与定位

**fastsearch = 混合检索引擎（keyword + 向量），以托管 Postgres 为真源，原生面向「可溯源、多租户」的 Agent 检索。**

为什么 Agent 开发选它（相对纯向量库 / 纯关键词）：

| 能力 | 对 Agent 的意义 |
|---|---|
| **可溯源引用**（`citation_id → page+bbox`，`resolve_citation` 深链） | LLM 答案能**带出处**、点回原文页/坐标——grounding 不只是"找到段落"，而是"指到位置" |
| **混合检索**（keyword∥vector→RRF 融合） | agentic-RAG 的召回层：精确词面 + 语义理解，先召回准候选，再做校正/验证循环 |
| **ACL 不可绕过**（按 API Key 服务端注入） | 多租户 Agent 天然隔离——客户端/LLM **无法**在请求里传或放宽权限 |
| **MCP 原生**（第四张脸） | LLM 直接把 `search`/`resolve_citation` 当工具调，无需自己写 HTTP 胶水 |
| **过滤 filter-aware** | 选择性强的过滤**不掉召回**（超越 pgvector 后过滤坑），适合 corrective retrieval |
| **高亮片段** | 只回命中片段，**省 token**（对位 Exa highlights） |
| **PG 真源、不锁定** | 数据在你自己的托管 PG（RDS/Supabase/Neon），引擎索引可重建；无专有存储 |

---

## 1. 五分钟上手（本机零依赖闭环）

不需要 PG、不需要模型——喂一个**文件夹**就能检索：

```bash
cargo build -p fastsearch-server -p fastsearch-cli
# ⓪ CLI 是瘦 REST 客户端——先起 server（索引/嵌入/落盘都在 server）
FASTSEARCH_DATA=./data FASTSEARCH_KEYS="dev=:" ./target/debug/fastsearch-server &   # REST :8642

# ① 喂一个资料文件夹（递归 .md/.txt，markdown 标题自动成面包屑）
./target/debug/fastsearch index-dir --server http://localhost:8642 --key dev --collection kb ./我的资料

# ② 检索（带 page+heading_path 溯源；--json 给结构化输出）
./target/debug/fastsearch search --server http://localhost:8642 --key dev --collection kb --query "毛利率" --json
```

输出（每条命中带可溯源引用）：

```json
[{ "citation_id": "kb:reports/2024年报.md:4", "score": 0.0164,
   "doc_id": "reports/2024年报.md", "page": 1,
   "heading_path": ["2024 年度财务报告", "风险提示"] }]
```

> 这是**纯本机、无外部依赖**的端到端检索闭环——适合先验证检索质量，再决定接 PG/向量/Agent。

---

## 2. Agent 开发的核心概念

### 2.1 引用与 grounding（最关键）

每条命中带 **`citation_id` = `collection:doc_id:chunk_id`** + `page` + `bbox` + `heading_path`。
让 LLM 在答案里引用 `citation_id`，再用 **`resolve_citation`** 把它解析成可深链的原文位置：

```
检索 → 把 {citation_id, snippet} 喂进 prompt → LLM 生成带 [citation_id] 的答案
     → resolve_citation(citation_id) → {page, bbox} → 前端高亮/跳转原文
```

REST：`GET /v1/asset/{citation_id}` → `DocRender{page,bbox}` / 媒资签名 URL（ACL 强制，越权/不存在均 404）。

### 2.2 三种检索模式

- `keyword`：BM25 全文，确定、亚毫秒、无需模型。**默认起点**。
- `vector`：语义近邻（需嵌入后端，见 §7）。词面不重叠也能召回。
- `hybrid`：keyword∥vector → RRF 融合。**agentic-RAG 推荐**（精确 + 语义）。

### 2.3 过滤（filter-aware，不掉召回）

过滤翻译成**超集索引查询 + 精确后过滤**——选择性强的过滤不丢召回。Filter AST（JSON，snake_case 外标签）：

```json
{"and": [
  {"eq": ["modality", "image"]},
  {"gte": ["page", 5]},
  {"in": ["kind", ["table", "paragraph"]]}
]}
```

算子：`and/or/not · eq/ne/gt/gte/lt/lte · in · exists · heading_prefix`。
可过滤字段：`kind / modality / doc_id / collection / page / section_id / tenant`（time 在媒资里）。

### 2.4 高亮（省 token）

`"highlight": true` → 命中带 `highlight` 片段（命中词包 `<b>`）。只把片段喂 LLM，省 context。

---

## 3. 四种接入方式（四张脸）

### 3.1 MCP —— Agent 原生（推荐给 LLM agent）

把检索作为 **MCP 工具**暴露给任意 MCP 客户端（Claude Desktop / IDE / 自建 host）。无需写 HTTP 胶水。

```bash
cargo build -p fastsearch-mcp --bin fastsearch-mcp
```

MCP 客户端配置（stdio server）：

```json
{
  "mcpServers": {
    "fastsearch": {
      "command": "/abs/path/target/debug/fastsearch-mcp",
      "env": {
        "FASTSEARCH_DATA": "/abs/path/idx",
        "FASTSEARCH_TOKENIZER": "jieba",
        "FASTSEARCH_MCP_TENANT": "acme",
        "FASTSEARCH_MCP_TAGS": "team-a,public"
      }
    }
  }
}
```

暴露两个工具：

- **`search`**：入参 `{query, mode?, top_k?, filter?, highlight?}` → 带引用命中（citation_id/page/heading_path/snippet）。
- **`resolve_citation`**：入参 `{citation_id}` → 媒资/原文位置（page+bbox 或签名 URL）。

ACL 由服务端 env（`FASTSEARCH_MCP_TENANT/TAGS`）注入——LLM 的工具入参**无法**夹带或放宽权限。

### 3.2 REST —— 任意 Agent 框架

```bash
FASTSEARCH_DATA=./idx FASTSEARCH_KEYS="dev=acme:team-a,public" \
  cargo run -p fastsearch-server --bin fastsearch-server   # :8642
```

```bash
# 检索（hybrid + 过滤 + 高亮 + 深分页游标）
curl -s localhost:8642/v1/search -H "x-api-key: dev" -H "content-type: application/json" -d '{
  "query": "毛利率为什么下降", "mode": "hybrid", "top_k": 8, "highlight": true,
  "filter": {"eq": ["kind", "table"]}
}'
# → { "hits": [ { "citation_id", "score", "page", "bbox", "heading_path",
#                 "highlight", "cursor", "time", "media" } ], "facets": {} }

# 相似（more_like_this，按 citation_id 反查）
curl -s localhost:8642/v1/search ... ; curl -s localhost:8642/v1/similar -H "x-api-key: dev" \
  -d '{"citation_id":"kb:rep.pdf:3","top_k":5}'

# 媒资 / 原文位置网关（ACL 强制；越权/不存在 → 404）
curl -s localhost:8642/v1/asset/kb:rep.pdf:3 -H "x-api-key: dev"

# 灌入（doc 级替换）
curl -s localhost:8642/v1/index -H "x-api-key: dev" -d '{"collection":"kb","doc_id":"d.pdf","chunks":[...]}'
```

认证：`X-API-Key: <k>` 或 `Authorization: Bearer <k>`。
深分页：把上一页末条命中的 `cursor` 作为下次请求的 `"search_after"`。
契约：`GET /openapi.json`（OpenAPI 3.0）；可观测：`GET /metrics`（Prometheus）。

### 3.3 SDK（Python / TypeScript + LangChain / LlamaIndex）

**Python**（[PyPI 待发布](../clients/python/README.md)，现从源码安装）：

```python
from fastsearch_client import FastsearchClient
from fastsearch_client.integrations import FastsearchRetriever, hits_to_llama_nodes

c = FastsearchClient("http://127.0.0.1:8642", api_key="dev")
c.index("kb", "report.pdf", chunks)          # chunks: docparse chunk dict 列表
hits = c.search("kb", "毛利率", mode="hybrid", top_k=8)

# LangChain：鸭子兼容 get_relevant_documents/invoke，直接进 LCEL 管道
retriever = FastsearchRetriever(c, "kb", mode="hybrid", top_k=8, highlight=True)
docs = retriever.invoke("毛利率为什么下降")    # -> list[Document]（metadata 含 citation_id）

# LlamaIndex：命中 -> NodeWithScore
nodes = hits_to_llama_nodes(c.search("kb", "毛利率", top_k=8, highlight=True))
```

依赖可选：未装 langchain/llama-index 时回退本地等价对象（同形 `page_content`/`metadata`）。

**TypeScript**（已发布 npm：`npm install fastsearch-client`；零依赖、Node 18+/Deno/Bun/浏览器通用）：

```ts
import { FastsearchClient, makeSearchTool, formatHitsForLLM, FastsearchRetriever } from "fastsearch-client";

const client = new FastsearchClient({ baseUrl: "http://127.0.0.1:8642", apiKey: "dev" });
await client.index("kb", "report.pdf", chunks);                 // chunks: docparse chunk 列表
const { hits } = await client.search("kb", "毛利率", { topK: 8, highlight: true });

// 给 agent 加检索工具：一次产出 Anthropic / OpenAI 两家工具定义 + 可执行 run()
const tool = makeSearchTool(client, "kb");                       // tool.anthropic / tool.openai / tool.run()

// 自己拼 RAG 上下文（带 [n] 引用标记）
const { content, citations } = formatHitsForLLM(hits);

// LangChain.js 检索器（鸭子兼容，可进 LCEL）
const docs = await new FastsearchRetriever(client, "kb", { topK: 8 }).invoke("毛利率");
```

完整用法见 [TS SDK README](../clients/typescript/README.md)（agent 工具、深分页、similar、媒资深链、错误重试）。

### 3.4 库（Rust）/ CLI

- **库**：`fastsearch_engine::Engine`（`create_in_ram` / `open`）+ `engine.search(req, acl)` / `resolve_citation`。
- **CLI**：`index`（docparse chunks）· `index-dir`（喂文件夹）· `search` · `ingest`（PDF，`--features parse`）· `eval`（相关性门禁）。

---

## 4. 典型 RAG / Agentic 流程（recipe）

```
① 检索      hybrid + highlight + filter   → top-K 带引用命中（citation_id/snippet）
② 组 prompt 把 snippet + citation_id 拼进上下文，要求 LLM 引用 [citation_id]
③ 生成      LLM 产出带 [citation_id] 的答案
④ 溯源      对答案里的 citation_id 调 resolve_citation → page+bbox → 前端深链/高亮
```

**Agentic 校正循环**（agent 自查 → 再检索）：

- 召回不足 → 调 `mode=hybrid`、放宽 `filter`、加大 `top_k`/`candidates`。
- 太宽泛 → 收紧 `filter`（`eq/in` on kind/modality/page）、`collapse` 每文档限 N 条防刷屏。
- 找"更多类似" → `/v1/similar`（more_like_this，按命中 `citation_id`）。
- 翻页 → `search_after`（取上页末条 `cursor`），与融合/rerank 排序一致、无重叠。
- 自评 → `fastsearch eval --golden ... --baseline ...`（nDCG/recall/MRR 回归门禁）。

---

## 5. 多租户 Agent（ACL 不可绕过）

把 API Key 映射到租户与标签，**服务端注入**到每次检索/解析——LLM 与客户端都无法越权。

```bash
# key=tenant:tag1,tag2 ; 分号分隔多个；tenant 留空=无租户限制（管理员）
FASTSEARCH_KEYS="alice=acme:team-a,public; bob=acme:team-b; admin=:public"
```

- `alice`（acme/team-a）只看到 `acl` 含 `team-a` 或 `public` 且 `tenant=acme` 的 chunk。
- 同一引擎、同一 MCP/REST，不同 key → 不同可见集；**请求体里传 `acl`/`tenant` 无效**（被忽略）。
- 媒资网关 `/v1/asset` 同样强制：越权/不存在均 404，不泄漏存在性。

> 适合"一个 Agent 服务多租户"：每租户一把 key，检索自动隔离，无需在 prompt/应用层做权限。

---

## 6. 多模态

> **文档解析 & OCR/表格**：`fastsearch ingest` 进程内解析 9 格式 + 图片；扫描件走 **PP-OCR** 抽文本、表格走**非 VLM 的 ONNX 结构识别**——见 **[文件解析与摄取](文件解析与摄取.md)**。

图片 caption、音视频转录都作为可检索文本进库；`modality` 是可过滤字段：

```json
{"query": "营收趋势图", "filter": {"eq": ["modality", "image"]}}
```

命中带 `media`（媒资引用）与 `time`（音视频区间）；`resolve_citation` 解析媒资位置/签名 URL（ACL 强制）。

---

## 7. 生产部署与配置（env 速查）

| 变量 | 作用 |
|---|---|
| `FASTSEARCH_DATA` | 索引数据目录（默认 `./data`） |
| `FASTSEARCH_KEYS` | API Key 表 `key=tenant:tags;...`（不设=单 dev key、无租户限制） |
| `FASTSEARCH_EMBEDDER` | `ollama`\|`openai`（+ `FASTSEARCH_EMBED_*`）→ 真语义嵌入（开 vector/hybrid） |
| `FASTSEARCH_VECTOR_BACKEND` | `brute`(默认确定)\|`hnsw`(大规模近似)\|`pgvector`(直查，需 `DATABASE_URL`) |
| `FASTSEARCH_CDC=1` | 开后台 CDC：PG 写 → 逻辑复制 → 自动嵌入 → 索引（需 `DATABASE_URL`） |
| `FASTSEARCH_RATE_LIMIT` | `cap,refill`（每 key 令牌桶，超限 429） |
| `FASTSEARCH_AUDIT=1` | 每个成功请求向 stderr 输出审计 JSON |
| `FASTSEARCH_TOKENIZER` | `jieba`(默认,中文)\|`default`(空白切分) |

- **本机语义检索**：`FASTSEARCH_EMBEDDER=ollama`（+本地 ollama）→ hybrid 真语义。
- **容器/K8s**：见 [`deploy/`](../deploy/)（Dockerfile + docker-compose + CloudNativePG 样例）。
- **容量/SLO/HA**：见 [容量与 SLO](governance/2026-06-26-容量与SLO.md)（无状态多副本 + 派生可重建）。

---

## 8. 与同类产品对比（诚实定位）

| 维度 | fastsearch | Meilisearch | Qdrant / Weaviate | pgvector(裸) | Elasticsearch |
|---|---|---|---|---|---|
| 检索 | keyword+向量**混合**(RRF) | keyword+向量混合 | 向量为主(+部分 BM25) | 向量 | keyword+向量 |
| **可溯源引用→深链** | ✅ page+bbox + `resolve_citation` | 高亮/属性 | 元数据 | 自理 | 高亮 |
| **ACL 不可绕过** | ✅ 服务端注入、多租户 | 应用层/multi-tenancy | 应用层 | 自理 | 文档级安全(商业) |
| **MCP 原生工具** | ✅ search/resolve_citation | ✅(近期) | 经第三方 | — | 经第三方 |
| 真源/锁定 | **托管 PG 真源、可重建** | 专有存储 | 专有存储 | 就是 PG | 专有存储 |
| filter-aware 召回 | ✅ 超集+精确后过滤 | ✅ | 视实现 | **后过滤易掉召回** | ✅ |
| 部署 | 单二进制 + 任意托管 PG | 单二进制 | 集群 | PG 内 | 集群 |

**何时选 fastsearch**：你要给 Agent 一个**带可溯源引用 + 多租户隔离**的检索层，且数据想留在**自己的托管 Postgres**里、不被专有存储锁定。纯语义/海量向量场景，专用向量库可能更合适。

---

## 参考

- Meilisearch — [Build a RAG pipeline](https://www.meilisearch.com/blog/rag-with-meilisearch)、[Agentic RAG](https://www.meilisearch.com/blog/agentic-rag)、[Hybrid search RAG](https://www.meilisearch.com/blog/hybrid-search-rag)、[RAG Infrastructure](https://www.meilisearch.com/products/rag)
- [Best AI Search Engines for Agents (Firecrawl, 2026)](https://www.firecrawl.dev/blog/best-ai-search-engines-agents)
- 本项目：[README](../README.md) · [架构/CLAUDE.md](../CLAUDE.md) · [模块 spec](specs/00-模块拆分.md) · [REST OpenAPI](../crates/fastsearch-server/src/lib.rs)（`GET /openapi.json`）
