# spec · clients (Python / TypeScript SDK)

> 模块 #13，依赖：fastsearch-server REST 契约。阶段 P5。需求 F55。状态：**v1 完成**（Python/TS SDK + LangChain/LlamaIndex 适配）。

## 1. 目的与范围

零依赖的瘦客户端 SDK，封装 REST API（index/search），对接 docparse chunk。

- Python：`clients/python/fastsearch_client`（标准库 urllib，无第三方依赖）。
- TypeScript：`clients/typescript/src`（全局 fetch，Node 18+/浏览器）。

**不做**：流式。（**LangChain/LlamaIndex 适配已实现**——见 §5 A10；**TS SDK 已发布 npm**，内建重试退避 + agent 工具定义——见 §5 A11。）

## 2. 接口（两端一致）

- `index(collection, doc_id, chunks) -> indexed_count`：docparse chunk（`id`）→ 自动映射 `chunk_id`、注入 `doc_id`、默认 `acl=[public]`。
- `search(collection, query, {mode,top_k,filter,vector}) -> hits[]`：每条 hit 带 `citation_id/score/bm25/vector/page/bbox/heading_path/section_id`。
- 认证：构造时传 api_key → `X-API-Key` 头。ACL 服务端强制。

## 3. 行为规约

- 非 2xx → 抛 FastsearchError（含状态码 + 服务端信息）。
- JSON (de)serialize 由标准库/内建完成，无第三方依赖。

## 4. 测试

- Python：`smoke_test.py` 对一个真实跑起来的 server 做 index→search→断言（手动/CI 可选）。
- TS：`tsc` 类型检查通过（构建即测）。

## 5. 验收与状态

- [x] v1：Python + TS 客户端 + 包元数据（pyproject/package.json）+ README + smoke。
- [x] **A10（2026-06-26）：LangChain/LlamaIndex 适配已实现**——`clients/python/fastsearch_client/integrations.py`：`FastsearchRetriever`（命中→`Document`）、`hits_to_llama_nodes`（→`NodeWithScore`），依赖可选回退、零硬依赖；`test_integrations.py` 自测绿。
- [x] **A11（2026-06-28）：TS SDK 重写 + 发布 npm**——`fastsearch-client@0.2.0` 已发布 npmjs（`npm install fastsearch-client`）：全量 REST + `makeSearchTool`（Anthropic/OpenAI 工具定义）+ `formatHitsForLLM`（RAG 上下文）+ `FastsearchRetriever`（LangChain.js 检索器）+ `FastsearchError` 重试分流。
- 下一迭代：Python SDK 发布 PyPI。
