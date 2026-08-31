# spec · fastsearch-mcp

> 模块 #11（[00-模块拆分](00-模块拆分.md) 第 11 行），依赖：core、engine、text。阶段 P4。
> 上游：[产品设计 §3.6 四张脸](../plans/2026-06-24-产品设计文档.md)、
> [知识库引擎迭代计划 §2.5 KB-0](../plans/2026-08-24-知识库引擎迭代计划.md)、
> [职责边界 ADR](../governance/2026-08-24-职责边界-不承担身份与控制面.md)。
> 姊妹 spec：[19-server](19-server.md)（REST 那张脸）、[17-cli](17-cli.md)（纯 REST 客户端范式）。
>
> **状态**：
> - **本地嵌引擎档：已完成 v1.1**（工具面 + ACL 进程级注入 + **KB-0.1 能力诚实化**，
>   2026-08-24 活服务验证 done）。
> - **远端模式（KB-0.2/0.3）：已完成**（2026-08-25），包含 REST 能力探测、
>   `search`/`resolve_citation` 和远端档专属 `index_chunks`；2026-08-31 新增常态 CI 真二进制 e2e。
> - 本 spec 是**补记的**：`fastsearch-mcp` 此前是唯一没有 spec 的模块，KB-0.1 的"回写 spec"
>   无处可写（只在模块清单留了一行）。本文一并还掉这笔欠账。

---

## 1. 目的与范围

第四张脸：把混合检索暴露成 **MCP（Model Context Protocol）工具**，让任意 MCP 客户端
（Claude Desktop / IDE / 自建 host）里的 LLM agent 直接调用，无需写 HTTP 胶水。
传输 **stdio + JSON-RPC 2.0，一行一条消息**（line-delimited）。

**做**：
- 协议分派：`initialize` / `ping` / `tools/list` / `tools/call`，`notifications/*` 无响应。
- 两个工具：`search`、`resolve_citation`（下方 §4.4）。
- **tool schema 按本实例真实能力生成**（§4.2，本模块的一等硬契约）。
- **ACL 服务端注入、不可绕过**（不变量 #3；两种运行模式各自的守法见 §4.3）。
- 两种运行模式：**本地嵌引擎**（现状）与 **远端 REST 客户端**（KB-0.2，§4.5–§4.9）。

**不做**：
- 不在本 crate 里养第二套嵌入配置（KB-0.1 §10 待决策 #1 已决）。文本语义检索能力**只能**来自
  远端 server；本地档如实宣称自己只有 keyword。
- 不做身份/租户控制面（ADR《职责边界》第 1 类）。远端模式下身份 = 一把 API key，交给 server 翻译。
- 不做 SSE / HTTP streamable transport（当前只有 stdio）。
- 不做原始文件写入工具 `ingest_document`（结构化的 `index_chunks` 已在 KB-0.3 落地）。
- 不做 token 精确预算；当前以 `max_context_chars` 做字符级上下文限制与 per-hit 截断。

---

## 2. 公开接口

### 2.1 现状（已实现，`crates/fastsearch-mcp/src/lib.rs`）

```rust
pub const PROTOCOL_VERSION: &str = "2024-11-05";

pub struct McpServer { /* engine: Engine, acl: Option<AclFilter> */ }

impl McpServer {
    pub fn new(engine: Engine, acl: Option<AclFilter>) -> Self;
    /// 一条 JSON-RPC 消息 → 请求返回 Some(响应)，通知（无 id）返回 None。纯函数，可单测。
    pub fn handle(&self, msg: &Value) -> Option<Value>;
    /// 两个工具的定义（名称/描述/inputSchema），**按实例真实能力生成**。
    pub fn tool_defs(&self) -> Value;
}
```

私有但属契约要害的方法（改它们等于改对外承诺）：
`can_embed_text_query` / `search_modes` / `reject_unavailable_mode` / `tool_search` /
`tool_resolve_citation` / `initialize_result` / `tools_call`，
以及 JSON-RPC 组装 `ok` / `err` / `tool_text`。

二进制壳 `crates/fastsearch-mcp/src/main.rs`：读 env → `Engine::open(&data, cfg)` →
`McpServer::new` → stdin 逐行 `handle` → stdout 逐行写响应。

### 2.2 远端模式（KB-0.2，**2026-08-25 已实现**）

不改 `handle` 的形状——**运行模式是 `McpServer` 的后端选择，不是第二套协议**：

```rust
/// 检索后端：本地嵌引擎 或 远端 server 的 REST 客户端。
pub enum Backend {
    /// 本地档：进程内 Engine + 进程级固定 ACL（部署方给的常量）。
    Local { engine: Engine, acl: Option<AclFilter> },
    /// 远端档：server 的纯 REST 客户端，**本进程不持有任何 ACL**（身份=key，ACL 由 server 注入）。
    Remote(RemoteBackend),
}

/// 远端后端：瘦 HTTP 客户端 + **启动时探到的 server 能力**（不可变，供 schema 生成）。
pub struct RemoteBackend { /* base, key, agent: ureq::Agent, caps: ServerCaps */ }

impl RemoteBackend {
    /// 连接配置：显式 > env（FASTSEARCH_SERVER / FASTSEARCH_KEY）> 默认 http://localhost:8642。
    /// 构造时**必做一次能力探测**（§4.5）；探测失败 → Err（fail-closed，不猜）。
    pub fn connect(server: Option<String>, key: Option<String>) -> Result<Self>;
    pub fn caps(&self) -> &ServerCaps;
}

impl McpServer {
    pub fn with_backend(backend: Backend) -> Self;
    /// 保留：等价于 with_backend(Backend::Local{..})，现有调用点与测试不动。
    pub fn new(engine: Engine, acl: Option<AclFilter>) -> Self;
}
```

> 与 [17-cli](17-cli.md) 的 `Client`（`crates/fastsearch-cli/src/lib.rs`）**同款**：同样的
> `--server/--key` + 同名 env、同样的 `ureq::Agent` + 超时、同样的
> `PostError::{Status, Transport}` 区分（`Status` 是确定性拒绝不重试，`Transport` 可重试）。
> **复用其形状，不复用其代码**：CLI 的 `Client` 是私有实现细节，把它提成公共 crate 属于
> 跨 crate 重构，不在 KB-0.2 范围内（见 §9 已完成的跨文件实施清单；本项未抽取公共客户端 crate）。

