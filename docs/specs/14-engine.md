# spec · fastsearch-engine

> 模块 #9（整合层），依赖：core/text/vector/rerank/sync/embed/pg。阶段 P1→P2。
> 上游：[产品设计 §3.4 排序管线](../plans/2026-06-24-产品设计文档.md)。状态：**已完成 v2.3**
> （三向量后端档 + 深分页 + 重建 + 媒资解析 + 多模态 + 崩溃安全 CDC）。

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
- [x] v1.7（**派生索引持久化 + 崩溃安全 CDC 检查点，Docker PG 验证 done**，2026-06-25）：`open(data_dir)→(Engine, applied_lsn)` / `persist(data_dir, lsn)`（text.commit + 向量原子落盘 + `checkpoint.json`）/ `consume_once(cfg, applier, data_dir)`（**peek 不推进 → 应用 → persist → 落盘后才 advance_slot**，崩溃任意点不丢/不重）。集成测试 `cdc_consume_persist_crashsafe`：消费→落盘→重启→检查点续传、向量不重嵌、slot 不重发、幂等。详见 [计划](../plans/2026-06-25-派生索引持久化与崩溃安全.md)。
- [x] v1.6（**完整产品主循环，PG+Ollama 双 env 验证 done**，2026-06-25）：`set_embedder` —— 设置后 **CDC 落地路径 `IndexSink::apply_upsert` 自动嵌入 chunk 正文 → 写向量索引**。至此 `PG 写 → 逻辑复制 → pgoutput 解码 → 嵌入 → 派生 BM25+向量 → 混合检索` 主循环完整成立。env-gated 全链路测试 `cdc_embed_hybrid_full_loop`（写 PG → CDC → 嵌入 → 语义 vector 检索词面不重叠查询命中）；两 CDC 集成测试经静态 `tokio::Mutex` 串行（共享 publication/表）。
- [x] v1.5：**more_like_this**（`Engine::more_like_this(gid, top_k, acl)`）—— 取种子 chunk 正文（`TextIndex::stored_text`）净化成 keyword 查询（剔元字符、取前 20 词）反查相似、排除种子自身、ACL 强制。REST `POST /v1/similar`（按 citation_id）。+2 单测（engine/server）。
- [x] v1.4：**分组折叠**（`req.collapse: Collapse{field,max_per_group}`，core 新增类型 + 校验）—— 按最终排名每组（`doc_id`/`section_id`）至多保留 N 条，防单文档/单段刷屏；保序、确定性；未知 field 不折叠。server 经 SearchRequest serde 透传。+1 单测。
- [x] v1.3：**auto-merging**（`req.auto_merge`）—— 融合后、rerank 前，把同 `(doc_id, section_id)`（`section_id!=0`）的多个命中片段归并为组内最高排名的代表，被并入的兄弟 chunk_id 记入 `SearchHit.merged_chunk_ids`（升序、答案层可解析整段全部引用）；保序、确定性。`section_id==0` 视为"无段"不并。server 响应透出 `merged_chunk_ids`。+2 单测。
- [x] v2.0（2026-06-26）：**多向量后端**——`vector` 字段改 `VectorStore` 门面（Brute/HNSW），
  `create_in_ram_with(cfg, backend)` 选档，`Checkpoint.vector_backend` 记录/`open_with` 恢复；
  **pgvector 直查**经 `set_pg_vector(Arc<PgStore>)`：`run()` 向量召回用 `block_in_place` 桥接 PG
  异步 ANN（要求 multi-thread runtime），命中引用取 PG 真实 page/bbox。
- [x] v2.1（A8b）：**深分页 `search_after`**——`SearchHit::cursor()`（排序键 bits+citation_id）+
  `run()` 末端按"严格在游标之后"过滤后截 top_k；与 fusion/rerank 最终排序一致、平铺无重叠/遗漏。
- [x] v2.2（A14）：**单集合原地重建** `rebuild_from(rows)`——清空 text+vector → 从真源重灌 → commit。
- [x] v2.3（MM6）：**媒资解析** `resolve_citation(cid, acl) -> Option<ResolvedAsset{fetch,time,media_type}}`
  （`AssetFetch::DocRender/SignedUrl/InlineRef`），ACL 强制（不可见/不存在均 None=404）。
- [x] v2.5（MM6-inline，2026-06-27/28，**Docker 真机验证**）：`source_pg` 真源句柄 + `set_source_store`。
  `resolve_citation` 的 `Inline` 路径只**定位**（ACL 强制）→ `AssetFetch::InlineRef`（不随 resolve 取字节，
  省一次 PG 读、便于签短时 URL，MM6-signer S1）；字节经 **`fetch_inline_bytes(cid)`**（无 acl，已授权出口用：
  authed 网关先过 resolve ACL / token 端点已验签）从 PG `media_bytes` 真源 `block_in_place` 直查（multi-thread
  runtime，同 B6）。集成 `mm6_inline_serves_bytes_from_source_pg`（授权 InlineRef + fetch_inline_bytes 吐真源字节、
  越权 None）；本环境 `fetch_inline_bytes_without_source_pg_is_none`。
- [x] v2.6（MM6-secure，2026-06-27）：`ObjectSigner` trait + `set_object_signer`；`resolve_citation` 的 `Object`
  路径**必须经签名器签短时 URL，未配签名器 → None（404），绝不回退裸 key**（堵不变量 #3 漏洞）。单测
  `mm6_secure_object_no_signer_is_404` / `mm6_secure_object_with_signer_signs`（签名 URL 不含 `s3://`）。
  真签名器（S3 presign 类）gated 对象存储。
- [x] 多模态（MM1-7）：`vec_meta` 透出 modality/time/media；分面支持 modality；kind Audio/Video。
- [x] v2.4（MM5，M0 路由，2026-06-27）：CDC `apply_upsert` 嵌入按"可检索文本表示"（`chunk.text`，含 caption/转录）；
  **无文本媒资（`text==""`）跳过向量嵌入/写入**（否则空串塌成退化向量污染 ANN），幂等覆盖时删旧向量。
  无文本媒资仍在 BM25 + modality fast field。单测 `mm5_textless_media_skips_vector`。真 CDC 端到端音视频召回需 PG（`待运行验证`）；M1 图像向量路由 `gated`。

**已知限制 / 下一迭代：**
- ✅ auto-merging（v1.3）、rerank 钩子、CDC 自动 embedding（v1.6）、search_after（v2.1）、单集合重建（v2.2）均已实现。
- 引擎并发去串行（Mutex→RwLock/副本）为后续——影响 server CDC 与检索的串行（见 19-server / [容量·SLO](../governance/2026-06-26-容量与SLO.md)）。
- ✅ pgvector 直查的 **CDC 自动写穿已落地**（2026-06-27，B6 续作）：`apply_upsert` 在 `set_pg_vector` 模式把嵌入写回 PG `embedding` 列（`set_embedding`），列清单 publication 排除派生列 + 幂等守卫双防线断 CDC 反馈环。Docker pgvector 真机验证。详见 [12-pg spec §7 v1.5](12-pg.md)、[devlog](../devlog/2026-06-27-B6-CDC写穿与断反馈环.md)。
- ✅ **M1 图像嵌入路由基线已落地**（2026-06-27，MM10）：`apply_upsert` 对无文本但 `caps.image`+inline 字节的 chunk 走图像嵌入；以图搜图 `query_image`（MM9）。真视觉模型/跨模态 gated。
