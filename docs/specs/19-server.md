# spec · fastsearch-server

> 模块 #10，依赖：core、engine。阶段 P4。上游：[产品设计 §3.6/§3.8/§4](../plans/2026-06-24-产品设计文档.md)、需求 F43–F46/F50/F54。
> 状态：**已完成 v1.9**（认证/ACL 不可绕过 + 指标/限流/审计 + 嵌入 + CDC 生命周期 +
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
// 现状：engine 用 Arc<Mutex<Engine>>（写/CDC 与检索串行）；RwLock/副本去串行为未来优化。
pub struct ServerState { engine: Arc<Mutex<Engine>>, keys, metrics, rate_limiter, audit, embedder }
pub fn router(state) -> axum::Router;
pub fn principal_from_headers(headers, keys) -> Option<Principal>;  // 纯, 可测
pub fn acl_for(principal) -> AclFilter;                              // 纯, 可测
```

请求/响应：
- `POST /v1/search` body = `SearchRequest`（core，serde）。注意：**body 里若带 ACL 字段会被忽略**——ACL 只来自认证身份。`include_text`/`include_metadata` 默认 false；开启后命中分别附带完整 `text`/不透明 `metadata`，未开启时字段直接省略。
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
2. `/v1/search` 无 key → 401；错 key → 401；对 key → 200。
3. **ACL 不可绕过**：两个 chunk（team-a / team-b，同 tenant）；以 team-a 的 key 搜 → 只回 team-a 的；即便请求 body 试图放宽也无效。
4. `/v1/index` 写入后 `/v1/search` 能查到、带引用。
5. 坏 body → 400。
6. principal_from_headers / acl_for 纯函数单测。
7. 无 PostgreSQL 时所有管理端点返回 503。
8. 真实 PostgreSQL 路由级生命周期覆盖顺序、metadata/searchable、ACL、跨 tenant 409、分页、
   context-only 不召回、chunk/collection 重复删除及其他 tenant 保留。
9. 管理读取不暴露 `media_bytes` 或 Object 原始定位信息。

## 6. 验收标准与状态

- [x] v1 完成：router + API-Key 认证 + **ACL 服务端注入不可绕过** + /v1/search + /v1/index + /healthz /readyz /metrics + 6 测试绿（HTTP oneshot：健康/认证 401/**acl_not_bypassable**/index→search/坏 body 400/纯函数）。clippy 净、fmt 净。
- [x] 可运行二进制 `fastsearch-server`（main.rs，端口 + key 配置）。
- [x] v1.1：Prometheus 指标完善 —— counters（requests/searches/indexed/**errors/unauthorized/rate_limited**）带标准 `# HELP`/`# TYPE`，+ **检索延迟直方图** `fastsearch_search_latency_seconds`（累积 le 桶 + _sum + _count）。+1 测试（指标含直方图与未授权计数）。
- [x] v1.5（**后台 CDC 同步循环 + 落盘恢复，Docker PG+Ollama 活服务验证 done**，2026-06-25）：`Engine::open(data)` 落盘恢复（text+vector.bin+checkpoint）；`spawn_cdc` 后台任务每 `FASTSEARCH_CDC_INTERVAL_MS` 调 `consume_once`（peek→嵌入→落盘→advance，崩溃安全）。`FASTSEARCH_CDC=1`+`DATABASE_URL` 开启。**活服务验证**：写 PG → 日志 `cdc: applied 2` → 语义 vector 检索命中；**重启**从 checkpoint 续传（resume lsn 非 0）、向量不重嵌、立即可检索。注：consume 期间持引擎锁（与检索串行），低延迟化待引擎并发优化。
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

**已知限制 / 下一迭代：** RBAC 细粒度策略引擎、TLS（交网关）、并发优化（当前 Mutex 串行；后续 RwLock/副本，见 [容量·SLO](../governance/2026-06-26-容量与SLO.md)）。MCP 工具面已独立实现（`fastsearch-mcp`）。
