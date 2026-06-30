# spec · fastsearch-vector

> 模块 #5，依赖：fastsearch-core。阶段 P2。上游：[产品设计 §3.3](../plans/2026-06-24-产品设计文档.md)、需求 F10–F13。
> 状态：**已完成 v2.1**（暴力 + HNSW/u8 量化 + pgvector 直查三档）。VecMeta 另含多模态
> `modality/time/media`（MM1/MM4）。

## 1. 目的与范围

引擎侧向量检索后端。

- `VectorBackend` trait：upsert/delete/search（带 filter + ACL 的 **filter-aware 召回**）。
- `MemVectorIndex`：内存暴力余弦（精确、filter-aware）——正确、可测、无需模型；适合中小集合，也作正确性基线。
- **真预过滤**：过滤/ACL 在打分前/打分中施加（非后过滤），这正是超越 pgvector 后过滤召回崩的点。

三个后端档（同 `VectorBackend` trait）：`MemVectorIndex`（暴力，默认确定）、`HnswVectorIndex`
（HNSW+u8 量化，A9，大规模近似）、**pgvector 直查**（ANN 在 PG 跑，B6，经 `fastsearch-pg::PgStore::vector_search`）。

**不做**：嵌入计算（embed 模块）；旋转档 engine 接线 / filtered-traversal（下一迭代；二值粗筛 + RaBitQ 估计器 + 随机旋转**已落地**于 vector crate，见 §6）；CDC 自动写穿 PG embedding（**已落地**，见 §6 已知限制 + [pg spec](12-pg.md)）；**多向量 MaxSim（ColPali，M2/MM11）`gated`**——只在引擎派生层、不入 PG 真源（不变量 #1），待多模态模型与规模信封。当前后端全是**单向量**（文本嵌入产出；视觉/跨模态向量属 M1 gated，本 crate 不感知模态、只存 `VecMeta.modality` 供过滤下推）。

## 2. 公开接口

```rust
pub trait VectorBackend {
    fn upsert(&mut self, gid: GlobalId, vector: Vec<f32>, meta: VecMeta) -> anyhow::Result<()>;
    fn delete(&mut self, gid: &GlobalId) -> anyhow::Result<()>;
    fn delete_doc(&mut self, collection: &str, doc_id: &str) -> anyhow::Result<()>;
    /// filter-aware 余弦近邻：先按 filter+acl 过滤候选，再算分取 top-k。
    fn search(&self, query: &[f32], k: usize,
              filter: Option<&Filter>, acl: Option<&AclFilter>) -> anyhow::Result<Vec<Scored>>;
}
/// 过滤/ACL/引用所需的随项元数据（实现 core::FieldSource）。
pub struct VecMeta { pub kind, doc_id, collection, tenant, page, section_id, heading_path, acl, bbox, chunk_id }
```

- 距离：余弦（向量入库时归一化，内积即余弦）。返回 `Scored{id, score∈[-1,1]}`。

## 3. 行为规约

- **filter-aware**：先用 `Filter::eval` + `AclFilter::visible` 筛候选，再算余弦、取 top-k。保证选择性强的过滤不掉召回（对位 pgvector 后过滤坑）。
- **upsert 幂等**：同 gid 覆盖。
- **delete_doc**：删 collection+doc_id 全部项。
- **确定性**：同分 tie-break 按 gid 升序。
- **维度校验**：query 维度与库不一致 → 显式错误。
- **健壮**：空库返回空；零向量/NaN 防护（norm=0 时跳过或置 0 分）。

## 4. 依赖

`fastsearch-core`、`anyhow`、`hnsw_rs`（纯 Rust，无 C 依赖）、`serde(_json)`、`tempfile`。

## 5. 测试用例

1. upsert 3 向量 + search → 按余弦降序、top-k 截断、Scored.id 正确。
2. filter-aware：加 `kind=table` 过滤后只在 table 项里排（验证预过滤，不是先 top-k 再过滤）。
3. ACL：越权项不出现在结果。
4. upsert 覆盖：改向量后排序变化。
5. delete / delete_doc 生效。
6. 维度不匹配报错；空库空结果；零向量不 panic。
7. 确定性：同分按 gid。

## 6. 验收标准与状态