---

## 3. 数据结构

### 3.1 工具入参 / 出参

| 工具 | 入参（**允许清单**，见 §4.7） | 出参（单个 text 内容块，内容为 JSON 字符串） |
|---|---|---|
| `search` | `query`(必填, string)、`mode`(enum 由 §4.2 推导)、`top_k`(int, 默认 20)、`filter`(core::Filter AST)、`highlight`(bool)、`vector`(number[]，调用方自带查询向量)、`include_text`(bool)、`max_context_chars`(int) | 基础形状 `{"hits":[{citation_id, score, page, heading_path, snippet}]}`；`include_text=true` 时 hit 增 `text`；设预算时顶层增 `dropped`/`context_chars`，截断 hit 增 `text_truncated` |
| `resolve_citation` | `citation_id`(必填, `"collection:doc_id:chunk_id"`) | `{"found":false,"reason":…}` 或 `{"found":true, media_type, time, fetch:{kind:"doc_render"\|"signed_url"\|"inline_ref", …}}` |

**命中形状在两种模式下必须逐字段相同**：远端模式拿到的是 server `hits_json`
（`crates/fastsearch-server/src/lib.rs`）的富对象（含 `bm25`/`vector`/`rerank`/`bbox`/
`section_id`/`media`/`cursor` 等），**必须投影回上表的五个基础字段**（`highlight` → `snippet`），
否则同一个工具在两种部署下给 agent 两种契约。只有显式 `include_text` 与预算处理才按上表
增加可选字段，且两种模式遵循相同形状；其余富字段仍不透传。

### 3.2 `ServerCaps`（远端能力探测结果，KB-0.2 新增）

```rust
/// 从 GET /v1/collections 的 `server` 字段（server 侧 `server_vector_info`）读到的**实测**运行档。
pub struct ServerCaps {
    /// server 是否配了嵌入后端 = 它会在 /v1/search 里替我们把文本 query 嵌成向量。
    pub embedded: bool,
    pub vector_backend: String,   // brute | hnsw | pgvector | …
    pub vector_dim: Option<usize>,
    pub source_of_truth: String,  // "postgres" | "none"
    pub rebuildable_from_source: bool,
    /// 本 key 名下已注册的集合名（**咨询性、内存态、可能不全**，见 §4.6 caveat）。
    pub collections: Vec<String>,
}
```

---

## 4. 行为规约

### 4.1 JSON-RPC 分派（现状，两种模式一致）

- `initialize` → `{protocolVersion: PROTOCOL_VERSION, capabilities:{tools:{}}, serverInfo:{name:"fastsearch-mcp", version: CARGO_PKG_VERSION}}`。
- `ping` → `{}`；`tools/list` → `{tools: tool_defs()}`；`tools/call` → 见下。
- 无 `id` 且 method 以 `notifications/` 开头 → **不响应**（返回 `None`）。
- 未知 method → JSON-RPC error `-32601`；stdin 那行不是合法 JSON → `-32700`（`main.rs`，`id:null`）。
- **工具级失败不发协议 error**：`tools/call` 的失败一律是 `result` + `isError:true` + 文本原因
  （`tool_text`）——MCP 约定，让 LLM 读得到失败原因并自纠。协议级 error 只留给协议本身的错误
  （未知 method / parse error / 缺 `params`(-32602)）。

### 4.2 硬契约 C1：**schema 宣称的能力 == 实际能力**（本模块第一条规矩）

KB-0.1 修的 bug 是它的反例：`mode` 无条件宣称 `["keyword","vector","hybrid"]` 且
`default:"hybrid"`，而本 crate 零 embedder、`Engine::run()` 又从不嵌入文本 query
（`crates/fastsearch-engine/src/lib.rs` 的 `run` 与 `embed_query_image`：`embedder` 只服务
`query_image`），于是 agent 按默认档调用，永远拿到纯 keyword 结果且毫不知情。
**agent 没有人类的试错直觉，它只能读到什么就信什么**——所以这条不是文档建议，是契约：

> **C1（双向）**：`tools/list` 宣称的每一档 / 每一个入参，本实例必须真能兑现；
> 反之，本实例真能兑现却**未宣称**的能力，一律**拒绝**（不得作为暗门存在）。

- **正向（已落地）**：`search_modes()` ← `can_embed_text_query()` 推导 `mode` 的 `enum`/`default`
  与工具 `description`；`reject_unavailable_mode()` 在显式要了给不出的档时返回**可自纠的错误**
  （告诉 agent ①改 `keyword` ②走 REST `/v1/search` ③自带 `vector`），而不是静默退化。
  另：`tool_search` 检测 `mode` 是否**显式给出**，未给则强制走 `SearchMode::Keyword`——因为
  `SearchRequest` 的 serde 默认是 `Hybrid`（`crates/fastsearch-core/src/query.rs`）而 schema
  宣称的默认是 `keyword`，两个 default 不对齐的话省略 `mode` 仍会掉回静默退化。
- **反向（已落地）**：`tool_search` 只从 schema 同源允许清单投影参数；未宣称字段在反序列化前
  逐项拒绝并点名。FS-002 又把允许集接入共享字段矩阵，并遍历 REST 的矩阵外字段证明均明确拒绝，
  不再存在 `explain`/`fusion`/`query_image_base64` 等暗门。
- **C1 适用于整张工具表，不只是 `mode`**：某种能力（如 KB-0.3 的写入工具）只有远端模式具备时，
  本地模式的 `tools/list` 里**不得出现该工具**——不是"出现了但会报错"。

### 4.3 ACL 注入：两条路径，同一条不变量

不变量 #3 的判据是唯一的：**ACL 只能来自认证身份，由服务端在过滤期注入，工具入参既不能传也不能放宽。**

