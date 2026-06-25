# spec · fastsearch-server

> 模块 #9，依赖：core、engine。阶段 P4。上游：[产品设计 §3.6/§3.8/§4](../plans/2026-06-24-产品设计文档.md)、需求 F43–F46/F50/F54。
> 状态：**开发中**。

## 1. 目的与范围

REST 服务（四张脸之一）+ 安全 + 基础可观测。

- 端点：`GET /healthz` `/readyz` `/metrics`；`POST /v1/search`；`POST /v1/index`。
- **认证（F43）**：API Key（`Authorization: Bearer <k>` 或 `X-API-Key`）→ Principal{tenant, tags}；缺/错 → 401。
- **逐文档 ACL（F44，安全核心）**：Principal → `AclFilter`，**服务端注入**给 engine.search；**客户端无法在请求里传 ACL/越权**。
- 可观测（F50 v1）：请求计数 + 简单 `/metrics` 文本。

**不做**：MCP（后续）、RBAC 细粒度策略引擎、TLS 终止（交给网关）、限流（后续）、完整 Prometheus 指标（先计数器）。

## 2. 接口与状态

```rust
pub struct Principal { pub tenant: Option<String>, pub tags: Vec<String> }
pub struct ServerState { engine: Arc<RwLock<Engine>>, keys: HashMap<String, Principal>, metrics }
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

**已知限制 / 下一迭代：** MCP 工具面、限流/admission control、完整 Prometheus 指标、RBAC 策略引擎、TLS（交网关）、并发优化（当前 Mutex 串行；后续 RwLock/副本）。