- [x] v1 完成：VectorBackend trait + MemVectorIndex（filter-aware 余弦，真预过滤）+ 7 单测绿（余弦排序/预过滤/ACL/覆盖/删除/维度校验/零向量）。clippy 净、fmt 净。已接入 engine 做真混合融合（engine 9 测试含 real_hybrid/vector_only）。
- [x] v1.1（2026-06-25）：**持久化** `MemVectorIndex::save/load`（JSON 快照 + 原子写 tmp→fsync→rename；存归一化向量，load 行为不变）+ `len/is_empty/dim`。+2 单测（往返、缺文件→空）。供 engine 落盘恢复（不重嵌）。压缩二进制格式（bincode）为后续优化。
- [x] v2（2026-06-26，A9）：**HnswVectorIndex**（hnsw_rs，纯 Rust）——增量 insert + 墓碑删除 +
  over-fetch 后过滤 + **u8 量化图（省 ~4× 图内存）+ 全精度 f32 重排**（recall@10≈0.99）+ 持久化
  （存向量、load 重建图）+ **小集合回退暴力**（≤1000 精确）。`VectorStore` 门面（在 engine）+
  `VectorBackendKind` 选档 + 检查点记录/恢复。诚实：HNSW 档近似 + 非确定（hnsw_rs 不可 seed），
  默认暴力档仍完全确定。
- [x] v2.1（2026-06-26，B6）：**pgvector 直查档**——`fastsearch-pg::PgStore::vector_search`
  （filter/ACL→SQL 下推 + iterative scan + Rust 精确后过滤 + 完整引用）；接 engine（block_in_place
  同步↔异步桥）+ server `FASTSEARCH_VECTOR_BACKEND=pgvector`。Docker 实测。