| | 本地嵌引擎档（现状） | 远端 REST 档（KB-0.2） |
|---|---|---|
| 身份来源 | 进程级 env `FASTSEARCH_MCP_TENANT` / `_TAGS`（部署方给的**常量**，一个进程一个租户） | 一把 API key（`--key`/`FASTSEARCH_KEY`）→ server `principal_from_headers` → `acl_for` |
| 注入点 | `McpServer` 持有的 `acl: Option<AclFilter>` → `engine.search(&req, self.acl.as_ref())` / `engine.resolve_citation(cid, self.acl.as_ref())` | server 的 `search_request` / `assets_resolve`：`engine.search_with_facets(&req, Some(&acl))`、`engine.resolve_citation(cid, Some(&acl))` |
| 工具入参能否影响 ACL | **不能**：`acl` 字段来自 `McpServer` 自己，`SearchRequest` 里根本没有 tenant/acl 字段 | **不能**：MCP 只发 `SearchRequest` 的允许清单字段 + `Authorization: Bearer`；ACL 在 server 侧从 header 推导 |
| 越权用例 | §6 用例 11 | §6 用例 16、21 |

守法要点（两条路径都要逐条对得上）：

1. **`SearchRequest` 没有 ACL 面**：`crates/fastsearch-core/src/query.rs` 的 `SearchRequest`
   不含 `tenant`/`acl` 字段，且**未加** `deny_unknown_fields` ⇒ agent 在 `arguments` 里伪造
   `"tenant"`/`"acl"` 会被 serde 直接丢弃，不会被任何一层当真。§4.7 的允许清单把"丢弃"升级为
   "显式拒绝 + 可自纠错误"，两者都不越权，后者更符合 KB-0.5。
2. **远端档本进程不得持有 ACL**：`Backend::Remote` 里**没有** `AclFilter` 字段。
   `FASTSEARCH_MCP_TENANT`/`_TAGS` 在远端档下是**无意义且危险**的（它是客户端自称的身份）
   ⇒ 同时设了 `FASTSEARCH_SERVER` 与 `FASTSEARCH_MCP_TENANT` → **拒绝启动**并给出改法
   （二选一：删掉 env 走远端 key，或删掉 `--server` 走本地档）。不猜、不合并、不静默取其一。
3. **远端档不得自行做任何 ACL 后过滤**：ACL 已在 server 侧的过滤期落实（text/vector 两端的
   SUPERSET 预过滤 + `AclFilter::visible` 精确后过滤，不变量 #5）。MCP 再补一层只会产生
   "两处判权且可能不一致"的第二真源。
4. **本地档的 fail-open 缺口（历史问题，KB-0.2 已收口）**：修复前 `main.rs` 里 `FASTSEARCH_MCP_TENANT`
   未设 → `acl = None` → `engine.search(&req, None)` ⇒ **不做任何 ACL 判定，全库可见**
   （`AclFilter::visible` 在 `tenant: None` 时也是"放行所有租户的 public 行"，
   `crates/fastsearch-core/src/filter.rs`）。这正是 server v2.4 fail-closed 修掉的那类"替调用方猜"
   （`crates/fastsearch-server/src/main.rs`：`FASTSEARCH_KEYS` 未设 → **拒绝启动** + 可粘贴的修复命令），
   而 ADR《职责边界》§"不豁免" 第 1 条写明：*既然 100% 依赖调用方接对，就绝不能在他没接对时替他编一个*。
   ⇒ **KB-0.2 已一并收紧本地档**：要么显式给 `FASTSEARCH_MCP_TENANT`，要么显式写
   `FASTSEARCH_MCP_ACL=all`（单机全量，需要写出来），否则拒绝启动。
   这是对既有本地部署的**破坏性变更**，已同步中英文 Agent 使用指南。

### 4.4 工具契约

**`search`**
1. 取 `arguments`（缺 `params` → `-32602`）。
2. **允许清单校验**（§4.7）→ 不通过即 `isError:true` + 列出本实例接受的字段。
3. `mode` 未显式给出 → 用 schema 宣称的 default（本地档 `keyword`；远端档见 §4.5）。
4. 能力守卫 `reject_unavailable_mode`：显式要了给不出的档 → `isError:true` + 三条改法。
   调用方自带 `vector` 时可走向量路；`query_image` 与 `query_image_base64` 在两档均明确拒绝并指路 REST。
5. 执行：本地档 `engine.search(&req, acl)`；远端档 `POST /v1/search`。
6. 投影成 §3.1 的基础命中，再按 `include_text`/`max_context_chars` 增加正文与可见的截断记账，序列化成 JSON 字符串放进单个 text 块。
7. 失败一律 `isError:true`（含反序列化失败、engine 错误、server 非 2xx）。

**`resolve_citation`**
1. 缺 `citation_id` → `isError:true`（`missing citation_id`）。
2. 本地档：`engine.resolve_citation(cid, acl)`；`None` → `{"found":false,"reason":"not found or not authorized"}`
   （**越权与不存在同一个回答**，不暴露存在性）。`Some` → `fetch` 三态映射
   `AssetFetch::{DocRender→"doc_render", SignedUrl→"signed_url", InlineRef→"inline_ref"}`；
   inline 字节不在 MCP 出，只给指针（字节走 REST `GET /v1/asset/{cid}`）。
3. 远端档：`POST /v1/assets/resolve` `{"ids":[cid]}` → 见 §4.6 映射表。

### 4.5 远端模式：怎么知道 server 的能力（KB-0.2 核心问题 1）

**结论：唯一可信来源是 `GET /v1/collections` 的 `server` 对象；`/openapi.json` 不是。**

读代码得到的事实（`crates/fastsearch-server/src/lib.rs`）：
- `server_vector_info` 由 `create_collection` / `get_collection` / `list_collections` 三处返回，
  字段为 `vector_backend` / `vector_dim` / `vector_count` / **`embedded`** /
  `source_of_truth` / `rebuildable_from_source`，且**取自运行中的引擎**（`has_pg_vector`、
  `vector_dim`、`vector_len`、`source_pg_clone`）与 `ServerState.embedder`——是**实例事实**。
- `openapi_spec()`（`GET /openapi.json`）是**手写的静态契约**，只随 crate 版本变，
  免认证、不反映本实例配了什么 ⇒ **不能**用来判断能力。它只适合做一次版本/端点存在性的兼容自检。
- `list_collections` 走 `require_principal` ⇒ **探测顺带验了 key**：401 就是"key 错/没配"，
  可以在启动时给出可自纠的错误，而不是等 agent 第一次调用才炸。

**能力 → schema 的映射（C1 的远端半边）**：

