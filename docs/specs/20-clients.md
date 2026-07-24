# spec · clients (Python / TypeScript SDK)

> 模块 #13，依赖：fastsearch-server REST 契约。阶段 P5。需求 F55。状态：**v1 完成**（Python/TS SDK + LangChain/LlamaIndex 适配）。

## 1. 目的与范围

零依赖的瘦客户端 SDK，封装 REST API（index/search），对接 docparse chunk。

- Python：`clients/python/fastsearch_client`（标准库 urllib，无第三方依赖）。
- TypeScript：`clients/typescript/src`（全局 fetch，Node 18+/浏览器）。

**不做**：流式。（**LangChain/LlamaIndex 适配已实现**——见 §5 A10；**TS SDK 已发布 npm**，内建重试退避 + agent 工具定义——见 §5 A11。）

## 2. 接口（两端一致，2026-07-08 M24 补齐后成立）

- `index(collection, doc_id, chunks) -> indexed_count`：docparse chunk（`id`）→ 自动映射 `chunk_id`、注入 `doc_id`、默认 `acl=[public]`。
- `search(collection, query, {mode,top_k,filter,vector,highlight,include_text,include_metadata,fusion,query_image,embedder,candidates,rerank,auto_merge,collapse,search_after,facets,explain})`：Python 回 `hits[]`（分面用 `search_with_facets` 回 `{hits,facets}`）；TS 使用 camelCase `includeText`/`includeMetadata` 并回 `{hits,facets}`（`searchHits` 只要数组）。完整正文与 metadata 默认不返回。**collection 作用域强制注入**为 Eq 过滤、与用户 filter `and` 合并（M23）。
- `similar(citation_id, {top_k}) -> hits[]`：more_like_this。
- `paginate(collection, query, {max_pages,...}) -> 页迭代器`：cursor 深分页，末条无游标/游标未推进即停（防死循环）。
- `resolve_assets(citation_ids) -> assets[]` / `fetch_asset_bytes(citation_id) -> (bytes, content_type) | None`：None=404 或 DocRender（JSON）。
- `delete_doc(collection, doc_id) -> {deleted,...}`：doc_id 可含 `/`；不可见/不存在 404（不暴露存在性）。
- `batch_get_chunks(ids)` / `batch_upsert_chunks(items)` / `batch_delete_chunks(ids)`：通用
  GlobalId chunk 生命周期；读取/删除保持请求顺序并合并不可见与不存在。
- `list_document_chunks(collection, doc_id, {after,limit})`：稳定分页；
  `delete_collection(name)`：删除当前 tenant scope，重复调用幂等。
- `health() -> bool` / `openapi() -> dict`。
- 认证：构造时传 api_key → `X-API-Key` 头。ACL 服务端强制。
- 可选重试：构造参数 `retries`（Python）/`retries`（TS），仅 429/5xx/网络错指数退避，默认 0。

## 3. 行为规约

- 非 2xx → 抛 FastsearchError（含状态码 `status` + 服务端信息 `detail`）。
- JSON (de)serialize 由标准库/内建完成，无第三方依赖。

## 4. 测试

- Python：`test_integrations.py`（零网络 stub，逐方法请求体/路径/停止条件断言，CI `sdk` job 必跑）+ `smoke_test.py` 对真实 server 做 index→search→断言（手动/CI 可选）。
- TS：`npm test`（node --test + stub fetch，CI `sdk` job 必跑）。

## 5. 验收与状态

- [x] v1：Python + TS 客户端 + 包元数据（pyproject/package.json）+ README + smoke。
- [x] **A10（2026-06-26）：LangChain/LlamaIndex 适配已实现**——`clients/python/fastsearch_client/integrations.py`：`FastsearchRetriever`（命中→`Document`）、`hits_to_llama_nodes`（→`NodeWithScore`），依赖可选回退、零硬依赖；`test_integrations.py` 自测绿。
- [x] **A11（2026-06-28）：TS SDK 重写 + 发布 npm**——`fastsearch-client@0.2.0` 已发布 npmjs（`npm install fastsearch-client`）：全量 REST + `makeSearchTool`（Anthropic/OpenAI 工具定义）+ `formatHitsForLLM`（RAG 上下文）+ `FastsearchRetriever`（LangChain.js 检索器）+ `FastsearchError` 重试分流。
- [x] **A12（2026-07-08）：M24 Python SDK 补齐至两端一致**——Python 补 search 全参/`search_with_facets`/`paginate`/`similar`/`resolve_assets`/`fetch_asset_bytes`/`health`/`openapi` + 可选 `retries` 退避（0.1.0→0.2.0）；双端补 server 已有而 SDK 都缺的 `delete_doc`（TS 0.2.0→0.3.0，npm 待发）；Python stub 测试 6→14、TS 16→17，均入 CI `sdk` job。§2"两端一致"自此成立。
- [x] **A13（2026-07-23）：通用命中 payload opt-in**——Python `include_text`/`include_metadata` 与 TS `includeText`/`includeMetadata` 映射到同名 REST 字段；Hit 类型仅在请求时包含 `text`/`metadata`，默认响应载荷不变。
- [x] **A14（2026-07-23）：通用管理协议**——Python/TypeScript 同步增加 batch chunk
  get/upsert/delete、document list 与 collection delete；TS 暴露 GlobalId/Chunk/response 类型。
  零网络协议测试分别为 Python 15/15、TypeScript 18/18。
- 下一迭代：Python SDK 发布 PyPI；TS 0.3.0 发 npm。