**已知限制 / 下一迭代：**
- ✅ **二值量化（1-bit）两阶段粗筛已落地**（2026-06-27，RaBitQ/BQ 核心）：`MemVectorIndex::with_binary_prefilter(oversample)` 开启——符号位 bit code（`binary.rs` `pack_signs`）Hamming 粗筛 top-`k·oversample`（`popcount`，~`d/64` 字操作 vs 精确 `d` flops）→ f32 精确重排，filter/ACL 仍在粗筛前（守 #5）、重排 + GlobalId tie-break 仍确定。+5 单测：全覆盖 oversample **逐条等于精确**、recall@10 ≥0.85(oversample=8)、filter-aware、pack/hamming 原语。**默认仍精确暴力**（`None`，零回归）。**✅ 已后端化**（2026-06-27）：`VectorBackendKind::BruteBinary(oversample)` + `VectorStore` 接线（`kind_str="brute_binary"`、与 brute 共享 on-disk f32 格式、load 后翻档）+ engine `open_with` 检查点恢复（记格式、oversample 取默认 `DEFAULT_BINARY_OVERSAMPLE=8`，同 HNSW 参数策略）+ server `FASTSEARCH_VECTOR_BACKEND=brute_binary`（`FASTSEARCH_BINARY_OVERSAMPLE` 调档）。+2 测试（VectorStore 落盘往返保持粗筛档；engine 重开恢复 `brute_binary` 覆盖默认）+ server 实跑 boot 200。
- ✅ **RaBitQ 无偏估计器粗筛已落地**（2026-06-28，替换对称 Hamming 为粗排打分）：粗筛改用 `binary::rabitq_estimate` = `⟨q, sign(x)⟩ / ‖x‖₁`——**用查询真实分量**（非对称）+ **逐向量 `‖x‖₁` 校正**（`Entry.l1`，由归一化向量派生、不落盘）。比 Hamming（只数符号一致维、丢 `q` 幅度）更接近真实余弦：同符号库向量 Hamming 必打平、估计器仍能按 `q` 幅度分开。只读 `code`（不取全精度 x），内存轻；估计降序 + GlobalId tie-break 仍确定；filter/ACL 仍在粗筛前（守 #5）；全覆盖 oversample 仍逐条等于精确。**实测**：recall@10 估计器 **0.87** vs Hamming **0.71**（oversample=4，~16pt 增益）。+2 单测（`estimate_separates_what_hamming_ties`、`rabitq_estimator_beats_hamming` 头对头 ≥+5pt 门禁）。devlog [2026-06-28](../devlog/2026-06-28-RaBitQ估计器.md)。
- ✅ **RaBitQ 随机旋转已落地**（2026-06-28 迭代②）：opt-in `MemVectorIndex::with_binary_prefilter_rotated(oversample)`——量化前对向量/查询做一次**数据无关固定种子正交变换**（`binary::Rotation`，高斯随机 + 改进 Gram-Schmidt；首次 upsert 惰性建矩阵）把信息摊匀，符号码更有信息 → 对**各向异性**（能量集中少数维）数据召回大增。正交不改内积 → 精排仍用原向量、全覆盖 oversample 仍逐条等于精确；固定种子 → 多副本/重开同矩阵（无需持久化）、确定。**实测**：各向异性集 recall@10 旋转 **0.97** vs 不旋转 **0.78**（oversample=3，~19pt 增益）。+3 单测（各向异性 A/B ≥+5pt、全覆盖=精确、确定性）。**搜索策略不落盘**（同 `binary_oversample`，调用方设；`load` 默认不旋转）。
- ✅ **旋转档 engine/server 接线已落地**（2026-06-28 迭代③）：`VectorBackendKind::BruteBinaryRotated(oversample)` + `VectorStore`（`kind_str="brute_binary_rotated"`、`load` 重建旋转矩阵 + 旋转空间重算 code）+ `MemVectorIndex::set_rabitq_rotation`（load 后翻档）+ engine `open_with` 检查点恢复（记 `brute_binary_rotated`）+ server `FASTSEARCH_VECTOR_BACKEND=brute_binary_rotated`（`FASTSEARCH_BINARY_OVERSAMPLE` 调档）。+2 测试（VectorStore 落盘往返保持旋转档；engine 重开恢复 `brute_binary_rotated` 覆盖默认）。**下一迭代**：查询分平面量化恢复 popcount 级粗筛速度。
- ✅ **HNSW 自适应过取（filtered-traversal 务实形式）已落地**（2026-06-28 迭代④）：强选择性 filter/ACL 下固定 `over_fetch` 候选会被过滤殆尽 → 召回崩；现有 filter/acl 时若过滤后不足 `k` 就**翻倍 `want` + 调高 `ef` 重搜**，上限=全集（最坏退化为对 filter 精确的全扫，守不变量 #5）。无 filter/acl 则单发不升级；同输入→同升级路径→同结果。+1 测试 `adaptive_overfetch_selective_filter_recall`（~1.7% 命中率 filter，recall@10 ≥0.85，对比固定过取仅 ~0.13）。**真·图内 filtered-traversal**（遍历时即过滤、免重搜）需 hnsw_rs 支持 → 下一迭代。
- ✅ **HNSW 墓碑自动压实已落地**（2026-06-30）：删除/更新只置墓碑（`entries[id]=None`、向量留图中），长跑高频 upsert/delete 下 `entries`/图原会无界增长。现删除/更新后按比例**自动压实**——`HnswVectorIndex::compact`（用活条目原地重建图 + 稠密 id 映射，等价 `save`→`load` 但纯内存、不落盘）经 `maybe_compact` 在「总槽位 > `COMPACT_MIN_TOTAL`(32) 且墓碑过半（dead > live）」时触发，「过半才压实」给摊还 O(1) 重建代价（仿动态数组倍增）。纯新增 dead=0 永不触发（不扰 bulk load）；活集/检索语义不变。亦可手动 `compact`。+2 单测（`auto_compaction_reclaims_tombstones` 跨阈值后槽位回落=活条目数、墓碑清零；`manual_compact_preserves_live_set` 暴力档压实前后 top-k 完全一致）。守不变量 #6（墓碑增长项）。
- HNSW 大 N 的 p95 与暴力交叉点实测（见 [容量/SLO](../governance/2026-06-26-容量与SLO.md)）；查询分平面量化恢复二值粗筛 popcount 级速度。
- ✅ pgvector 直查的 **CDC 自动写穿已落地**（2026-06-27，B6 续作）：engine `apply_upsert` 在配了 `set_pg_vector` 时把嵌入写回 PG `embedding` 列（`PgStore::set_embedding`，block_in_place 桥），而非引擎派生索引——直查读 PG、写也归 PG，闭环。**CDC 反馈环**经"列清单 publication 排除派生列（`embedding`/`embed_model`/`updated_at`）+ `set_embedding` 幂等守卫（0 行不复制）"双防线断开。Docker pgvector 验证（见 [pg spec §7](12-pg.md)、devlog）。
- 向量经 CDC 落地路径自动嵌入（`engine.set_embedder` + `apply_upsert`）或 `ingest_vector` 灌入。
