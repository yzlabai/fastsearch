# spec · fastsearch-server

> 模块 #10，依赖：core、engine。阶段 P4。上游：[产品设计 §3.6/§3.8/§4](../plans/2026-06-24-产品设计文档.md)、需求 F43–F46/F50/F54。
> 状态：**已完成 v2.8**（认证/ACL 不可绕过 + 指标/限流/审计 + 嵌入 + CDC 生命周期 +
> 媒资网关 + 签名 URL + inline Range + 深分页 + 多向量后端 env）。MCP 第四张脸已独立成 `fastsearch-mcp` crate。

## 1. 目的与范围

REST 服务（四张脸之一）+ 安全 + 基础可观测。

- 端点：健康/契约、search/similar/index/assets，以及通用管理端点
  `POST /v1/chunks/batch-get|batch-upsert|batch-delete`、`GET /v1/chunks`、
  `DELETE /v1/collections/{name}`。
- **认证（F43）**：API Key（`Authorization: Bearer <k>` 或 `X-API-Key`）→ Principal{tenant, tags}；缺/错 → 401。
- **逐文档 ACL（F44，安全核心）**：Principal → `AclFilter`，**服务端注入**给 engine.search/resolve_citation；**客户端无法在请求里传 ACL/越权**（含 /v1/asset：越权/不存在均 404，不泄漏存在性）。
- 可观测（F50）：counters + 延迟直方图 `/metrics`（Prometheus 文本）；限流（令牌桶 429）；审计（可插拔 sink）。
- 向量后端：`FASTSEARCH_VECTOR_BACKEND=brute|brute_binary|brute_binary_rotated|hnsw|pgvector`（pgvector 直查需 `DATABASE_URL`，见 [B6 设计](../plans/2026-06-26-B6-pgvector直查档设计.md)）。
- 资产 URL 签名（MM6-signer）：`FASTSEARCH_ASSET_SIGNING_KEY` 设密钥即开启 token URL（`/v1/asset/{cid}/bytes` 凭 HMAC token 取 inline 字节，让前端 `<img src>` 免 Bearer）；`FASTSEARCH_ASSET_URL_TTL` 调过期秒（默认 300）。多副本须同密钥。

**不做**：RBAC 细粒度策略引擎、TLS 终止（交给网关）。（MCP 工具面已实现，见 `fastsearch-mcp`；限流/完整指标已实现。）

## 2. 接口与状态

```rust
pub struct Principal { pub tenant: Option<String>, pub tags: Vec<String> }
// 现状：engine 用 Arc<Mutex<Engine>>；CDC 嵌入准备锁外，PG 写穿/本地发布/持久化与检索串行。
pub struct ServerState { engine: Arc<Mutex<Engine>>, keys, metrics, rate_limiter, audit, embedder }
pub fn router(state) -> axum::Router;
pub fn principal_from_headers(headers, keys) -> Option<Principal>;  // 纯, 可测
pub fn acl_for(principal) -> AclFilter;                              // 纯, 可测
```

请求/响应：
- `GET /healthz` 始终是进程存活探针。未启用 CDC 时，`GET /readyz` 返回进程级就绪；启用 CDC 后切换为依赖级探针，首轮源轮询成功且无恢复意图/死信漂移才返回 200，否则返回 503，并公开 commit LSN、slot lag、最近成功轮询、死信累计和 rebuild-needed。
- `POST /v1/search` body 经 REST 外部契约解码为 `SearchRequest`。图片字节只接受 `query_image_base64`（或图片上传接口），内部字段 `query_image` 明确 400。当前嵌入后端是服务端级配置，`embedder != null` 的逐请求选择明确 400。ACL 只来自认证身份。`include_text`/`include_metadata` 默认 false；`explain=false` 时省略 `sources`，开启后每条命中附来源、rank、原始分和融合贡献。
- `POST /v1/index` body = `{collection, doc_id, chunks:[Chunk]}` → ingest+commit，返回 `{indexed:n}`。Chunk 支持默认 `{}` 的 `metadata` 和默认 true 的 `searchable`；metadata 在副作用前校验。
- chunk 管理端点以现有 `GlobalId=(collection,doc_id,chunk_id)` 寻址；batch 上限 1000。
  Batch get 保持请求顺序并用 `chunk:null` 合并不可见/不存在；batch delete 同理返回
  `deleted:false`；文档列表按 `chunk_id` 游标分页（默认 100、上限 500）。
