//! # fastsearch-mcp
//!
//! 第四张脸：**MCP（Model Context Protocol）服务**，stdio + JSON-RPC 2.0，把混合检索暴露为
//! LLM 可直接调用的工具（`search` / `resolve_citation`）。薄适配引擎，逻辑在 lib（纯、可单测），
//! main 只是 stdio 收发壳。
//!
//! **ACL 不可绕过（守不变量 #3）**：principal/ACL 由**服务端配置**注入 `engine.search`/
//! `resolve_citation`，MCP 客户端（LLM）的工具入参里**不接受也无法放宽** ACL——与 REST 一致。
//!
//! 协议：`initialize` 握手 → `tools/list` 列工具 → `tools/call` 调用；`ping` 存活；
//! 通知（`notifications/*`）无响应。详见架构大图"四张脸"。

use fastsearch_core::{AclFilter, SearchMode, SearchRequest};
use fastsearch_engine::{AssetFetch, Engine};
use serde_json::{json, Value};

/// 支持的 MCP 协议版本（与主流客户端对齐）。
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// 从 `GET /v1/collections` 的 `server` 对象（server 侧 `server_vector_info`）读到的**实测**运行档。
///
/// **为什么只认这个来源**：`/openapi.json` 是手写静态契约、免认证、只随 crate 版本变，
/// 不反映本实例配了什么（见 [22-mcp spec §4.5](../../../docs/specs/22-mcp.md)）。
#[derive(Debug, Clone, Default)]
pub struct ServerCaps {
    /// server 是否配了嵌入后端 = 它会在 `/v1/search` 里替我们把文本 query 嵌成向量。
    ///
    /// **诚实记账**：`true` 只证明"配了嵌入后端"，**不**证明该向量是语义的
    /// （`HashEmbedder` 基线也是一个 `Embedder`，而 `server_vector_info` 不吐 `caps().semantic`）。
    /// 故描述措辞只能是"由 server 侧嵌入后端提供"，不得写成"语义检索"。等 KB-2.4 的实测 caps。
    pub embedded: bool,
    pub vector_backend: String,
    pub source_of_truth: String,
    /// 本 key 名下**已注册**的集合名。**咨询性**：server 的 collection registry 是进程内
    /// HashMap、非真源、多副本各持一份（ADR《职责边界》已定为"必须写进对外契约的 caveat"）。
    pub collections: Vec<String>,
}

/// 远端后端：server 的瘦 REST 客户端 + **启动时探到的能力**（不可变，供 schema 生成）。
///
/// **本结构刻意不持有 `AclFilter`**：远端档的身份是那把 API key，ACL 由 server 的
/// `principal_from_headers` → `acl_for` 注入。MCP 再持一份只会产生"两处判权且可能不一致"
/// 的第二真源（spec §4.3-2/3）。
pub struct RemoteBackend {
    base: String,
    key: String,
    agent: ureq::Agent,
    caps: ServerCaps,
}