| 探测结果 | `mode.enum` | `mode.default` | 依据 |
|---|---|---|---|
| `embedded: true` | `["keyword","vector","hybrid"]` | `"hybrid"` | server 的 `search_request` 在 `mode∈{Hybrid,Vector}`、`vector==None`、`query_image==None`、`embedder.is_some()` 时**锁外嵌 query** 塞进 `req.vector` ⇒ 三档真能兑现 |
| `embedded: false` | `["keyword"]` | `"keyword"` | 与本地档同因同果：没人替我们算查询向量 |
| 探测失败（连不上 / 401 / 非法 JSON） | — | — | **拒绝启动**，见下 |

**探测时机与失败处理**：
- 在 `RemoteBackend::connect` 里做**一次**，结果存进 `ServerCaps` 后**不可变**——`tool_defs()`
  必须是纯的、确定的（不变量 #4），不能每次 `tools/list` 都联网抖动。
- 失败 → **拒绝启动（fail-closed）**，stderr 打出可粘贴的修复提示（server 在跑吗 / key 对吗 /
  `--server` 指对了吗）。理由：连不上时本进程什么也干不了，"降级成只宣称 keyword"反而制造
  "工具列出来了、一调就全错"的假象；与 server 的 `FASTSEARCH_KEYS` fail-closed 同一条原则。
  > 备选（不采纳，记录在案）：探测失败时退回本地档。不采纳是因为它会在用户以为在查远端库时
  > 悄悄查了另一份本地索引——比报错坏得多。
- server 后来才配上 embedder（或反之）⇒ 本进程的 schema **会过期**，须重启 MCP。
  MCP 协议有 `notifications/tools/list_changed`，但本 crate 当前不发任何通知、
  客户端支持度也未核实 ⇒ **`[待验证]`**，不进 KB-0.2 范围；在 `description` 里如实写明
  "能力在本进程启动时确定"。

### 4.6 远端模式：端点映射

| MCP 工具 | 端点 | 备注 |
|---|---|---|
| `search` | `POST /v1/search` | body = 允许清单字段构成的 `SearchRequest` JSON。**绝不带** `query_image`（server 的 `decode_search_value` 对 body 里的 `query_image` 直接 400：`"query_image is internal; use query_image_base64 or multipart image"`） |
| `resolve_citation` | `POST /v1/assets/resolve` `{"ids":[cid]}` | 返回 `{"assets":[…]}`；**越权/不存在的 id 被省略**（`assets_resolve` 里 `continue`）⇒ 空数组 → 映射成 `{"found":false,"reason":"not found or not authorized"}`，与本地档字面一致 |

`assets_resolve` 的三种 item → MCP `fetch`：
- `{"type":"doc_render", doc_id, page, bbox, media_type}` → `{"kind":"doc_render", …}`（本地档同形）。
- `{"type":"object", url, expires_s}` → `{"kind":"signed_url", url, expires_s}`。
- `{"type":"inline", url, expires_s, media_type}` → **本地档没有对应形状**：本地是
  `{"kind":"inline_ref"}`（只给指针），远端已经把签名 URL 给出来了。
  ⇒ 取"远端更有用"的一侧：`{"kind":"inline_ref", "url":…, "expires_s":…}`，
  本地档保持无 `url`（它没有签名器，也不该有）。**这是两种模式唯一允许的出参差异**，
  必须写进工具 `description`。
- `{"type":"inline", error:"asset signing not configured"}`（server 未配 `FASTSEARCH_ASSET_SIGNING_KEY`）
  → `isError:true` + 可自纠文本（"server 未配置资产签名密钥，取字节请走 `GET /v1/asset/{cid}`
  或让运维设置 `FASTSEARCH_ASSET_SIGNING_KEY`"）。

**已收口契约**：`assets_resolve` 的三个分支均返回 `time`，远端档与本地档
`resolve_citation` 在音视频定位上保持一致；server 单测钉住批量与单资产端点的对齐。

**collection 作用域**：MCP 不设 `collection` 入参，作用域走 `filter` 的 `Eq("collection", …)`
（与 CLI 的 `build_filter` 同惯例）。远端档可用探测到的 `collections` 名单充实工具
`description`（KB-0.5）——但必须如实标注**这是咨询性的内存 registry**：ADR《职责边界》
§"不豁免" 第 4 条已把它定为"必须写进对外契约的 caveat"（多副本各持一份、非真源、
只列本 tenant 名下已 `POST /v1/collections` 注册过的名字）。⇒ 措辞只能是
"**已注册**的集合（可能不全）"，不得说成"本库全部集合"。

### 4.7 入参允许清单：收掉 `query_image` 与所有暗门（KB-0.2 核心问题 4）

**KB-0.1 遗留的确切事实**（读 `tool_search` + `Engine::run` 得到，非推测）：
1. `tool_search` 把整个 `arguments` 反序列化成 `SearchRequest` ⇒ `query_image`（`Option<Vec<u8>>`，
   无自定义 serde，即 JSON 的**数字数组**）可以硬传，而 schema 从未宣称它。
2. schema 宣称的默认 mode 是 `keyword`，而 `run()` 里 `want_vec` 要求
   `mode ∈ {Vector, Hybrid}` ⇒ **`keyword` + `query_image` 时向量路根本不启用，图片被完全忽略**，
   没有任何提示。
3. 就算显式 `mode=vector`：`reject_unavailable_mode` 只在 `query_image.is_some() && has_embedder()`
   时放行；引擎无 embedder 时 `embed_query_image` 返回 `None`（不报错）⇒ 静默退化。
   （`caps.image=false` / `caps.cross_modal=false` 的后端**会**报错，这两条是好的。）

**规约**：
- `search` 只接受**本实例 schema 里宣称的字段**：`query`、`mode`、`top_k`、`filter`、
  `highlight`、`vector`、`include_text`、`max_context_chars`。其余一律拒绝，`isError:true` +
  文本列出接受的字段名。
  - **`vector` 必须补进 schema**：它今天真能用（有测试 `caller_supplied_vector_is_not_rejected`
    背书）却没宣称——这是 C1 反向的另一处违例，一并收。
  - 明确落在拒绝侧、并在错误文本里点名的：`query_image`、`search_after`、`facets`、`collapse`、
    `auto_merge`、`rerank`、`explain`、`candidates`、`ef_search`、`embedder`、`fusion`、
    `include_metadata`。理由各自成立且要写进错误文本，例如
    `search_after` 无意义（本面命中不吐 `cursor`）。`include_text` 已与
    `max_context_chars` 成对落地，避免只开放全文却不给上下文刹车。
