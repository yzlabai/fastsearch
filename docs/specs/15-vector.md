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

**不做**：嵌入计算（embed 模块）；RaBitQ 量化 / filtered-traversal（下一迭代）；CDC 自动写穿 PG embedding（下一迭代）；**多向量 MaxSim（ColPali，M2/MM11）`gated`**——只在引擎派生层、不入 PG 真源（不变量 #1），待多模态模型与规模信封。当前后端全是**单向量**（文本嵌入产出；视觉/跨模态向量属 M1 gated，本 crate 不感知模态、只存 `VecMeta.modality` 供过滤下推）。

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
- RaBitQ 量化 / filtered-traversal（更优压缩/召回）；HNSW 大 N 的 p95 与暴力交叉点实测（见 [容量/SLO](../governance/2026-06-26-容量与SLO.md)）。
- pgvector 直查的 **CDC 自动写穿**（嵌入→PG embedding 列）下一迭代；当前需 embedding 已在 PG。
- 向量经 CDC 落地路径自动嵌入（`engine.set_embedder` + `apply_upsert`）或 `ingest_vector` 灌入。