impl RemoteBackend {
    /// 连接并**必做一次能力探测**；探测失败 → `Err`（fail-closed，不猜、不静默回退本地档）。
    ///
    /// 配置优先级：显式参数 > env（`FASTSEARCH_SERVER` / `FASTSEARCH_KEY`）> `http://localhost:8642`。
    /// 超时必须显式设：`ureq` 默认无读超时，server 挂起会让 stdio 单线程循环**整个工具面死锁**。
    pub fn connect(server: Option<String>, key: Option<String>) -> anyhow::Result<Self> {
        let base = server
            .or_else(|| std::env::var("FASTSEARCH_SERVER").ok())
            .unwrap_or_else(|| "http://localhost:8642".into())
            .trim_end_matches('/')
            .to_string();
        let key = key
            .or_else(|| std::env::var("FASTSEARCH_KEY").ok())
            .unwrap_or_default();
        let timeout = std::time::Duration::from_secs(
            std::env::var("FASTSEARCH_TIMEOUT_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(30),
        );
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(timeout)
            .timeout_read(timeout)
            .timeout_write(timeout)
            .build();
        let caps = probe_caps(&agent, &base, &key)?;
        Ok(RemoteBackend {
            base,
            key,
            agent,
            caps,
        })
    }

    pub fn caps(&self) -> &ServerCaps {
        &self.caps
    }

    /// POST 一个 JSON body，成功返回解析后的 JSON；非 2xx / 传输失败都变成**给 agent 看的**文本。
    fn post(&self, path: &str, body: &Value) -> Result<Value, String> {
        let url = format!("{}{path}", self.base);
        match self
            .agent
            .post(&url)
            .set("authorization", &format!("Bearer {}", self.key))
            .send_json(body.clone())
        {
            Ok(r) => r
                .into_json::<Value>()
                .map_err(|e| format!("server 响应不是合法 JSON：{e}")),
            Err(ureq::Error::Status(401, _)) => {
                Err("server 拒绝了 API key（401）。key 可能已被轮换或撤销：\
                 请更新 FASTSEARCH_KEY 并重启 MCP。"
                    .into())
            }
            Err(ureq::Error::Status(code, r)) => Err(format!(
                "server 返回 {code}: {}",
                r.into_string().unwrap_or_default()
            )),
            Err(e) => Err(format!(
                "请求 {url} 失败：{e}（server 在运行吗？检查 FASTSEARCH_SERVER）"
            )),
        }
    }
}

/// 能力探测：`GET /v1/collections`。顺带**验 key**——401 在启动期就报，
/// 而不是等 agent 第一次调用工具才发现（`list_collections` 走 `require_principal`）。
fn probe_caps(agent: &ureq::Agent, base: &str, key: &str) -> anyhow::Result<ServerCaps> {
    let url = format!("{base}/v1/collections");
    let resp = agent
        .get(&url)
        .set("authorization", &format!("Bearer {key}"))
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(401, _) => anyhow::anyhow!(
                "能力探测被拒（401）：API key 无效。设 FASTSEARCH_KEY 或 --key 为 server \
                 FASTSEARCH_KEYS 里配置过的 key。"
            ),
            other => anyhow::anyhow!(
                "能力探测失败（GET {url}）：{other}。\
                 远端档拒绝在不知道 server 能力的情况下启动——否则只能靠猜生成 tool schema，\
                 那正是 KB-0.1 修掉的那类谎。请确认 server 在运行且 FASTSEARCH_SERVER 正确。"
            ),
        })?;
    let v: Value = resp
        .into_json()
        .map_err(|e| anyhow::anyhow!("能力探测响应不是合法 JSON：{e}"))?;
    let srv = v.get("server").cloned().unwrap_or_else(|| json!({}));
    Ok(ServerCaps {
        embedded: srv
            .get("embedded")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        vector_backend: srv
            .get("vector_backend")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
        source_of_truth: srv
            .get("source_of_truth")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
        collections: v
            .get("collections")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|c| {
                        c.as_str()
                            .map(String::from)
                            .or_else(|| c.get("name").and_then(Value::as_str).map(String::from))
                    })
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// 检索后端：本地嵌引擎 或 远端 server 的 REST 客户端。
///
/// **运行模式是后端选择，不是第二套协议**——`handle` 的形状两档一致。
pub enum Backend {
    /// 本地档：进程内 `Engine` + 进程级固定 ACL（部署方给的常量）。
    ///
    /// `Engine` 装箱：它比 `RemoteBackend` 大一个量级，不装箱则整个 `Backend`（连带 `McpServer`）
    /// 都按最大变体分配——远端档白背一份引擎大小的栈/堆布局。
    Local {
        engine: Box<Engine>,
        acl: Option<AclFilter>,
    },
    /// 远端档：server 的纯 REST 客户端，**本进程不持有任何 ACL**。
    Remote(Box<RemoteBackend>),
}

/// MCP 服务：持有一个后端（本地嵌引擎 / 远端 REST）。
pub struct McpServer {
    backend: Backend,
}

impl McpServer {
    /// 本地档（保留原签名，既有调用点与测试不动）。
    pub fn new(engine: Engine, acl: Option<AclFilter>) -> Self {
        McpServer {
            backend: Backend::Local {
                engine: Box::new(engine),
                acl,
            },
        }
    }

    pub fn with_backend(backend: Backend) -> Self {
        McpServer { backend }
    }

    /// 处理一条 JSON-RPC 消息：**请求**返回 `Some(响应)`，**通知**（无 `id`）返回 `None`。
    pub fn handle(&self, msg: &Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        // 通知（如 notifications/initialized）：无 id、无响应。
        if id.is_none() && method.starts_with("notifications/") {
            return None;
        }
        let result = match method {
            "initialize" => Ok(self.initialize_result()),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": self.tool_defs() })),
            "tools/call" => return Some(self.tools_call(id, msg.get("params"))),
            _ => Err((-32601, format!("method not found: {method}"))),
        };
        Some(match result {
            Ok(v) => ok(id, v),
            Err((code, m)) => err(id, code, &m),
        })
    }

    fn initialize_result(&self) -> Value {
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "fastsearch-mcp", "version": env!("CARGO_PKG_VERSION") },
        })
    }

    /// `tools/call`：分派到具体工具。工具内部错误以 `isError:true` 的内容返回（MCP 约定：
    /// 工具执行失败不发协议级 error，便于 LLM 读到失败原因）。
    fn tools_call(&self, id: Option<Value>, params: Option<&Value>) -> Value {
        let Some(params) = params else {
            return err(id, -32602, "missing params");
        };
        let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
        let args = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let outcome = match name {
            "search" => self.tool_search(args),
            "resolve_citation" => self.tool_resolve_citation(args),
            // 本地档不宣称此工具（C1 全表版）⇒ 它在这里也必须真的不可用，
            // 否则"没宣称但能调"就是又一个暗门。
            "index_chunks" => self.tool_index_chunks(args),
            other => Err(format!("unknown tool: {other}")),
        };
        match outcome {
            Ok(text) => ok(id, tool_text(&text, false)),
            // 工具级失败：result + isError，不是协议 error。
            Err(e) => ok(id, tool_text(&e, true)),
        }
    }

    /// 本面能否产出文本查询向量。
    ///
    /// - **本地档恒 `false`**，这是代码事实而非保守估计：`Engine::run()` **从不嵌入文本 query**
    ///   （`embedder` 只服务 `query_image`），文本 query 的向量是 **server** 在 `search_request`
    ///   里算好塞进 `req.vector` 的；MCP 直连引擎、没有那一步。
    /// - **远端档看实测**（KB-0.2）：`ServerCaps.embedded` —— server 配了嵌入后端就会替我们算，
    ///   于是三档都能如实宣称。**这就是"不在 MCP 里养第二套嵌入配置"的兑现方式**。
    fn can_embed_text_query(&self) -> bool {
        match &self.backend {
            Backend::Local { .. } => false,
            Backend::Remote(r) => r.caps.embedded,
        }
    }

    /// 本实例如实宣称的检索模式。**schema 宣称的能力必须等于实际能力**：宣称了做不到的档，
    /// agent 会拿着静默退化的结果当语义检索用（本条正是 KB-0.1 修的 bug）。
    fn search_modes(&self) -> Vec<&'static str> {
        if self.can_embed_text_query() {
            vec!["keyword", "vector", "hybrid"]
        } else {
            vec!["keyword"]
        }
    }

    /// 显式要了本面给不出的档 → **报可自纠的错，而不是静默退化**。
    ///
    /// 引擎的既有语义是"`Hybrid` 无 `req.vector` 时退化为全文、`Vector` 无向量时向量路不启用"——
    /// 对 server 那张脸是合理的（它已经把向量算好了），对**直连引擎且不会算向量**的 MCP 却意味着
    /// agent 拿到全文结果却以为是语义检索。agent 没有人类的试错直觉，只能读错误文本自纠，
    /// 所以这里给出"下一步该怎么改"的指引（KB-0.5 的同一条原则）。
    ///
    /// **放行两条真能走通的路**：调用方自带 `vector`；或带 `query_image` 且引擎配了 embedder
    /// （以图搜图，引擎会自己嵌图）。
    fn reject_unavailable_mode(&self, req: &SearchRequest) -> Result<(), String> {
        if !matches!(req.mode, SearchMode::Vector | SearchMode::Hybrid) {
            return Ok(());
        }
        if req.vector.is_some() {
            return Ok(());
        }
        Err(format!(
            "本 MCP 实例产不出查询向量，mode=\"{}\" 在这里只会静默退化成全文检索，故拒绝。\
             改法（任选其一）：① 用 mode=\"keyword\"（本实例唯一如实支持的档，见 tools/list）；\
             ② 需要语义/混合检索时改走 REST POST /v1/search——server 侧配了嵌入后端；\
             ③ 自行算好查询向量后在入参里带 `vector` 字段。",
            match req.mode {
                SearchMode::Vector => "vector",
                _ => "hybrid",
            }
        ))
    }

    /// 给工具描述补一句**本实例**的作用域提示（KB-0.5）：可用 filter 字段 + 已注册集合。
    ///
    /// **集合名单只在远端档有，且必须带 caveat**：server 的 collection registry 是进程内
    /// HashMap、非真源、多副本各持一份，只列本 tenant 名下**显式注册过**的名字
    /// （ADR《职责边界》已把它定为"必须写进对外契约的 caveat"）⇒ 措辞只能是"已注册（可能不全）"，
    /// 不得说成"本库全部集合"。本地档没有这个信息就什么都不说——**编一个比不说更糟**。
    fn scope_hint(&self) -> String {
        let mut out = String::from(
            "\n作用域：MCP 不设 collection 入参，限定集合请用 filter 的 \
             Eq(\"collection\", \"<名字>\")。可用 filter 字段：collection / doc_id / kind / \
             modality / page（page 支持 Gte/Lte 范围）。",
        );
        if let Backend::Remote(r) = &self.backend {
            if !r.caps.collections.is_empty() {
                out.push_str(&format!(
                    "本 key 名下**已注册**的集合（咨询性、可能不全——registry 是 server 进程内内存态、\
                     非真源）：{}。",
                    r.caps.collections.join(" / ")
                ));
            }
        }
        out
    }

    /// 本实例 `search` 宣称接受的入参名 —— **允许清单**，与 [`Self::tool_defs`] 的
    /// `properties` 必须逐字一致（测试 `schema_and_allowlist_agree` 盯住）。
    const SEARCH_ARGS: [&'static str; 8] = [
        "query",
        "mode",
        "top_k",
        "filter",
        "highlight",
        "vector",
        "include_text",
        "max_context_chars",
    ];

    /// 拒绝一切未宣称的入参 —— 诚实契约 C1 的执行面：**未宣称的能力不得作为暗门存在**。
    ///
    /// 反面教材就在本文件的历史里：`tool_search` 曾把整个 `arguments` 直接反序列化成
    /// `SearchRequest`，于是 `query_image` 能硬传却从未被宣称，而 schema 默认 `mode=keyword`
    /// 时 `run()` 的 `want_vec` 为假 ⇒ **图片被完全忽略且毫无提示**。
    /// 丢弃（serde 默认）与拒绝都不越权，但只有拒绝能让 agent 自纠。
    fn reject_unadvertised_args(&self, args: &Value) -> Result<(), String> {
        let Some(obj) = args.as_object() else {
            return Ok(());
        };
        let unknown: Vec<&str> = obj
            .keys()
            .map(String::as_str)
            .filter(|k| !Self::SEARCH_ARGS.contains(k))
            .collect();
        if unknown.is_empty() {
            return Ok(());
        }
        let hint = if unknown.contains(&"query_image") || unknown.contains(&"query_image_base64") {
            " 以图搜图请走 REST POST /v1/search 的 query_image_base64 或 multipart——\
             本面尚未宣称该能力（需先有 server 实测的 image/cross_modal caps）。"
        } else {
            ""
        };
        Err(format!(
            "不接受的入参：{}。本实例的 search 只接受 {}（见 tools/list 的 inputSchema）。{hint}",
            unknown.join("、"),
            Self::SEARCH_ARGS.join("、"),
        ))
    }

    /// `search` 工具：入参即 `SearchRequest`（query 必填，mode/top_k/filter 可选）；ACL 服务端注入。
    fn tool_search(&self, args: Value) -> Result<String, String> {
        self.reject_unadvertised_args(&args)?;
        // `mode` 是否**显式**给出：`SearchRequest` 的 serde 默认是 `Hybrid`，而本面 schema 宣称的
        // 默认是 `keyword`（见 `search_modes`）。不在此对齐的话，agent 省略 mode 就会掉进
        // "宣称 hybrid、实际全文"的静默退化——即本条要修的 bug。**以 schema 宣称的为准**。
        let mode_given = args.get("mode").is_some();
        // `max_context_chars` 是**本面**的旋钮，不是 `SearchRequest` 的字段——先摘走再反序列化，
        // 否则 serde 会把它当未知字段丢掉（丢掉不报错，于是预算静默失效）。
        let mut args = args;
        let budget = args
            .as_object_mut()
            .and_then(|o| o.remove("max_context_chars"))
            .and_then(|v| v.as_u64())
            .map(|v| v as usize);
        let mut req: SearchRequest =
            serde_json::from_value(args).map_err(|e| format!("invalid search args: {e}"))?;
        if !mode_given {
            req.mode = SearchMode::Keyword;
        }
        self.reject_unavailable_mode(&req)?;
        // 命中形状**两档必须逐字段相同**，否则同一个工具在两种部署下给 agent 两种契约。
        // 远端拿到的是 server `hits_json` 的富对象（含 bm25/vector/rerank/bbox/media/cursor…），
        // 必须投影回这五个字段；多出来的等 KB-0.4 决定要不要宣称再开。
        let arr: Vec<Value> = match &self.backend {
            Backend::Local { engine, acl } => engine
                .search(&req, acl.as_ref())
                .map_err(|e| format!("search failed: {e}"))?
                .iter()
                .map(|h| {
                    let mut v = json!({
                        "citation_id": h.citation.citation_id(),
                        "score": h.score,
                        "page": h.citation.page,
                        "heading_path": h.citation.heading_path,
                        "snippet": h.highlight,
                    });
                    if req.include_text {
                        v["text"] = json!(h.text);
                    }
                    v
                })
                .collect(),
            Backend::Remote(r) => {
                let body = serde_json::to_value(&req).map_err(|e| e.to_string())?;
                let resp = r.post("/v1/search", &body)?;
                resp.get("hits")
                    .and_then(Value::as_array)
                    .map(|hits| {
                        hits.iter()
                            .map(|h| project_remote_hit(h, req.include_text))
                            .collect()
                    })
                    .unwrap_or_default()
            }
        };
        // 无预算 → 响应形状与本能力落地前逐字节一致（不给既有调用方平白多出字段）。
        let Some(budget) = budget else {
            return serde_json::to_string(&json!({ "hits": arr })).map_err(|e| e.to_string());
        };
        let (arr, dropped, used) = apply_budget(arr, budget);
        serde_json::to_string(&json!({
            "hits": arr,
            // 截断必须**对 agent 可见**：静默丢证据 = 让它以为自己看到了全部。
            "dropped": dropped,
            "context_chars": used,
        }))
        .map_err(|e| e.to_string())
    }

    /// `resolve_citation` 工具：由 citation_id 解析媒资/原文位置（ACL 服务端强制，越权/不存在
    /// 均报"未找到或无权限"，不暴露存在性）。
    /// `index_chunks` 宣称接受的入参 —— 同 `SEARCH_ARGS`，与 schema 逐字一致。
    const INDEX_ARGS: [&'static str; 3] = ["collection", "doc_id", "chunks"];

    /// `index_chunks` 工具（**仅远端档**，KB-0.3）：把已分块内容写入知识库。
    ///
    /// **为什么本地档没有这个工具**：本地档的身份是进程级常量、不是每请求身份，
    /// 在它上面开写入口等于让引擎"替调用方猜写入 ACL"——正是 server 用 403 拒绝掉的那件事
    /// （见 22-mcp spec §4.2 的 C1 全表版与"待决策"的裁定）。
    ///
    /// **解析与分块仍归调用方**：本工具不收原始文件。那是 ADR《职责边界》划定的边界，
    /// 未被 2026-08-24 的修订推翻（修订只收回"摄取作业面"的状态，不是解析算力）。
    fn tool_index_chunks(&self, args: Value) -> Result<String, String> {
        let Backend::Remote(r) = &self.backend else {
            return Err(
                "本实例是本地档，不提供写入工具（它没有每请求身份，写入 ACL 无从判定）。\
                        改用远端档：设 FASTSEARCH_SERVER + FASTSEARCH_KEY。"
                    .into(),
            );
        };
        let obj = args.as_object().ok_or("arguments 必须是对象")?;
        let unknown: Vec<&str> = obj
            .keys()
            .map(String::as_str)
            .filter(|k| !Self::INDEX_ARGS.contains(k))
            .collect();
        if !unknown.is_empty() {
            return Err(format!(
                "不接受的入参：{}。index_chunks 只接受 {}。",
                unknown.join("、"),
                Self::INDEX_ARGS.join("、")
            ));
        }
        let chunks = obj
            .get("chunks")
            .and_then(Value::as_array)
            .ok_or("chunks 必须是数组")?;
        if chunks.is_empty() {
            return Err("chunks 为空：没有可写入的内容。".into());
        }
        // **夹带身份 → 显式拒绝**，而不是让 server 静默覆盖。
        // server 的 `apply_ingest_identity` 无条件用调用者身份覆盖 chunk 的 tenant/acl，
        // 所以夹带本来"无害"；但静默覆盖会让 agent 以为自己控制了可见性——
        // 这正是本仓反复在修的那类"悄悄替调用方决定"。宁可让它读到一句话。
        if let Some(bad) = chunks.iter().position(|c| {
            c.get("tenant").is_some_and(|v| !v.is_null())
                || c.get("acl").is_some_and(|v| !v.is_null())
        }) {
            return Err(format!(
                "chunks[{bad}] 夹带了 tenant/acl。写入身份由服务端从本实例的 API key 注入，\
                 工具入参既不能传也不能放宽——请删掉这两个字段。\
                 要写到别的租户/标签下，请换一把对应的 key。"
            ));
        }
        // **每条 chunk 补上 `doc_id`**：`core::Chunk.doc_id` 是必填字段，`/v1/index` 的
        // `IndexChunk` 用 `#[serde(flatten)]` 展开它 ⇒ 缺了直接 422，而 server 侧的
        // `apply_ingest_identity` 是**反序列化之后**才用 body.doc_id 覆盖它的，救不了。
        // 让 agent 在每条 chunk 里重复一遍 doc_id 是纯粹的仪式——这里替它补，值反正会被覆盖成同一个。
        let doc_id = obj.get("doc_id").cloned().unwrap_or(Value::Null);
        let chunks: Vec<Value> = chunks
            .iter()
            .map(|c| {
                let mut c = c.clone();
                if let Some(o) = c.as_object_mut() {
                    o.insert("doc_id".into(), doc_id.clone());
                }
                c
            })
            .collect();
        let body = json!({
            "collection": obj.get("collection"),
            "doc_id": doc_id,
            "chunks": chunks,
        });
        let resp = r.post("/v1/index", &body)?;
        serde_json::to_string(&json!({ "indexed": resp.get("indexed") })).map_err(|e| e.to_string())
    }

    /// 远端档的 `resolve_citation`：`POST /v1/assets/resolve {"ids":[cid]}`。
    ///
    /// **越权与不存在同一个回答**：`assets_resolve` 对不可见/不存在的 id 直接 `continue`
    /// （不暴露存在性）⇒ 空数组 ⇒ 映射成与本地档**字面一致**的 `found:false`。
    fn remote_resolve(&self, cid: &str) -> Result<String, String> {
        let Backend::Remote(r) = &self.backend else {
            unreachable!("remote_resolve 只在远端档调用")
        };
        let resp = r.post("/v1/assets/resolve", &json!({ "ids": [cid] }))?;
        let Some(item) = resp
            .get("assets")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
        else {
            return serde_json::to_string(
                &json!({ "found": false, "reason": "not found or not authorized" }),
            )
            .map_err(|e| e.to_string());
        };
        // server 未配签名密钥时 inline 分支只回 error —— 报可自纠的错，不把半个资产塞给 agent。
        if let Some(err) = item.get("error").and_then(Value::as_str) {
            return Err(format!(
                "server 无法签发该资产的短时 URL（{err}）。取字节请走 REST GET /v1/asset/{cid}，\
                 或让运维设置 FASTSEARCH_ASSET_SIGNING_KEY。"
            ));
        }
        let fetch = match item.get("type").and_then(Value::as_str).unwrap_or_default() {
            "doc_render" => json!({
                "kind": "doc_render",
                "doc_id": item.get("doc_id"), "page": item.get("page"), "bbox": item.get("bbox"),
            }),
            "object" => json!({
                "kind": "signed_url",
                "url": item.get("url"), "expires_s": item.get("expires_s"),
            }),
            // 两档唯一允许的出参差异：远端已经签出 URL 了，本地档没有签名器也不该有。
            // 差异写进工具 description（见 `tool_defs`）。
            "inline" => json!({
                "kind": "inline_ref",
                "url": item.get("url"), "expires_s": item.get("expires_s"),
            }),
            other => return Err(format!("server 返回了未知的资产类型 {other:?}")),
        };
        serde_json::to_string(&json!({
            "found": true,
            "media_type": item.get("media_type"),
            "time": item.get("time"),
            "fetch": fetch,
        }))
        .map_err(|e| e.to_string())
    }

    fn tool_resolve_citation(&self, args: Value) -> Result<String, String> {
        let cid = args
            .get("citation_id")
            .and_then(|c| c.as_str())
            .ok_or("missing citation_id")?;
        let Backend::Local { engine, acl } = &self.backend else {
            return self.remote_resolve(cid);
        };
        let resolved = engine
            .resolve_citation(cid, acl.as_ref())
            .map_err(|e| format!("resolve failed: {e}"))?;
        let v = match resolved {
            None => json!({ "found": false, "reason": "not found or not authorized" }),
            Some(a) => {
                let fetch = match a.fetch {
                    AssetFetch::DocRender { doc_id, page, bbox } => json!({
                        "kind": "doc_render", "doc_id": doc_id, "page": page, "bbox": bbox,
                    }),
                    AssetFetch::SignedUrl { url, expires_s } => json!({
                        "kind": "signed_url", "url": url, "expires_s": expires_s,
                    }),
                    AssetFetch::InlineRef => json!({
                        // inline 小图：字节在 PG 真源，经 REST `GET /v1/asset/{cid}` 取（MCP 只给指针）。
                        "kind": "inline_ref",
                    }),
                };
                json!({ "found": true, "media_type": a.media_type, "time": a.time, "fetch": fetch })
            }
        };
        serde_json::to_string(&v).map_err(|e| e.to_string())
    }
}