- chunk/collection 管理依赖 PostgreSQL 真源；未配置时返回 503。管理读取移除 inline 字节，
  Object 媒资只暴露种类，不暴露 URI/bucket/key。
- 401（无/错 key）、400（坏 body）、200（成功）。

## 3. 行为规约

- **认证强制**：除 `/healthz`/`/readyz`/`/metrics` 外都要求合法 key。
- **ACL 注入**：search 一律以 `acl_for(principal)` 调 engine.search（Some），客户端不可绕过；越权 chunk 不出现在结果。
- **健壮**：坏 JSON→400、不 panic；engine 错误→500 + 简短信息。
- **真源约束**：REST 收到 `searchable=false` 时必须已配置 PostgreSQL source store，否则返回 400；避免把“需持久化但不可检索”的 row 静默丢失。普通 `searchable=true` 兼容既有无 PG 模式。
- **身份覆盖**：batch upsert 与 doc index 一样，由 Principal 强制覆盖 tenant/acl；跨 tenant
  GlobalId 冲突返回 409，不允许覆盖。
- **删除幂等**：chunk 删除、collection 删除重复调用均返回 200；collection 删除按 tenant owner
  scope 清真源，再按 PG 返回的实际 GlobalId/对象列表清派生状态。
- 确定性、无敏感信息泄漏到错误体。

## 4. 依赖

`fastsearch-core`、`fastsearch-engine`、`axum`、`tokio`、`serde`、`serde_json`；dev `tower`（oneshot）。

## 5. 测试用例（用 tower oneshot 打 router，不起真端口）

1. `/healthz` 无需 key → 200。
2. `/readyz` 无需 key；CDC 关闭时 200 且 `scope=process`，CDC 启用但未恢复时 503，成功轮询后 200 且 `scope=dependencies`。
3. `/v1/search` 无 key → 401；错 key → 401；对 key → 200。
4. **ACL 不可绕过**：两个 chunk（team-a / team-b，同 tenant）；以 team-a 的 key 搜 → 只回 team-a 的；即便请求 body 试图放宽也无效。
5. `/v1/index` 写入后 `/v1/search` 能查到、带引用。
6. 坏 body → 400。
7. principal_from_headers / acl_for 纯函数单测。
8. 无 PostgreSQL 时所有管理端点返回 503。
9. 真实 PostgreSQL 路由级生命周期覆盖顺序、metadata/searchable、ACL、跨 tenant 409、分页、
   context-only 不召回、chunk/collection 重复删除及其他 tenant 保留。
10. 管理读取不暴露 `media_bytes` 或 Object 原始定位信息。

## 6. 验收标准与状态