- **`query_image` 两种模式下都明确拒绝**（KB-0.2 的范围内答案）：
  - 远端档：server 本来就 400 拒绝 body 里的 `query_image`；MCP 提前拒绝并给出改法
    （"以图搜图请走 REST `POST /v1/search` 的 `query_image_base64` 或 multipart"）。
  - 本地档：改掉"静默不嵌"，同样拒绝。
  - **什么时候能真正宣称以图搜图**：远端档需要知道 server 的 `caps.image` / `caps.cross_modal`，
    而 `server_vector_info` 今天只吐一个 `embedded` 布尔 ⇒ 依赖 **KB-2.4 Embedder 能力探测**
    把实测 caps 放进 introspection，届时才可以在 schema 里如实加 `query_image_base64`。
    在那之前，"不宣称 + 明确拒绝 + 指路"就是 C1 下唯一诚实的做法。
- 允许清单是**唯一**扩展点：以后任何新入参（如 metadata 预算或新写入工具字段）
  都必须**同时**改 schema 与清单，测试用例 19 会盯住"两者一致"。

### 4.8 确定性与健壮性

- `tool_defs()` 是纯函数（本地档看 `has_embedder` 之类的实例常量，远端档看不可变的 `ServerCaps`）
  ⇒ 同一进程内多次 `tools/list` 逐字节相同（不变量 #4）。
- 命中顺序的确定性由 engine 保证（同分按 `GlobalId` 升序 tie-break）；MCP 只做投影，不重排。
- `top_k`/`candidates` 的上界由 `SearchRequest::validate`（`MAX_TOP_K` / `MAX_CANDIDATES`）在
  engine 侧兜住；远端档由 server 同样调用 `run()` 兜住。MCP 不自行设第二套上界。
- 远端档的 HTTP 超时必须显式设（CLI 的 `FASTSEARCH_TIMEOUT_SECS`，默认 30s，理由同 M26：
  `ureq` 默认无读超时，server 挂起会让 MCP 永久阻塞——对 stdio 单线程循环意味着**整个工具面死锁**）。
- 远端档非 2xx → `isError:true` + 带状态码与 body 的文本（照搬 CLI `PostError::into_anyhow`
  的措辞风格："server 返回 {code}: {body}" / "请求失败…（server 在运行吗？）"）。
- **`/v1/search` 的 401 要单独说人话**：探测时 key 是好的、后来 401，多半是 key 被轮换/撤销
  ⇒ 文本里写明"API key 已失效，请更新 `FASTSEARCH_KEY` 并重启 MCP"。

### 4.9 诚实记账：远端档 `hybrid` 的成色

`embedded: true` 只证明 **server 配了嵌入后端、会替我们算查询向量**，**不**证明这个向量是语义的
——`fastsearch-embed` 的 `HashEmbedder` 基线也是一个 `Embedder`，而 `server_vector_info`
没有暴露 `caps().semantic`。⇒ 远端档 `description` 的措辞必须是
"本实例的 `hybrid` 由 server 侧嵌入后端提供"，**不得**写成"语义检索"。
真正的实测 caps 等 KB-2.4。**这是本 spec 主动接受的一处 C1 弱化，记在 §8 已知限制里**。

同源的第二处：server 的 `search_request` 在 `filter_targets_image(...)==true`
且 `embedder.caps().cross_modal==false` 时**跳过 query 嵌入**（`cross_modal_ok`），
于是那一类查询在 server 侧静默退化成纯 keyword。这是 server 那张脸的既有行为，
远端档会原样继承 ⇒ 记入已知限制并**指向 server 侧修复**（本 spec 不改 server 行为）。

---

## 5. 依赖

- 现状：`fastsearch-core`、`fastsearch-engine`、`fastsearch-text`、`serde`、`serde_json`、`anyhow`、
  `ureq.workspace = true`；dev 侧无额外 HTTP 测试依赖
  （mock HTTP 用 `std::net::TcpListener`，照抄 CLI `crates/fastsearch-cli/src/tests.rs` 的
  `spawn_mock` / `spawn_capture` / `drain_request` 形状）。
- **不新增**：`fastsearch-embed`（KB-0.1 §10 已决不接）、`tokio`/`axum`（stdio 同步循环足够）。
- **不变量 #7（搜索热路径零 docparse/ONNX）**：`.github/workflows/ci.yml` 的 `hot-path-isolation`
  job 已把 `fastsearch-mcp` 列入断言名单。`ureq` 与 docparse 无关，**该门禁不受影响**——
  远端模式落地后已重跑 `cargo tree -p fastsearch-mcp -e normal` 并纳入 CI 门禁。
- 本地档保留 `engine`/`text` 依赖；远端档在同一 crate 内**不使用**它们（编译期仍在——
  拆 feature 会让 `Cargo.toml` 长出两套构建档，收益不抵复杂度，**不做**，理由记在 §8）。

---

## 6. 测试用例

单测在 `crates/fastsearch-mcp/src/lib.rs` 的 `mod tests`；本地档与远端档共用同一套协议测试面。

**现状已有（KB-0.1 后）**：
1. `initialize_and_tools_list` — 握手版本 + 两个工具都在。
2. `notification_has_no_response` — `notifications/*` 无响应。
3. `unknown_method_errors` — `-32601`。
4. `tools_call_search_returns_hits` — keyword 检索命中 + `citation_id` 形状。
5. `tools_list_advertises_only_real_capability` — **C1 正向**：`enum==["keyword"]`、
   `default=="keyword"`、description 含限制说明。
6. `omitted_mode_runs_keyword_not_silent_hybrid` — 省略 mode 不掉进静默退化。
7. `explicit_unavailable_mode_errors_with_actionable_text` — `hybrid`/`vector` → `isError:true`
   且文本含可执行改法；**不是**协议 error。
8. `caller_supplied_vector_is_not_rejected` — 自带向量不被守卫误伤。
9. `tools_call_bad_args_is_tool_error_not_protocol_error` — 缺 `query` → 工具级错误，非协议 error。
10. `tools_call_resolve_citation_no_media` — 无媒资 chunk → `found:false`，非协议错误。

**KB-0.2 必须新增**（编号接着写，名字是建议）：