impl McpServer {
    /// 两个工具的定义（名称/描述/入参 JSON Schema）。
    ///
    /// **按本实例的真实能力生成**（KB-0.1）：`mode` 的 enum 与 default 来自 [`Self::search_modes`]，
    /// 不再无条件宣称 `hybrid`。描述里也写明本实例的档位——agent 只能读到什么就信什么。
    pub fn tool_defs(&self) -> Value {
        let modes = self.search_modes();
        let semantic = self.can_embed_text_query();
        let default_mode = if semantic { "hybrid" } else { "keyword" };
        let search_desc = if semantic {
            // **诚实记账**：`embedded:true` 只证明 server 配了嵌入后端，不证明向量是语义的
            // （HashEmbedder 基线也是 Embedder，而 server_vector_info 不吐 caps().semantic）。
            // 故只能说"由 server 侧嵌入后端提供"，不得写"语义检索"。等 KB-2.4 的实测 caps。
            "在 fastsearch 混合检索引擎中检索（keyword/vector/hybrid），返回带引用\
             （citation_id/page/heading_path/snippet）的命中，供答案层溯源。\
             本实例的 vector/hybrid 档**由 server 侧的嵌入后端提供**（是否为语义嵌入取决于该后端配置）。\
             ACL 由服务端按 API key 强制注入，工具入参无法传递或放宽。"
        } else {
            "在 fastsearch 中做**全文（BM25）**检索，返回带引用（citation_id/page/heading_path/snippet）\
             的命中，供答案层溯源。ACL 由服务端强制。\
             **本实例只支持 mode=\"keyword\"**：MCP 面直连引擎、自身不产查询向量，\
             语义/混合检索请改走 REST POST /v1/search（server 侧配了嵌入后端），\
             或自带 `vector` 入参（见其说明）。"
        };
        // `vector` 在两档下含义不同：无语义档时它是**唯一**能开出向量召回的路（故要讲清代价）；
        // 有语义档时它只是"跳过服务端嵌入"的旁路。
        let vector_desc = if semantic {
            "可选：外部预计算的查询向量，传了则跳过服务端嵌入。维度须与索引一致。"
        } else {
            "可选：外部预计算的查询向量。本实例自身不产查询向量，**这是在此开出 \
             mode=vector/hybrid 的唯一途径**；须与索引用同一嵌入模型、同一维度，否则召回无意义。"
        };
        // KB-0.5：agent 没有人类的试错直觉，只能读描述与错误文本。工具描述必须讲清**本实例**
        // 的作用域与可用字段，而不是泛泛的"混合检索引擎"。
        let search_desc = format!("{search_desc}{}", self.scope_hint());
        let mut defs = json!([
        {
            "name": "search",
            "description": search_desc,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "检索词/问题" },
                    "mode": { "type": "string", "enum": modes, "default": default_mode },
                    "top_k": { "type": "integer", "default": 20 },
                    "filter": { "type": "object", "description": "core::Filter AST（可选）" },
                    "highlight": { "type": "boolean", "default": false },
                    // 诚实契约是**双向**的：宣称的必须能兑现，**能兑现的也必须宣称**——
                    // 否则就是暗门（调用方只能靠读源码发现）。`vector` 这条真能走通
                    // （`reject_unavailable_mode` 明确放行、`caller_supplied_vector_is_not_rejected`
                    // 测试背书），此前却不在 schema 里，属同一条契约的反向违例。
                    "vector": {
                        "type": "array", "items": { "type": "number" },
                        "description": vector_desc
                    },
                    "include_text": {
                        "type": "boolean", "default": false,
                        "description": "在每条命中里附带完整 chunk 正文。**默认关闭**：整段正文很容易                            冲爆上下文。开它时建议同时设 max_context_chars。"
                    },
                    "max_context_chars": {
                        "type": "integer",
                        "description": "本次返回的 snippet+text 总**字符数**上限（不是 token：本面没有                            分词器，估算 token 会是编造的数字）。按既有排序前向累加，放不下的那条若还能                            留够 80 字符就截断并标 text_truncated，否则整条丢弃；其后一律不返回。                            设了它时响应会多出 dropped（丢弃条数）与 context_chars（实际用量）——                            截断对调用方始终可见。"
                    }
                },
                "required": ["query"]
            }
        },
        {
            "name": "resolve_citation",
            "description": "由 citation_id 解析媒资/原文位置（page+bbox 或签名 URL），用于深链/打开\
                原文。ACL 由服务端强制，越权/不存在均返回 found:false。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "citation_id": { "type": "string", "description": "collection:doc_id:chunk_id" }
                },
                "required": ["citation_id"]
            }
        }
        ]);
        // C1 **全表版**：只有远端档具备的能力，在本地档的 tools/list 里**不得出现**——
        // 不是"出现了但会报错"。本地档没有每请求身份，在它上面开写入口等于让引擎
        // "替调用方猜写入 ACL"，正是 server 用 403 拒绝掉的那件事。
        if matches!(self.backend, Backend::Remote(_)) {
            defs.as_array_mut()
                .expect("tool_defs 是数组")
                .push(json!({
                    "name": "index_chunks",
                    "description": "把**已分块**的内容写入知识库（doc 级替换：同 doc_id 重复调用会                        整篇替换，不会重复堆积）。写入身份来自本实例的 API key，                        **tenant/acl 由服务端强制注入，工具入参不接受也无法放宽**。                        解析与分块归调用方：本工具不接受原始文件——把文件变成 chunk 的做法见                        docs/文件解析与摄取.md。",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "collection": { "type": "string", "description": "目标集合名" },
                            "doc_id": { "type": "string", "description": "文档标识；同值重复写=整篇替换" },
                            "chunks": {
                                "type": "array",
                                "description": "chunk 数组。每条至少含 chunk_id/kind/text/page/bbox/char_len；                                    **不得含 tenant/acl**（身份由服务端注入）。字段释义见 core::Chunk。",
                                "items": { "type": "object" }
                            }
                        },
                        "required": ["collection", "doc_id", "chunks"]
                    }
                }));
        }
        defs
    }
}