- [x] v1 完成：router + API-Key 认证 + **ACL 服务端注入不可绕过** + /v1/search + /v1/index + /healthz /readyz /metrics + 6 测试绿（HTTP oneshot：健康/认证 401/**acl_not_bypassable**/index→search/坏 body 400/纯函数）。clippy 净、fmt 净。
- [x] 可运行二进制 `fastsearch-server`（main.rs，端口 + key 配置）。
- [x] v1.1：Prometheus 指标完善 —— counters（requests/searches/indexed/**errors/unauthorized/rate_limited**）带标准 `# HELP`/`# TYPE`，+ **检索延迟直方图** `fastsearch_search_latency_seconds`（累积 le 桶 + _sum + _count）。+1 测试（指标含直方图与未授权计数）。
- [x] v1.5（**后台 CDC 同步循环 + 落盘恢复，Docker PG+Ollama 活服务验证 done**，2026-06-25；FS-102 更新）：`Engine::open(data)` 落盘恢复（text+vector.bin+checkpoint）；`spawn_cdc` 后台任务每 `FASTSEARCH_CDC_INTERVAL_MS` 调 `consume_once_shared`（peek→锁外批量嵌入→锁内 PG 写穿/本地发布/落盘→advance）。`FASTSEARCH_CDC=1`+`DATABASE_URL` 开启。日志附带 prepare/lock_wait/lock_hold 微秒数；200ms 故障嵌入实测期间搜索可取得 Engine 锁。
  FS-101 起可用 `FASTSEARCH_CDC_SOURCE_TABLE=schema.table` 配置唯一真源 Relation 白名单（默认 `public.fastsearch_chunks`），防同形旁表进入 chunk 映射。
- [x] v1.4（**真语义混合端到端 Ollama 验证 done**，2026-06-25）：接入可配置嵌入后端（`with_embedder`，从 `FASTSEARCH_EMBEDDER=ollama|openai` 构造）。`/v1/index` **锁外** `spawn_blocking` 嵌入每个 chunk 正文（passage）→ `ingest_vector`；`/v1/search` 在 Hybrid/Vector 模式且未传 vector 时锁外嵌入 query → 真混合。默认（无嵌入后端）行为不变（纯全文）。env-gated 测试：经 server 灌入 + 词面不重叠的语义查询走 vector → 语义最近 chunk 居首（本机 Ollama 验证）。
- [x] v1.3：**OpenAPI 3.0 契约**导出 `GET /openapi.json`（手写、随 API 维护）—— 描述 /v1/search、/v1/index、健康/指标端点 + SearchRequest/Hit/IndexRequest schema + ApiKey 安全方案；version 取 crate 版本。供 SDK 生成/契约校验（F54）。+1 测试（免认证可取、含关键 path/schema）。
- [x] v1.2：**限流/admission control**（`with_rate_limit(capacity, refill_per_sec)`，每 key 令牌桶，超限 429 + 计数）+ **审计日志**（`with_audit(sink)`，每个成功请求发 `AuditEvent{endpoint,tenant,tags,query,collection,doc_id,hits,status}`）。二进制经 `FASTSEARCH_RATE_LIMIT="cap,refill"` / `FASTSEARCH_AUDIT=1`（stderr JSON）接入。+2 测试，活服务验证（cap=2→`200 429 429`，审计 JSON 落 stderr）。

- [x] v1.6（2026-06-26）：**媒资 ACL 网关** `GET /v1/asset/{cid}`（`principal→acl_for→resolve_citation`；
  DocRender JSON / 302 SignedUrl / InlineRef→按需 `fetch_inline_bytes` 吐字节；越权/不存在 404 不泄漏存在性，+测试 `asset_acl_not_bypassable`）；
  **深分页** `search_after` 经 serde 透传 + 响应每命中带 `cursor`（+REST 翻页测试）；media/time 透出命中；
  `FASTSEARCH_VECTOR_BACKEND=hnsw|pgvector`（首启选档 / pgvector `set_pg_vector`）。OpenAPI 同步新端点。
- [x] v1.8（2026-06-28，MM6-signer S1+S2）：`resolve_citation` Inline→`InlineRef`（只定位）、字节经 engine `fetch_inline_bytes` 按需取；新增 **`AssetSigner`**（HMAC-SHA256(`cid\|exp\|ct`)，常量时间验签）+ **token 门控 `GET /v1/asset/{cid}/bytes`**（验签即授权=presigned 语义，免 Bearer；未配/无效/过期→403；无字节→404）+ env `FASTSEARCH_ASSET_SIGNING_KEY`/`_URL_TTL`。+5 单测（sign/verify 往返/过期/篡改 cid/ct/sig/密钥、端点 403/404 路径，本环境）。**S3：`POST /v1/assets/resolve`**（authed 批量：每 id resolve_citation→ACL→ InlineRef 签 token URL / Object 签名 URL / DocRender JSON；**越权 id 省略不暴露**）+ `mint_inline_url`（cid/ct 百分号编码）。+2 单测（mint↔字节端点验签闭环、resolve 越权省略）。**inline 档"搜索→resolve→`<img src>`"端到端就绪**（真字节路 Docker PG 验证）；object 真 presign(S4) gated；OpenAPI 两端点已入 /openapi.json。
- [x] v1.9（2026-06-30，inline Range）：两个 inline 字节出口（authed `GET /v1/asset/{cid}` + token 门控 `/bytes`）支持 **HTTP `Range`**（音视频 seek / 断点续传）。`parse_range` 解析单段 `bytes=A-B`/`A-`/`-N`（后缀式）→ `serve_inline_bytes` 共用组装：无 Range→**200** 全量 + `Accept-Ranges: bytes`；命中→**206** + `Content-Range: bytes A-B/total`（闭区间含端、末端越界自动截断）；起点越界/空体→**416** + `Content-Range: bytes */total`；多段（逗号）→退 200 全量（RFC 7233 允许忽略）。+6 单测（200/206/后缀+开区间/416/多段退化，纯函数确定）。OpenAPI 补 206/416/Range 头。
- [x] v1.7（2026-06-27，MM6-inline/secure）：main 装配 `set_source_store`（gated DATABASE_URL，任意向量后端）→
  `/v1/asset` 的 **Inline 路径从 PG `media_bytes` 真源吐字节**（+Content-Type）。**server HTTP E2E** `asset_inline_bytes_e2e`
  （Docker 真机：授权 200+image/png+真源字节 / 越权 404 / 无 key 401）。**Object 无签名器→404 不泄露裸 key**（MM6-secure）。
  真签名 URL（S3 presign）/ **对象存储档 Range**（交对象存储）随 S4 presign（gated）；**inline 档 Range 已落地**（见 v1.9）。
- [x] v2.0（2026-07-23，通用 chunk 协议）：REST/OpenAPI 暴露 `metadata`、`searchable`、`include_text`、`include_metadata`；响应按 opt-in 省略完整 payload。新增 metadata 限制、无 PG 拒绝 `searchable=false`、真实 PG 持久化但不可检索的测试。
- [x] v2.1（2026-07-23，通用管理 API）：完成 batch get/upsert/delete、文档内分页和幂等
  collection 删除；OpenAPI 同步全部 schema/path。真实 pgvector route test 证明 ACL/tenant/分页/
  幂等语义，Object 定位和 inline 字节经统一管理 DTO 脱敏。
- [x] v2.2（2026-07-23，实例级向量维度）：`FASTSEARCH_EMBED_DIM` 同时约束服务端
  collection 注册；`dim=0` 或与实例维度不一致时在写入前返回 400，`server.vector_dim`
  可用于契约自检。单元测试覆盖维度拒绝，真实 Compose smoke 覆盖 1024 接受/768 拒绝。
- [x] v2.3（2026-08-24，`/v1/index` 的 pgvector 写穿，**Docker pgvector 真机验证**）：直查档（B6）下
  `/v1/index` 在 PG 真源 `upsert_doc` 成功后把向量写回 PG `embedding` 列（`set_embedding`），与
  `/v1/chunks`（`batch_upsert_chunks`）的既有写穿**对齐**。此前只有 `/v1/chunks` 与 CDC `apply_upsert`
  写穿，`/v1/index` 没有：而直查档**读的是 PG `embedding`**、`upsert_doc` 又是 doc 级 delete+insert
  （新行 embedding 必为 NULL），故刚 index 完的文档要等 CDC 消费到才可向量检索（CDC 没开就永远查不到），
  重复 index 同一 doc 更会把已可检索的向量清成 NULL。`embed_model` 标记沿用同一约定
  （`api-precomputed`/`api-embedder`）。+1 env-gated 集成测试
  `index_writes_embedding_through_to_pg_in_pgvector_mode`（index 后立即向量命中 + 重复 index 仍命中；
  去掉写穿即红：0 命中）。**活服务验证**：实跑 server（pgvector 档、**CDC 未开**）→ `psql` 直查真源确认
  两行 `embedding IS NOT NULL` / `embed_model=api-precomputed` → 立即检索命中 → 重复 index 后仍命中。
  详见 [plan §6.1](../plans/2026-08-24-index写穿pgvector对齐chunks.md)。
- [x] v2.5（2026-08-31，FS-002）：OpenAPI SearchRequest 补齐 `fusion`/`embedder`/`explain`，Hit 补齐 `time`/`media`/`sources`；REST/OpenAPI 字段集与共享矩阵做精确集合断言。`explain=true` 的 server 路由测试证明来源明细可见，默认响应继续省略该字段。
- [x] v2.6（2026-08-31，FS-003）：`/readyz` 改为结构化进程级就绪响应，明确不检查 PG/CDC/embedder；单测、OpenAPI 与真二进制 MCP↔server e2e 共同钉住语义。
- [x] v2.7（2026-08-31，FS-103）：CDC 启用后 `/readyz` 升级为真实依赖探针；后台轮询共享 slot lag、最后 commit LSN、最后成功时间、死信累计和 rebuild-needed，Prometheus 同步暴露七项 CDC 指标。未启用 CDC 的进程级契约保持兼容。
- [x] v2.8（2026-08-31，FS-202）：OpenAPI `Hit.sources.items` 增加非必填 `model/model_version`；REST 直接复用 `SourceHit` serde，旧来源不输出空字段，默认 `explain=false` 仍省略整个 sources。
- [x] v2.9（2026-08-31，FS-203）：REST/OpenAPI 增加仅 explain 请求可见的可选 `rerank_explain{status,model,reason?}`；默认响应字节保持不变。真实二进制中纯标点 rerank 返回 200、保留 `[3,2,1]` 与 3 条数量并给出 `empty_query_tokens`，正常文本返回 `applied`。
  显式启用 CDC 但缺 `DATABASE_URL` 时拒绝启动；死信/rebuild 状态持久化，不能靠重启恢复 ready。真实 server + curl 已验证 PG 停止时 200→503、恢复后 503→200。
- [x] v2.4（2026-08-24，**fail-closed 默认 + 运行档如实标注**）：上游决策
  [职责边界：不承担身份与控制面](../governance/2026-08-24-职责边界-不承担身份与控制面.md)——
  身份归调用方，**正因 100% 依赖调用方接对，才不能在他没接对时替他猜**。两处"替他猜"已断根：
  ① `FASTSEARCH_KEYS` 未设不再自造 `dev` 密钥（`tenant: None` 可读**所有租户**的 public 行），
  改为**拒绝启动** + 可直接粘贴的修复命令；② 身份无 tags 时不再把写入 ACL 默认成 `["public"]`
  （把"忘配标签"静默升级为"数据公开"），改为 **403** + 可操作错误信息，覆盖 `/v1/index`、
  `/v1/chunks/batch-upsert`、`/v1/images` 三条写路径。public 依然可用但必须显式（`key=:public`，
  集成指南里本就是既定惯例）。**运行档如实标注**（不变量 #2）：真源有无是运行时事实而非配置声明，
  经启动日志 + introspection（`source_of_truth`/`rebuildable_from_source`）+ metrics gauge
  `fastsearch_source_store_configured` 三处暴露。**未引入 profile 开关**——一个"生产档"布尔会立刻
  产生"默认取哪个"的两难。+3 测试（契约单测、三条写路径 403 且无副作用、local-only 如实报告）。
  文档同步：README×2 / CLAUDE.md / example×2 / 集成指南×2 / benchmarks×2 的 `dev=:` → `dev=:public`。
  详见 [plan](../plans/2026-08-24-fail-closed默认与运行档如实标注.md)。

**已知限制 / 下一迭代：** 写穿是**每 chunk 一条 UPDATE、顺序 await**（`/v1/chunks` 同此形态），
`/v1/index` 又不像 batch 端点那样有 `MAX_CHUNK_BATCH` 上限——大文档会产生成百上千次往返。
本次修复以"与既有路径一字不差地一致"为先，未顺手改批量化；批量 `set_embeddings`
（`UPDATE ... FROM (VALUES ...)`）属 pg crate 的 API 扩展，单独立项。
另：写入的真源与 embedding **不在同一事务**（`set_embedding` 是 `upsert_doc`
提交后的独立 UPDATE，`/v1/chunks` 同）——中间崩溃会留下"真源已提交、embedding 未就绪"的窗口，
由 CDC 补齐；原子化需新增 `upsert_doc_with_embeddings` 事务 API，且写入响应应显式区分
`source_committed`/`embedding_ready`/`derived_index_visible`，均属契约变更，单独立项。
另：RBAC 细粒度策略引擎、TLS（交网关）、并发优化（当前 Mutex 串行；后续 RwLock/副本，见 [容量·SLO](../governance/2026-06-26-容量与SLO.md)）。MCP 工具面已独立实现（`fastsearch-mcp`）。