11. `acl_not_bypassable_local`（**越权用例·本地档**，对标 server 的 `acl_not_bypassable`）：
    同 tenant `acme`、acl 分别 `["team-a"]`/`["team-b"]` 两条 chunk；
    `McpServer::new(engine, Some(AclFilter{tenant:Some("acme"), allowed_tags:["team-a"]}))`；
    - `tools/call search` → 命中只含 team-a 的 `citation_id`；
    - `arguments` 里塞 `"tenant":"other"`、`"acl":["team-b"]`、`"allowed_tags":["team-b"]`
      → **结果集逐字节相同**（伪造字段不改变可见集）；
    - `resolve_citation` team-b 的 cid → `{"found":false}`（与"不存在"同一回答）。
12. `local_fail_closed_without_acl_config`（§4.3-4）：未设 `FASTSEARCH_MCP_TENANT` 且未设
    `FASTSEARCH_MCP_ACL=all` → 装配失败并返回含改法的错误（把 main 的装配逻辑提成可测函数，
    如 `resolve_local_acl(env) -> Result<Option<AclFilter>>`，纯函数单测；避免在测试里改进程 env）。
13. `remote_and_local_acl_env_conflict_rejected`（§4.3-2）：同时给 `--server` 与
    `FASTSEARCH_MCP_TENANT` → 拒绝启动 + 二选一的改法文本。
14. `remote_probe_maps_caps_to_schema`（**C1 远端半边**）：mock server 对
    `GET /v1/collections` 返回 `{"collections":["kb"],"server":{"embedded":true,…}}`
    → `tool_defs()` 的 `enum==["keyword","vector","hybrid"]`、`default=="hybrid"`；
    换成 `"embedded":false` → 退回 `["keyword"]`/`"keyword"`。
15. `remote_probe_failure_refuses_to_start`：mock 返回 401 / 连接被拒 →
    `RemoteBackend::connect` 返回 `Err`，文本含"key/`--server`/server 是否在跑"三点改法；
    **且不得回退本地档**。
16. `remote_search_sends_no_acl_and_carries_bearer`（**越权用例·远端档，最关键的一条**）：
    用 `spawn_capture` 截获出站请求 →
    - 请求头含 `authorization: Bearer <key>`；
    - body 的 JSON **不含** `tenant`/`acl`/`allowed_tags` 任何键（即使 `arguments` 里塞了）；
    - body 不含 `query_image`（§4.7）。
17. `remote_hit_projection_matches_local_shape`：mock 返回 server 富命中（含 `bm25`/`cursor`/
    `media`/`bbox`）→ MCP 默认输出只有 §3.1 的五个基础键，且 `snippet` 取自 `highlight`；
    `include_text` 与预算测试另行钉住可选字段。
18. `remote_resolve_maps_empty_to_found_false`：mock `/v1/assets/resolve` 返回 `{"assets":[]}`
    → `{"found":false,"reason":"not found or not authorized"}`；返回 `inline` 且带 `error`
    → `isError:true` + 改法文本。
19. `unadvertised_arg_is_rejected`（§4.7，**C1 反向**）：`query_image`、
    `collapse`、`facets` 各一次 → `isError:true`，文本列出接受字段；
    并断言 **schema 宣称的属性集合 == 允许清单**（一个从 `tool_defs()` 反读 `properties` 的断言，
    防止将来两边漂移）。
20. `schema_is_deterministic`：同一实例连续两次 `tools/list` 序列化后逐字节相等（不变量 #4）。

**端到端（CI job，对标 CLI 的 `cli-server-e2e`）**——这条是 KB-0.2 验收的硬要求，
因为"两个不同 key 看到不同可见集"只有真 server 才能证：

21. `mcp-server-e2e`：起 `fastsearch-server`
    （`FASTSEARCH_KEYS="a=acme:team-a; b=acme:team-b"`）→
    起两个以 `FASTSEARCH_SERVER`/`FASTSEARCH_KEY` 连接的 `fastsearch-mcp`，各自经 stdio JSON-RPC
    `index_chunks` 写入不同 ACL 的 chunk，再验证：
    - 同一 query，两个实例**各自只看到自己那条**；
    - key `a` 的实例 `resolve_citation` key `b` 那条 → `found:false`；
    - key `a` 的实例 `resolve_citation` 自己那条 → `found:true` 且 doc-render 位置正确；
    - `tools/list` 对真二进制无 embedder server 宣称 `mode.enum=["keyword"]`；
      `embedded=true` 到三档的映射由用例 14 的 mock 能力探测单测覆盖，不冒充真二进制证据；
    - 同一 query 经 MCP 与经 `curl POST /v1/search` 的 `citation_id` 序列**完全一致**
      （KB-0.2 验收原文："结果与直连 REST 一致"）。
22. 本地档不回归：现有 1–9 全绿 + 实跑本地 `fastsearch-mcp` 二进制的握手/`tools/list`/检索冒烟
    （KB-0.1 已跑过一次，KB-0.2 后重跑）。

---

## 7. 验收标准

**本地档（KB-0.1，已达成）**
- [x] 四张脸之 MCP：`initialize`/`ping`/`tools/list`/`tools/call` + 通知无响应 + 协议错误码。
- [x] ACL 由 `McpServer` 持有并强制传入 `engine.search`/`resolve_citation`，工具入参不可传。
- [x] **C1 正向**：不能产出查询向量的实例，`mode.enum` 只有 `keyword`、`default` 为 `keyword`、
      description 如实说明；显式 `hybrid`/`vector` → `isError:true` + 可自纠文本；keyword 零回归。
- [x] 收口三绿 + 活服务验证（2026-08-24，见 §8 迭代记录）。

**远端档（KB-0.2/0.3，已实施并纳入 CI）**
- [x] `--server/--key`（env `FASTSEARCH_SERVER`/`FASTSEARCH_KEY`）可用，与本地档并存且**互斥**；
      两者同时指定（或远端档下还设了 `FASTSEARCH_MCP_TENANT`）→ 拒绝启动 + 二选一改法。
- [x] 启动时探测 `GET /v1/collections`；`embedded:true` ⇒ 三档 + default `hybrid`，
      `false` ⇒ 单档 keyword；探测失败 ⇒ 拒绝启动，**绝不回退本地档**。