/// MCP `tools/call` 结果：单个文本内容块 + isError 标志。
fn tool_text(text: &str, is_error: bool) -> Value {
    json!({
        "content": [ { "type": "text", "text": text } ],
        "isError": is_error,
    })
}

/// server `hits_json` 的富对象 → MCP 宣称的五字段命中（`highlight` → `snippet`）。
///
/// 投影而非透传：多出来的 `bm25`/`vector`/`rerank`/`bbox`/`media`/`cursor` 若原样吐给 agent，
/// 就变成"远端档有、本地档没有"的隐性契约差异（spec §3.1）。
fn project_remote_hit(h: &Value, include_text: bool) -> Value {
    let mut v = json!({
        "citation_id": h.get("citation_id"),
        "score": h.get("score"),
        "page": h.get("page"),
        "heading_path": h.get("heading_path"),
        "snippet": h.get("highlight"),
    });
    if include_text {
        v["text"] = h.get("text").cloned().unwrap_or(Value::Null);
    }
    v
}

/// 一条命中占的上下文成本（**字符数**，不是字节——CJK 一个字 3 字节，按字节算会离谱地高估）。
fn hit_cost(h: &Value) -> usize {
    ["snippet", "text"]
        .iter()
        .filter_map(|k| h.get(*k).and_then(Value::as_str))
        .map(|t| t.chars().count())
        .sum()
}

