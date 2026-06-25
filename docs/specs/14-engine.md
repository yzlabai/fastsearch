# spec · fastsearch-engine

> 模块 #4.5（整合层），依赖：fastsearch-core、fastsearch-text、fastsearch-sync。阶段 P1→P2。
> 上游：[产品设计 §3.4 排序管线](../plans/2026-06-24-产品设计文档.md)。状态：**开发中**。

## 1. 目的与范围

把已建模块整合成端到端可检索引擎：

- 持有派生索引（当前 text；P2 加 vector）。
- 实现 `sync::IndexSink`（适配器）：CDC 变更落到 text 索引，避免 text 反依赖 sync。
- `search(req, acl)`：执行排序管线（ACL 强制注入 → keyword∥semantic 召回 → 融合 → 组装命中）。当前 keyword 全量可用；vector/hybrid 在 vector 后端落地前回退到 keyword（显式标注）。
- 命中结构 `SearchHit`：id + citation + 融合分 + 各路分。

**不做**：REST/MCP（server）、向量后端实现（vector）、PG 连接（pg/sync 集成层）。

## 2. 公开接口

```rust
pub struct Engine { /* TextIndex（+later VectorBackend） */ }
pub struct SearchHit { pub id: GlobalId, pub score: f64, pub citation: Citation,
                       pub bm25: Option<f32>, pub vector: Option<f64> }

impl Engine {
    pub fn create_in_ram(cfg: TextIndexConfig) -> Result<Self>;
    pub fn ingest(&mut self, collection: &str, chunk: &Chunk) -> Result<()>; // upsert+不提交
    pub fn commit(&mut self) -> Result<()>;
    pub fn search(&self, req: &SearchRequest, acl: Option<&AclFilter>) -> Result<Vec<SearchHit>>;
}
impl fastsearch_sync::IndexSink for Engine { ... }   // CDC 落地
```

## 3. 行为规约（排序管线）

1. `req.validate()`。
2. **ACL 强制注入**：acl 传给 text.search（text 内部预过滤 + 精确 visible 判定，不可绕过）。
3. **召回**：
   - mode=Keyword：text.search(query, filter, acl, candidates) → bm25 候选。
   - mode=Vector/Hybrid：vector 后端未接入前**回退到 keyword**（返回结果但标注 vector=None；spec 记为已知限制，P2 补真混合）。
4. **融合**：keyword 单路时直接按 bm25 降序；hybrid 时用 core::fuse（待 vector）。
5. **组装**：取 top_k，产出 SearchHit（citation 来自 text 命中）。
6. 确定性：同库同请求结果一致（继承 text/core 的 tie-break）。

## 4. 测试用例

1. ingest 多个 chunk + commit + search（keyword）→ 命中正确、带 citation（page/bbox/heading_path）。
2. 经 `IndexSink`（Applier 驱动一批 ChangeEvent）灌入 → search 得到对应结果（端到端 CDC→索引→检索）。
3. ACL：注入 acl 后越权 chunk 不出现。
4. doc_id 级替换：DeleteDoc + Upsert 后旧 chunk 消失、新出现。
5. validate 失败（top_k=0）→ Err。
6. mode=Hybrid 当前回退 keyword：不报错、返回 keyword 结果、vector=None。

## 5. 验收标准与状态

- [x] v1 完成：Engine + `IndexSink` 适配 + 全文/向量/**真混合**排序管线 + 9 端到端测试绿（含 CDC→索引→检索、ACL 强制、doc 级替换、过滤、real_hybrid 融合、vector_only、校验失败）。clippy 净、fmt 净。
- [x] v1.1：接入 fastsearch-vector，mode=Hybrid 走 keyword∥vector → core::fuse；mode=Vector 纯向量；过滤/ACL 两路各自真预过滤。
- [x] v1.2：`engine::golden::run(set, cfg, mode, k)` —— 把 eval `GoldenSet` 语料灌入内存引擎、对每个查询跑真实检索、用判定算 `Metrics`，承接 eval 的"跑检索"那步（eval 不反依赖 engine，分层不破）。配套 `tests/relevance_gate.rs` 回归门禁（见 [eval spec §6 v2](18-eval.md)）。
- [x] v1.5：**more_like_this**（`Engine::more_like_this(gid, top_k, acl)`）—— 取种子 chunk 正文（`TextIndex::stored_text`）净化成 keyword 查询（剔元字符、取前 20 词）反查相似、排除种子自身、ACL 强制。REST `POST /v1/similar`（按 citation_id）。+2 单测（engine/server）。
- [x] v1.4：**分组折叠**（`req.collapse: Collapse{field,max_per_group}`，core 新增类型 + 校验）—— 按最终排名每组（`doc_id`/`section_id`）至多保留 N 条，防单文档/单段刷屏；保序、确定性；未知 field 不折叠。server 经 SearchRequest serde 透传。+1 单测。
- [x] v1.3：**auto-merging**（`req.auto_merge`）—— 融合后、rerank 前，把同 `(doc_id, section_id)`（`section_id!=0`）的多个命中片段归并为组内最高排名的代表，被并入的兄弟 chunk_id 记入 `SearchHit.merged_chunk_ids`（升序、答案层可解析整段全部引用）；保序、确定性。`section_id==0` 视为"无段"不并。server 响应透出 `merged_chunk_ids`。+2 单测。

**已知限制 / 下一迭代：**
- ✅ auto-merging（section 归并）已实现（v1.3）；rerank 钩子已接入（宽召回→rerank→top-K）。
- 向量经 `ingest_vector` 灌入；CDC 自动 embedding 回填待 embed 模块（P2）。