- [x] 真二进制中 MCP `search` 的 citation 序列与直连 REST 一致；
      `resolve_citation` 同时覆盖授权正向与跨 key 拒绝（用例 21）。
- [x] 两个不同 key 的 MCP 实例看到不同可见集（用例 21）；出站请求不含任何 ACL 字段（用例 16）。
- [x] 本地档 fail-closed 收紧（用例 12），且破坏性变更已写进中英文 Agent 使用指南。
- [x] 未宣称入参一律拒绝，`query_image` 两档都给可自纠错误（用例 19）；schema 属性集 == 允许清单。
- [x] 收口三绿（`cargo fmt --all --check` + `cargo clippy --workspace --all-targets -- -D warnings`
      + `cargo test --workspace`）+ **实跑两个二进制**的活服务验证；未跑前一律标 `待运行验证`。
- [x] `cargo tree -p fastsearch-mcp -e normal` 仍零 docparse（不变量 #7）。

---

## 8. 状态 / 迭代记录 / 已知限制 / 下一迭代

### 迭代记录

- **2026-08-31 · FS-003 真二进制 CI 门禁**：新增 `mcp-server-e2e` job，起真
  `fastsearch-server` 和两个远端档 `fastsearch-mcp` stdio 进程，由 MCP 分别写入两个 ACL 集合，
  验证 initialize/tools/list、`index_chunks`、`search`、跨 key `resolve_citation` 不可见、两 key 搜索隔离及
  MCP/REST citation 序列一致。脚本还钉住 `/readyz` 的进程级语义。

- **2026-08-26 · KB-0.4/0.5 已实施 + 活服务验证**：`include_text` + `max_context_chars`
  同时进 schema 与允许清单（只放开前者不给预算 = 把冲爆上下文的开关递给 agent 却不给刹车）；
  `apply_budget` 按既有顺序前向累加、放不下时留够 `MIN_HIT_CHARS`(80) 就截断并标 `text_truncated`
  否则整条丢弃，响应报 `dropped`/`context_chars`；**不设预算时响应形状逐字节不变**。
  旋钮按**字符**而非 token——本面没有分词器，估算 token 是编造的数字（诚实记账）。
  KB-0.5：描述带本实例作用域（filter 字段 + 远端档探到的集合名单，**带"可能不全"caveat**；
  本地档没有该信息就什么都不说）。测试 +5。

- **2026-08-25 · KB-0.3 写入工具 `index_chunks` 已实施 + 活服务验证**：仅远端档宣称且仅远端档可调
  （C1 全表版）；入参允许清单 `INDEX_ARGS` 与 schema 由 `index_schema_and_allowlist_agree` 钉住；
  chunk 夹带 `tenant`/`acl` **显式拒绝**（不让 server 静默覆盖）；MCP 替调用方把顶层 `doc_id`
  补进每条 chunk（`Chunk.doc_id` 必填 + `IndexChunk` 是 flatten ⇒ 缺了在反序列化阶段就 422，
  `apply_ingest_identity` 的覆盖发生在那之后救不了——**活服务验证抓到，单测的 mock 不校验请求体故漏掉**）。
  测试 +4；实跑闭环：agent 写入 → `{"indexed":1}` → 立即 `search` 命中该 `citation_id`。
  `ingest_document`（收原始文件）未做，等 KB-3 上传端点。

- **2026-08-25 · KB-0.2 远端模式已实施 + 活服务验证**：`Backend::{Local,Remote}`、
  `RemoteBackend::connect`（启动时 `GET /v1/collections` 探一次、顺带验 key、失败即拒启）、
  `ServerCaps`、入参允许清单 `SEARCH_ARGS`、远端命中投影 `project_remote_hit`、
  `remote_resolve`；main 加两档选择 + 冲突拒绝 + 本地档 fail-closed。
  测试 +7（含 mock server 的探测/出站无 ACL/投影形状/探测失败拒启）；
  实跑验证：远端档探到 `embedded=false backend=brute` ⇒ schema 如实只列 keyword；
  检索经 REST 返回投影后的五字段；`query_image` 被允许清单拒绝；两条拒绝启动均生效。
  **实跑抓到一个单测没抓到的字段名错误**：探测读的是 `server.backend`，而
  `server_vector_info` 吐的是 `vector_backend` ⇒ 静默变成 `"unknown"`。已修，
  并把真实字段名写进 mock 响应 + 加断言钉死（这正是"活服务验证"这一步的价值）。

- **v1.0（2026-06-26）**：第四张脸落地（stdio + JSON-RPC，`search`/`resolve_citation`，
  ACL 服务端注入）。6 个 dispatch 单测 + 真 stdio 冒烟。见
  [迭代循环](../plans/2026-06-25-迭代循环.md) 的 MCP 行。
- **v1.1（2026-08-24，KB-0.1 能力诚实化，活服务验证 done）**：`tool_defs` 由 `pub fn` 改为
  `McpServer` 方法，`mode` 的 enum/default/描述由 `search_modes()`→`can_embed_text_query()` 推导；
  新增 `reject_unavailable_mode()`；对齐"schema 默认 keyword"与"`SearchRequest` serde 默认 Hybrid"
  两个 default。engine 侧加 `Engine::has_embedder()`。+4 测试；收口三绿（`cargo test --workspace`
  349 passed）。活服务验证：实跑二进制 `tools/list` 得 `{"enum":["keyword"],"default":"keyword"}`、
  显式 `hybrid` 报三条改法、省略 mode 正常命中。详见
  [迭代计划 §2.5 KB-0.1](../plans/2026-08-24-知识库引擎迭代计划.md)。
- **v1.2（本文，2026-08-25）**：补记本 spec（此前本模块无 spec）+ 落 KB-0.2 远端模式设计。
  **文档-only，未改任何代码**。

### 已知限制（现状为准）

1. **本地档只有 keyword**：MCP 直连引擎，而 `Engine::run()` 从不嵌文本 query。语义/混合需
   自带 `vector` 或使用已实施的远端档。
2. **本地档 ACL 是进程级常量**：一个进程一个租户，多租户场景用不了；启动时必须
   显式给出 `FASTSEARCH_MCP_TENANT` 或 `FASTSEARCH_MCP_ACL=all`，否则 fail-closed 拒绝启动。