/// 单条命中至少要留这么多字符才值得放进来——否则宁可整条丢掉。
/// 一个被砍到只剩十几个字的片段对答案层没有价值，却仍要占一条 citation 的位置。
const MIN_HIT_CHARS: usize = 80;

/// 按预算裁剪命中（KB-0.4）。返回 (裁剪后的命中, 丢弃条数, 实际用掉的字符数)。
///
/// **落点在 MCP 层而非 engine**：token/上下文预算是**答案层约束**，塞进通用 engine 的 top-k
/// 会让"检索该返回什么"与"调用方的上下文有多大"耦死（见 FastGPT 参考建议 §5.2-5）。
///
/// **确定性**（不变量 #4）：按融合后的既有顺序前向累加，同输入必同结果——不重排、不抽样。
/// **截断对 agent 可见**：被砍的那条标 `text_truncated`，整体报 `dropped` 与 `context_chars`；
/// 静默丢证据是本仓反复在修的那类错。
fn apply_budget(hits: Vec<Value>, budget: usize) -> (Vec<Value>, usize, usize) {
    let total = hits.len();
    let mut out = Vec::with_capacity(total);
    let mut used = 0usize;
    for mut h in hits {
        let cost = hit_cost(&h);
        if used + cost <= budget {
            used += cost;
            out.push(h);
            continue;
        }
        // 放不下整条：能留够 MIN_HIT_CHARS 就截断它，否则整条丢弃。两种情况后续都不再放。
        let remaining = budget.saturating_sub(used);
        if remaining >= MIN_HIT_CHARS {
            if let Some(t) = h.get("text").and_then(Value::as_str) {
                let kept: String = t.chars().take(remaining).collect();
                let kept_len = kept.chars().count();
                h["text"] = json!(format!("{kept}…"));
                h["text_truncated"] = json!(true);
                used += kept_len;
                out.push(h);
            }
        }
        break;
    }
    let dropped = total - out.len();
    (out, dropped, used)
}

/// JSON-RPC 成功响应。
fn ok(id: Option<Value>, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id.unwrap_or(Value::Null), "result": result })
}

