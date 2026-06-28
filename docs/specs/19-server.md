# spec · fastsearch-server

> 模块 #10，依赖：core、engine。阶段 P4。上游：[产品设计 §3.6/§3.8/§4](../plans/2026-06-24-产品设计文档.md)、需求 F43–F46/F50/F54。
> 状态：**已完成 v1.6**（认证/ACL 不可绕过 + 指标/限流/审计 + 嵌入 + CDC 生命周期 +
> 媒资网关 + 深分页 + 多向量后端 env）。MCP 第四张脸已独立成 `fastsearch-mcp` crate。

## 1. 目的与范围

REST 服务（四张脸之一）+ 安全 + 基础可观测。

- 端点：`GET /healthz` `/readyz` `/metrics` `/openapi.json`；`POST /v1/search`（含 `search_after` 深分页，命中带 `cursor`）；`POST /v1/similar`（按 citation_id more_like_this）；`GET /v1/asset/{cid}`（媒资 ACL 网关，authed）；`GET /v1/asset/{cid}/bytes?exp&ct&sig`（**token 门控** inline 字节，免 Bearer，MM6-signer）；`POST /v1/index`。
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
- `POST /v1/search` body = `SearchRequest`（core，serde）。注意：**body 里若带 ACL 字段会被忽略**——ACL 只来自认证身份。响应 = `{hits:[{citation_id,score,page,bbox,heading_path,doc_id,chunk_id,bm25,vector}]}`。
- `POST /v1/index` body = `{collection, doc_id, chunks:[Chunk]}` → ingest+commit，返回 `{indexed:n}`。
- 401（无/错 key）、400（坏 body）、200（成功）。

## 3. 行为规约

- **认证强制**：除 `/healthz`/`/readyz`/`/metrics` 外都要求合法 key。
- **ACL 注入**：search 一律以 `acl_for(principal)` 调 engine.search（Some），客户端不可绕过；越权 chunk 不出现在结果。
- **健壮**：坏 JSON→400、不 panic；engine 错误→500 + 简短信息。
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
- [x] v1.8（2026-06-28，MM6-signer S1+S2）：`resolve_citation` Inline→`InlineRef`（只定位）、字节经 engine `fetch_inline_bytes` 按需取；新增 **`AssetSigner`**（HMAC-SHA256(`cid\|exp\|ct`)，常量时间验签）+ **token 门控 `GET /v1/asset/{cid}/bytes`**（验签即授权=presigned 语义，免 Bearer；未配/无效/过期→403；无字节→404）+ env `FASTSEARCH_ASSET_SIGNING_KEY`/`_URL_TTL`。+5 单测（sign/verify 往返/过期/篡改 cid/ct/sig/密钥、端点 403/404 路径，本环境）。**S3：`POST /v1/assets/resolve`**（authed 批量：每 id resolve_citation→ACL→ InlineRef 签 token URL / Object 签名 URL / DocRender JSON；**越权 id 省略不暴露**）+ `mint_inline_url`（cid/ct 百分号编码）。+2 单测（mint↔字节端点验签闭环、resolve 越权省略）。**inline 档"搜索→resolve→`<img src>`"端到端就绪**（真字节路 Docker PG 验证）；object 真 presign(S4) gated；OpenAPI 条目=小 follow-up。
- [x] v1.7（2026-06-27，MM6-inline/secure）：main 装配 `set_source_store`（gated DATABASE_URL，任意向量后端）→
  `/v1/asset` 的 **Inline 路径从 PG `media_bytes` 真源吐字节**（+Content-Type）。**server HTTP E2E** `asset_inline_bytes_e2e`
  （Docker 真机：授权 200+image/png+真源字节 / 越权 404 / 无 key 401）。**Object 无签名器→404 不泄露裸 key**（MM6-secure）。
  真签名 URL（S3 presign）/ Range 待对象存储（gated）。

**已知限制 / 下一迭代：** RBAC 细粒度策略引擎、TLS（交网关）、并发优化（当前 Mutex 串行；后续 RwLock/副本，见 [容量·SLO](../governance/2026-06-26-容量与SLO.md)）。MCP 工具面已独立实现（`fastsearch-mcp`）。