3. **C1 反向已闭合**：schema、允许清单和共享字段矩阵一致；未宣称的 REST 搜索字段均显式拒绝。
4. **远端档的 `hybrid` 只诚实到"server 会算查询向量"这一层**：`server_vector_info` 不吐
   `caps().semantic`/`image`/`cross_modal` ⇒ 无法证明它是语义的、能收图。等 **KB-2.4**。
5. **继承 server 的一处静默退化**：`filter_targets_image` 命中且后端 `caps.cross_modal==false`
   时 server 跳过 query 嵌入 ⇒ 那类查询实际是纯 keyword。修在 server 侧，不在本面。
6. **schema 在进程启动时定死**：server 能力后来变了要重启 MCP；`tools/list_changed` 通知
   **`[待验证]`**（本 crate 不发通知，客户端支持度未核实）。
7. **只有 stdio 传输**；MCP 的 HTTP/SSE 传输未做。
8. **远端档不拆 feature**：`engine`/`text` 依赖在远端档下也编译进来（二进制偏大）。
   拆 feature 会长出两套构建档 + 两条 CI 路径，收益不抵复杂度 ⇒ 明确不做，记在此处。

### 下一迭代

- ~~**KB-0.2**：按本文 §4.5–§4.9 实施远端模式（C1 通道，Wave 2）。~~
  **2026-08-25 已实施，2026-08-31 已纳入真二进制 CI。**
- ~~**KB-0.3**：写入工具 `index_chunks`~~ —— **2026-08-25 已实施**（见迭代记录）。
  `ingest_document` 仍待 KB-3 上传端点。原条目：**依赖远端档**——本地档没有每请求身份，
  在它上面开写入口等于让引擎"替调用方猜写入 ACL"，正是 server v2.4 用 403 拒绝掉的那件事
  ⇒ 按 §4.2 的 C1 全表版：**写入工具只在远端档的 `tools/list` 里出现**。
- ~~**KB-0.4** / **KB-0.5**~~ —— **2026-08-26 已实施**（见迭代记录）。
  余下：SDK 侧（TS/Python）的对应预算旋钮；`include_metadata` 仍在拒绝侧（要连同 metadata
  的预算语义一起想）。
- **KB-2.4 之后**：introspection 吐实测 caps ⇒ 才可以诚实地宣称 `query_image_base64`（以图搜图）。

### 待决策

- **`[已决 2026-08-25 · 迭代计划 §10 #2]` 保留本地嵌引擎档。**

  **实施结论：保留，但降级为"必须显式选择的单机档"，并按 §4.3-4 收紧 fail-closed。**

  理由：
  1. **弃用等于产品失去唯一的离线入口**。CLI 已于 2026-06-28 改成纯 REST 客户端，
     其 spec 的已知限制第一条就是"**CLI 不再离线**"。MCP 本地档是四张脸里最后一个
     "单二进制 + 一个索引目录就能跑"的形态，而"外部单二进制"正是 CLAUDE.md 里的定位原话。
  2. **两条 ACL 注入路径的成本比看上去低**：两条路最终都收敛到**同一个接缝**
     `engine.search(&req, Some(&acl))` / `resolve_citation(cid, Some(&acl))`，
     差别只在 `AclFilter` 由谁构造（本地：进程 env；远端：server 的 `acl_for`）。
     不变量 #3 的真正判据是"**工具入参不能影响 ACL**"，这一条对两条路是**同一个测试形状**
     （用例 11 与用例 16/21 是同一个断言的两种落地）。
  3. **风险已关闭**：本地档历史上的真问题不是"有两条路"，而是它那条路**fail-open**。
     §4.3-4 收紧后，两条路都是 fail-closed，风险面已收敛。
  4. **弃用的正当理由只有一个**，若成立则应改判：KB-0.3 写入工具落地后，本地档会变成
     "能写但没有身份"的形态。本 spec 的应对是**本地档保持只读**（写入工具不在其 `tools/list` 里）。
     如果将来出现"本地档也必须能写"的真实需求，那时**弃用本地档**优于"给它编一个写入身份"。

  ⇒ **决策条件写死**：只要本地档保持 ①fail-closed、②只读、③schema 如实只宣称 keyword，
  就保留；三条中任何一条破了，就该弃用而不是打补丁。KB-0.2 实施者已将结论回写在下方。

  > **`[已决 2026-08-25 · KB-0.2 实施者裁定]` 采纳本 spec 的建议：保留，降级为"必须显式选择的单机档"。**
  > 三条决策条件在本次实施后**均已成立且有测试背书**：
  > ① fail-closed —— `local_acl()` 未显式声明可见范围即拒绝启动（实跑验证过）；
  > ② 只读 —— KB-0.3 的写入工具按 C1 全表版只进远端档的 `tools/list`；
  > ③ 如实只宣称 keyword —— `search_modes()` 由 `can_embed_text_query()` 推导，
  > 本地档恒 `false`（`tools_list_advertises_only_real_capability`）。
  > **复审触发**：三条中任何一条被破（尤其"本地档也要能写"），按本节第 4 条改判为弃用，
  > 而不是给它编一个写入身份。

- **`[待决策]` 远端档探测失败时的策略**：本文取"拒绝启动"（与 server `FASTSEARCH_KEYS`
  fail-closed 同构）。备选"降级为只宣称 keyword"已在 §4.5 论证不采纳，但若实施中发现
  MCP 客户端对"启动即退出"的处理很糟（如无限重启风暴），可改为"启动但 `tools/list` 返回空表
  + 每次调用给可自纠错误"——**不得**改成静默回退本地档。

- **`[待验证]`** `notifications/tools/list_changed` 的客户端支持度与本 crate 的通知发送能力
  （本 crate 目前只对 `notifications/*` **收**、从不**发**）。

---

## 9. 跨文件实施清单（已完成）

以下是远端档设计推导出的跨文件改动，现均已落地：

1. [x] `assets_resolve` 三个分支补齐 `time`，并同步 OpenAPI/server spec。
2. [x] `docs/specs/00-模块拆分.md` 已建立本 spec 索引。
3. [x] 中英文 Agent 使用指南已写明远端档与本地档 fail-closed 破坏性变更。
4. [x] `crates/fastsearch-mcp/Cargo.toml` 已使用 `ureq.workspace = true`。
5. [x] `.github/workflows/ci.yml` 已新增 `mcp-server-e2e` job（用例 21）。