/// JSON-RPC 错误响应。
fn err(id: Option<Value>, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id.unwrap_or(Value::Null), "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastsearch_core::{BBox, Chunk, ChunkKind};
    use fastsearch_text::{TextIndexConfig, TokenizerKind};
    use std::sync::mpsc;

    fn chunk(doc: &str, id: u64, text: &str) -> Chunk {
        Chunk {
            doc_id: doc.into(),
            chunk_id: id,
            kind: ChunkKind::Paragraph,
            text: text.into(),
            page: 1,
            bbox: BBox {
                x0: 0.0,
                y0: 0.0,
                x1: 1.0,
                y1: 1.0,
            },
            heading_path: vec!["财务".into()],
            section_id: 0,
            char_len: text.chars().count() as u32,
            media: None,
            media_bytes: None,
            image_vector_status: None,
            tenant: None,
            acl: vec!["public".into()],
            metadata: Default::default(),
            searchable: true,
        }
    }

    fn server() -> McpServer {
        let cfg = TextIndexConfig {
            tokenizer: TokenizerKind::Jieba,
            ..Default::default()
        };
        let mut e = Engine::create_in_ram(cfg).unwrap();
        e.ingest("kb", &chunk("r.pdf", 1, "毛利率提升至 42%"))
            .unwrap();
        e.ingest("kb", &chunk("r.pdf", 2, "营业收入增长")).unwrap();
        e.commit().unwrap();
        McpServer::new(e, None)
    }

    #[test]
    fn initialize_and_tools_list() {
        let s = server();
        let init = s
            .handle(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}))
            .unwrap();
        assert_eq!(init["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(init["result"]["serverInfo"]["name"], "fastsearch-mcp");

        let list = s
            .handle(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
            .unwrap();
        let tools = list["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"search") && names.contains(&"resolve_citation"));
    }

    #[test]
    fn notification_has_no_response() {
        let s = server();
        assert!(s
            .handle(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
            .is_none());
    }

    #[test]
    fn unknown_method_errors() {
        let s = server();
        let r = s
            .handle(&json!({"jsonrpc":"2.0","id":9,"method":"bogus"}))
            .unwrap();
        assert_eq!(r["error"]["code"], -32601);
    }

    #[test]
    fn tools_call_search_returns_hits() {
        let s = server();
        let r = s
            .handle(&json!({
                "jsonrpc":"2.0","id":3,"method":"tools/call",
                "params": { "name": "search", "arguments": { "query": "毛利率", "mode": "keyword" } }
            }))
            .unwrap();
        assert_eq!(r["result"]["isError"], false);
        let text = r["result"]["content"][0]["text"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        let hits = parsed["hits"].as_array().unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0]["citation_id"], "kb:r.pdf:1");
    }

    /// KB-0.1 —— **schema 宣称的能力必须等于实际能力**。
    ///
    /// 修复前：`mode` 无条件宣称 `["keyword","vector","hybrid"]` 且 `default:"hybrid"`，
    /// 而本 crate 零 embedder + `Engine::run()` 从不嵌文本 query ⇒ agent 按 schema 用默认档，
    /// 拿到的永远是纯 keyword 结果且毫不知情。
    #[test]
    fn tools_list_advertises_only_real_capability() {
        let s = server();
        let list = s
            .handle(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
            .unwrap();
        let search = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "search")
            .unwrap();
        let mode = &search["inputSchema"]["properties"]["mode"];
        assert_eq!(mode["enum"], json!(["keyword"]), "不得宣称做不到的档");
        assert_eq!(mode["default"], "keyword");
        // 描述必须把限制讲给 agent 听（它只能读到什么就信什么）。
        let desc = search["description"].as_str().unwrap();
        assert!(desc.contains("keyword") && desc.contains("/v1/search"));

        // 诚实契约的**反向**半边：真能兑现的能力也必须宣称，不得作为暗门存在。
        // `vector` 真能走通（见 `caller_supplied_vector_is_not_rejected`），故必须在 schema 里，
        // 且说明要点破"本实例下它是开出向量召回的唯一途径"。
        let vector = &search["inputSchema"]["properties"]["vector"];
        assert_eq!(vector["type"], "array", "能兑现的 `vector` 必须被宣称");
        assert!(vector["description"].as_str().unwrap().contains("唯一"));
    }

    /// 省略 `mode` 时按 **schema 宣称的默认（keyword）** 走，而不是 `SearchRequest` 的
    /// serde 默认（Hybrid）—— 后者正是静默退化的入口。行为上仍返回命中，无回归。
    #[test]
    fn omitted_mode_runs_keyword_not_silent_hybrid() {
        let s = server();
        let r = s
            .handle(&json!({
                "jsonrpc":"2.0","id":6,"method":"tools/call",
                "params": { "name": "search", "arguments": { "query": "毛利率" } }
            }))
            .unwrap();
        assert_eq!(r["result"]["isError"], false);
        let text = r["result"]["content"][0]["text"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        assert!(!parsed["hits"].as_array().unwrap().is_empty());
    }

    /// 显式要一个本面给不出的档 → **可自纠的错误**，不是静默退化成全文。
    #[test]
    fn explicit_unavailable_mode_errors_with_actionable_text() {
        let s = server();
        for mode in ["hybrid", "vector"] {
            let r = s
                .handle(&json!({
                    "jsonrpc":"2.0","id":7,"method":"tools/call",
                    "params": { "name": "search",
                                "arguments": { "query": "毛利率", "mode": mode } }
                }))
                .unwrap();
            assert!(r.get("error").is_none(), "工具级失败不发协议 error");
            assert_eq!(r["result"]["isError"], true, "mode={mode} 应被拒绝");
            let text = r["result"]["content"][0]["text"].as_str().unwrap();
            // agent 只能读错误文本自纠：必须给出可执行的下一步。
            assert!(text.contains("keyword"), "要告诉 agent 改用哪个档");
            assert!(text.contains("/v1/search"), "要指出语义检索该走哪条路");
        }
    }

    /// 调用方**自带** `vector` 时 vector/hybrid 是真能走通的 —— 守卫不得误伤这条路。
    #[test]
    fn caller_supplied_vector_is_not_rejected() {
        let s = server();
        let r = s
            .handle(&json!({
                "jsonrpc":"2.0","id":8,"method":"tools/call",
                "params": { "name": "search", "arguments": {
                    "query": "毛利率", "mode": "vector", "vector": [0.1, 0.2, 0.3] } }
            }))
            .unwrap();
        assert_eq!(r["result"]["isError"], false, "自带向量不该被能力守卫拦下");
    }

    /// 无预算时响应形状与本能力落地前**逐字节一致**——不给既有调用方平白多出字段。
    #[test]
    fn no_budget_keeps_response_shape() {
        let s = server();
        let r = s
            .handle(&json!({
                "jsonrpc":"2.0","id":60,"method":"tools/call",
                "params": { "name":"search", "arguments": { "query":"毛利率" } }
            }))
            .unwrap();
        let v: Value =
            serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["hits"], "无预算时不得多出 dropped/context_chars");
    }

    /// 预算按**既有顺序**前向累加，放不下的整条丢弃，且丢弃对 agent 可见。
    /// 确定性（不变量 #4）：同输入必同结果——不重排、不抽样。
    #[test]
    fn budget_drops_tail_and_reports_it() {
        let s = server();
        let call = |budget: usize| {
            let r = s
                .handle(&json!({
                    "jsonrpc":"2.0","id":61,"method":"tools/call",
                    "params": { "name":"search", "arguments": {
                        "query":"毛利率 营业收入", "include_text": true,
                        "max_context_chars": budget } }
                }))
                .unwrap();
            serde_json::from_str::<Value>(r["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap()
        };
        // 预算 0：一条都放不下（连 MIN_HIT_CHARS 都留不出），全丢且如实报数。
        let tight = call(0);
        assert_eq!(tight["hits"].as_array().unwrap().len(), 0);
        assert!(tight["dropped"].as_u64().unwrap() >= 1, "丢弃必须被报出来");
        assert_eq!(tight["context_chars"], 0);
        // 预算充裕：全给，dropped=0。
        let loose = call(100_000);
        assert_eq!(loose["dropped"], 0);
        assert!(!loose["hits"].as_array().unwrap().is_empty());
        // 确定性：同输入两次结果逐字节相同。
        assert_eq!(call(100_000), loose);
    }

    /// 放不下整条但还留得住 MIN_HIT_CHARS ⇒ **截断而非丢弃**，并标 `text_truncated`。
    #[test]
    fn budget_truncates_instead_of_dropping_when_worth_it() {
        let long: String = "毛利率".repeat(200); // 600 字符
        let cfg = TextIndexConfig {
            tokenizer: TokenizerKind::Jieba,
            ..Default::default()
        };
        let mut e = Engine::create_in_ram(cfg).unwrap();
        e.ingest("kb", &chunk("r.pdf", 1, &long)).unwrap();
        e.commit().unwrap();
        let s = McpServer::new(e, None);
        let r = s
            .handle(&json!({
                "jsonrpc":"2.0","id":62,"method":"tools/call",
                "params": { "name":"search", "arguments": {
                    "query":"毛利率", "include_text": true, "max_context_chars": 200 } }
            }))
            .unwrap();
        let v: Value =
            serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        let hit = &v["hits"][0];
        assert_eq!(hit["text_truncated"], true, "被截断必须标出来");
        let text = hit["text"].as_str().unwrap();
        assert!(
            text.chars().count() <= 201,
            "截断后不得超预算（+1 是省略号）：{}",
            text.chars().count()
        );
        assert!(text.ends_with('…'));
        assert!(v["context_chars"].as_u64().unwrap() <= 200);
    }

    /// KB-0.5：描述要讲清**本实例**的作用域与可用 filter 字段，而不是泛泛的"混合检索引擎"。
    #[test]
    fn description_tells_agent_the_scope_and_filter_fields() {
        let s = server();
        let defs = s.tool_defs();
        let desc = defs[0]["description"].as_str().unwrap();
        assert!(desc.contains("collection"), "要讲清怎么限定集合");
        assert!(
            desc.contains("modality") && desc.contains("page"),
            "要列出可用 filter 字段"
        );
        // 本地档拿不到集合名单 ⇒ 什么都不说；编一个比不说更糟。
        assert!(!desc.contains("已注册"), "本地档不得凭空给出集合名单");
    }

    /// 远端档的集合名单来自探测，**必须带 caveat**（registry 是内存态、非真源、可能不全）。
    #[test]
    fn remote_description_lists_collections_with_caveat() {
        let (url, _rx) = spawn_server(PROBE_EMBEDDED, r#"{"hits":[]}"#);
        let backend = RemoteBackend::connect(Some(url), Some("k".into())).unwrap();
        let s = McpServer::with_backend(Backend::Remote(Box::new(backend)));
        let defs = s.tool_defs();
        let desc = defs[0]["description"].as_str().unwrap();
        assert!(desc.contains("kb"), "要列出探到的集合");
        assert!(
            desc.contains("可能不全"),
            "必须带 caveat，不得说成本库全部集合"
        );
    }

    // ============ 远端档（KB-0.2）============

    /// 起一个按路径分派的 mock server；返回 base URL + 收到的请求（原始字节）。
    ///
    /// 必须先读完整个请求再回响应：提前关连接会让客户端写 body 时收到 RST（flaky）。
    fn spawn_server(
        probe: &'static str,
        search: &'static str,
    ) -> (String, mpsc::Receiver<Vec<u8>>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(8) {
                let Ok(mut st) = stream else { break };
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                loop {
                    match st.read(&mut tmp) {
                        Ok(0) => break,
                        Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        Err(_) => break,
                    }
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..p]).to_lowercase();
                        let cl = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if buf.len() - (p + 4) >= cl {
                            break;
                        }
                    }
                }
                let body = if String::from_utf8_lossy(&buf).starts_with("GET /v1/collections") {
                    probe
                } else {
                    search
                };
                let _ = tx.send(buf);
                let _ = write!(
                    st,
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = st.flush();
            }
        });
        (url, rx)
    }

    /// 探测响应用 server `server_vector_info` 的**真实字段名**——字段名写错会静默变成
    /// "unknown"（实跑时就这么暴露过一次），故测试里也必须按真名断言。
    const PROBE_EMBEDDED: &str = r#"{"collections":["kb"],"server":{"embedded":true,
        "vector_backend":"brute","source_of_truth":"postgres","rebuildable_from_source":true}}"#;

    /// 远端探测把 server 的**实测**运行档映射进 schema：`embedded:true` ⇒ 三档可如实宣称。
    /// 这正是"不在 MCP 里养第二套嵌入配置"的兑现——hybrid 由 server 免费提供。
    #[test]
    fn remote_probe_maps_caps_to_schema() {
        let (url, _rx) = spawn_server(PROBE_EMBEDDED, r#"{"hits":[]}"#);
        let backend = RemoteBackend::connect(Some(url), Some("k".into())).unwrap();
        assert!(backend.caps().embedded);
        // 字段名必须与 server_vector_info 对得上（写错只会静默变 "unknown"）。
        assert_eq!(backend.caps().vector_backend, "brute");
        assert_eq!(backend.caps().source_of_truth, "postgres");
        assert_eq!(backend.caps().collections, vec!["kb".to_string()]);
        let s = McpServer::with_backend(Backend::Remote(Box::new(backend)));
        let defs = s.tool_defs();
        let mode = &defs[0]["inputSchema"]["properties"]["mode"];
        assert_eq!(mode["enum"], json!(["keyword", "vector", "hybrid"]));
        assert_eq!(mode["default"], "hybrid");
        // 诚实记账：不得把 server 有嵌入后端说成"语义检索"（HashEmbedder 基线也是 Embedder）。
        let desc = defs[0]["description"].as_str().unwrap();
        assert!(desc.contains("嵌入后端"), "措辞要说明 hybrid 的来源");
    }

    /// 探测失败 → **拒绝启动**（fail-closed），绝不静默回退本地档或猜一个 schema。
    #[test]
    fn remote_probe_failure_refuses_to_start() {
        // 未监听的端口：连接直接失败。
        let msg = match RemoteBackend::connect(Some("http://127.0.0.1:9".into()), Some("k".into()))
        {
            Ok(_) => panic!("探测失败必须 Err，绝不能静默启动"),
            Err(e) => format!("{e}"),
        };
        assert!(msg.contains("能力探测失败"), "{msg}");
        assert!(
            msg.contains("FASTSEARCH_SERVER"),
            "要给出可自纠的方向：{msg}"
        );
    }

    /// 出站请求**不得携带任何 ACL 面**，且必须带 Bearer——身份是那把 key，ACL 由 server 注入。
    #[test]
    fn remote_search_sends_no_acl_and_carries_bearer() {
        let (url, rx) = spawn_server(PROBE_EMBEDDED, r#"{"hits":[]}"#);
        let backend = RemoteBackend::connect(Some(url), Some("secret-key".into())).unwrap();
        let s = McpServer::with_backend(Backend::Remote(Box::new(backend)));
        let _ = rx.recv().unwrap(); // 探测请求
        let r = s
            .handle(&json!({
                "jsonrpc":"2.0","id":40,"method":"tools/call",
                "params": { "name":"search", "arguments": { "query":"毛利率" } }
            }))
            .unwrap();
        assert_eq!(r["result"]["isError"], false);
        let req = String::from_utf8(rx.recv().unwrap()).unwrap();
        assert!(req.contains("bearer secret-key") || req.contains("Bearer secret-key"));
        let body = req.split("\r\n\r\n").nth(1).unwrap_or_default();
        assert!(!body.contains("\"tenant\""), "出站 body 不得含 tenant");
        assert!(!body.contains("\"acl\""), "出站 body 不得含 acl");
        assert!(
            !body.contains("query_image"),
            "出站 body 不得含 query_image"
        );
    }

    /// 命中形状两档必须一致：server 的富对象要投影回宣称的五个字段，多余字段不得泄漏给 agent。
    #[test]
    fn remote_hit_projection_matches_local_shape() {
        let (url, rx) = spawn_server(
            PROBE_EMBEDDED,
            r#"{"hits":[{"citation_id":"kb:r.pdf:1","score":1.5,"page":7,
                "heading_path":["财务"],"highlight":"毛利率 42%",
                "bm25":1.2,"vector":0.8,"rerank":null,"bbox":{"x0":0.0,"y0":0.0,"x1":1.0,"y1":1.0},
                "section_id":3,"media":null,"cursor":"abc"}]}"#,
        );
        let backend = RemoteBackend::connect(Some(url), Some("k".into())).unwrap();
        let s = McpServer::with_backend(Backend::Remote(Box::new(backend)));
        let _ = rx.recv().unwrap();
        let r = s
            .handle(&json!({
                "jsonrpc":"2.0","id":41,"method":"tools/call",
                "params": { "name":"search", "arguments": { "query":"毛利率" } }
            }))
            .unwrap();
        let text = r["result"]["content"][0]["text"].as_str().unwrap();
        let hit = serde_json::from_str::<Value>(text).unwrap()["hits"][0].clone();
        let keys: std::collections::BTreeSet<String> =
            hit.as_object().unwrap().keys().cloned().collect();
        assert_eq!(
            keys,
            ["citation_id", "heading_path", "page", "score", "snippet"]
                .iter()
                .map(|k| k.to_string())
                .collect::<std::collections::BTreeSet<_>>(),
            "远端命中必须投影成与本地档相同的五字段"
        );
        assert_eq!(hit["snippet"], "毛利率 42%", "highlight → snippet");
    }

    /// C1 **全表版**：写入工具只在远端档出现——不是"出现了但会报错"。
    #[test]
    fn write_tool_is_absent_in_local_mode() {
        let s = server();
        let defs = s.tool_defs();
        let names: Vec<&str> = defs
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(!names.contains(&"index_chunks"), "本地档不得宣称写入工具");
        // 未宣称 ⇒ 也必须真的不可调用，否则"没宣称但能调"就是又一个暗门。
        let r = s
            .handle(&json!({
                "jsonrpc":"2.0","id":50,"method":"tools/call",
                "params": { "name":"index_chunks",
                            "arguments": {"collection":"kb","doc_id":"d","chunks":[{}]} }
            }))
            .unwrap();
        assert_eq!(r["result"]["isError"], true);
        let text = r["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("FASTSEARCH_SERVER"), "要给出改法：{text}");
    }

    /// 远端档宣称写入工具，且写入落到 `/v1/index`（doc 级替换）。
    #[test]
    fn remote_index_chunks_posts_to_v1_index() {
        let (url, rx) = spawn_server(PROBE_EMBEDDED, r#"{"indexed":2}"#);
        let backend = RemoteBackend::connect(Some(url), Some("k".into())).unwrap();
        let s = McpServer::with_backend(Backend::Remote(Box::new(backend)));
        let _ = rx.recv().unwrap(); // 探测

        let defs = s.tool_defs();
        let names: Vec<&str> = defs
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"index_chunks"), "远端档必须宣称写入工具");

        let r = s
            .handle(&json!({
                "jsonrpc":"2.0","id":51,"method":"tools/call",
                "params": { "name":"index_chunks", "arguments": {
                    "collection":"kb","doc_id":"note-1",
                    "chunks":[{"chunk_id":0,"kind":"paragraph","text":"甲","page":1,
                               "bbox":{"x0":0,"y0":0,"x1":1,"y1":1},"char_len":1}] } }
            }))
            .unwrap();
        assert_eq!(r["result"]["isError"], false);
        let text = r["result"]["content"][0]["text"].as_str().unwrap();
        assert_eq!(serde_json::from_str::<Value>(text).unwrap()["indexed"], 2);

        let req = String::from_utf8(rx.recv().unwrap()).unwrap();
        assert!(req.starts_with("POST /v1/index"), "落点必须是 /v1/index");
        assert!(req.contains("Bearer k") || req.contains("bearer k"));
        // `core::Chunk.doc_id` 必填且 `IndexChunk` 是 flatten ⇒ 每条 chunk 都得带，
        // 否则 server 在**反序列化阶段**就 422（`apply_ingest_identity` 的覆盖发生在之后，救不了）。
        // 这条断言是活服务验证抓到该问题后补的：mock 无脑回固定 JSON，不看请求体，抓不到。
        let body: Value =
            serde_json::from_str(req.split("\r\n\r\n").nth(1).unwrap_or_default()).unwrap();
        assert_eq!(
            body["chunks"][0]["doc_id"], "note-1",
            "每条 chunk 必须带 doc_id"
        );
    }

    /// chunk 里夹带 tenant/acl → **显式拒绝**，不让 server 静默覆盖。
    ///
    /// 夹带本来"无害"（`apply_ingest_identity` 无条件覆盖），但静默覆盖会让 agent
    /// 以为自己控制了可见性——正是本仓反复在修的那类"悄悄替调用方决定"。
    #[test]
    fn smuggled_identity_in_chunks_is_refused_not_silently_overwritten() {
        let (url, rx) = spawn_server(PROBE_EMBEDDED, r#"{"indexed":1}"#);
        let backend = RemoteBackend::connect(Some(url), Some("k".into())).unwrap();
        let s = McpServer::with_backend(Backend::Remote(Box::new(backend)));
        let _ = rx.recv().unwrap();
        for bad in [json!({"tenant":"other"}), json!({"acl":["admin"]})] {
            let mut chunk = json!({"chunk_id":0,"kind":"paragraph","text":"甲","page":1,
                                   "bbox":{"x0":0,"y0":0,"x1":1,"y1":1},"char_len":1});
            for (k, v) in bad.as_object().unwrap() {
                chunk[k] = v.clone();
            }
            let r = s
                .handle(&json!({
                    "jsonrpc":"2.0","id":52,"method":"tools/call",
                    "params": { "name":"index_chunks", "arguments": {
                        "collection":"kb","doc_id":"d","chunks":[chunk] } }
                }))
                .unwrap();
            assert_eq!(r["result"]["isError"], true, "夹带身份必须被拒");
            let text = r["result"]["content"][0]["text"].as_str().unwrap();
            assert!(text.contains("API key"), "要说清身份从哪来：{text}");
        }
    }

    /// 写入工具的允许清单同样与 schema 一致（C1 的唯一扩展点）。
    #[test]
    fn index_schema_and_allowlist_agree() {
        let (url, _rx) = spawn_server(PROBE_EMBEDDED, r#"{"indexed":0}"#);
        let backend = RemoteBackend::connect(Some(url), Some("k".into())).unwrap();
        let s = McpServer::with_backend(Backend::Remote(Box::new(backend)));
        let defs = s.tool_defs();
        let tool = defs
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "index_chunks")
            .unwrap();
        let props: std::collections::BTreeSet<String> = tool["inputSchema"]["properties"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        let allow: std::collections::BTreeSet<String> = McpServer::INDEX_ARGS
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(props, allow);
    }

    /// 允许清单必须与 schema 宣称的 `properties` **逐字一致**。
    ///
    /// 这是 C1 唯一的扩展点：以后任何新入参都必须同时改两处，本测试盯住它们不漂移。
    #[test]
    fn schema_and_allowlist_agree() {
        let s = server();
        let defs = s.tool_defs();
        let props = defs[0]["inputSchema"]["properties"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        let allow = McpServer::SEARCH_ARGS
            .iter()
            .map(|s| s.to_string())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(props, allow, "schema properties 与允许清单必须一致");
    }

    /// 未宣称的入参一律**拒绝**，不是静默丢弃。
    ///
    /// `query_image` 是最典型的一条：它此前能硬传（`SearchRequest` 有该字段且无
    /// `deny_unknown_fields`），而 schema 从未宣称 ⇒ 默认 `keyword` 档下 `want_vec` 为假、
    /// 图片被完全忽略且毫无提示。
    #[test]
    fn unadvertised_args_are_rejected_with_guidance() {
        let s = server();
        // 注意 `include_text` **不在**此列：KB-0.4 已把它连同 `max_context_chars` 一起宣称
        // （两者必须同时进 schema 与允许清单——只放开 include_text 而不给预算，
        // 等于把冲爆上下文的开关递给 agent 却不给刹车）。
        for (arg, val) in [
            ("query_image", json!([1, 2, 3])),
            ("include_metadata", json!(true)),
            ("search_after", json!("cursor")),
        ] {
            let r = s
                .handle(&json!({
                    "jsonrpc":"2.0","id":30,"method":"tools/call",
                    "params": { "name": "search",
                                "arguments": { "query": "毛利率", arg: val } }
                }))
                .unwrap();
            assert_eq!(r["result"]["isError"], true, "{arg} 应被拒绝");
            let text = r["result"]["content"][0]["text"].as_str().unwrap();
            assert!(text.contains(arg), "错误文本要点名是哪个入参：{text}");
            assert!(text.contains("query"), "要列出本实例接受的字段");
        }
        // 伪造 tenant/acl 同样落在拒绝侧（此前是被 serde 静默丢弃——不越权，但 agent 无从知晓）。
        let r = s
            .handle(&json!({
                "jsonrpc":"2.0","id":31,"method":"tools/call",
                "params": { "name": "search",
                            "arguments": { "query": "x", "tenant": "other", "acl": ["admin"] } }
            }))
            .unwrap();
        assert_eq!(r["result"]["isError"], true, "伪造身份字段必须被显式拒绝");
    }

    /// `query_image` 走 schema 宣称的合法入参时也不该悄悄生效——本地档已彻底拒绝它，
    /// 于是"带图但被忽略"这个静默失败面消失。
    #[test]
    fn image_search_is_refused_with_a_route_not_silently_ignored() {
        let s = server();
        let r = s
            .handle(&json!({
                "jsonrpc":"2.0","id":32,"method":"tools/call",
                "params": { "name": "search",
                            "arguments": { "query": "", "query_image": [1,2,3] } }
            }))
            .unwrap();
        assert_eq!(r["result"]["isError"], true);
        let text = r["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("query_image_base64"),
            "要给出以图搜图的正确走法：{text}"
        );
    }

    #[test]
    fn tools_call_bad_args_is_tool_error_not_protocol_error() {
        let s = server();
        // 缺 query → SearchRequest 反序列化失败 → isError:true（工具级），仍是 result 不是 error。
        let r = s
            .handle(&json!({
                "jsonrpc":"2.0","id":4,"method":"tools/call",
                "params": { "name": "search", "arguments": { "mode": "keyword" } }
            }))
            .unwrap();
        assert!(r.get("error").is_none());
        assert_eq!(r["result"]["isError"], true);
    }

    #[test]
    fn tools_call_resolve_citation_no_media() {
        let s = server();
        let r = s
            .handle(&json!({
                "jsonrpc":"2.0","id":5,"method":"tools/call",
                "params": { "name": "resolve_citation", "arguments": { "citation_id": "kb:r.pdf:1" } }
            }))
            .unwrap();
        let text = r["result"]["content"][0]["text"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        // chunk 无 media → found:false（无媒资），但非协议错误。
        assert_eq!(parsed["found"], false);
    }
}
