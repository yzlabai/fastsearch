# spec · fastsearch-vector

> 模块 #5，依赖：fastsearch-core。阶段 P2。上游：[产品设计 §3.3](../plans/2026-06-24-产品设计文档.md)、需求 F10–F13。
> 状态：**开发中**。

## 1. 目的与范围

引擎侧向量检索后端。

- `VectorBackend` trait：upsert/delete/search（带 filter + ACL 的 **filter-aware 召回**）。
- `MemVectorIndex`：内存暴力余弦（精确、filter-aware）——正确、可测、无需模型；适合中小集合，也作正确性基线。
- **真预过滤**：过滤/ACL 在打分前/打分中施加（非后过滤），这正是超越 pgvector 后过滤召回崩的点。

**不做**：嵌入计算（embed 模块）、量化/HNSW（下一迭代：RaBitQ + hnsw_rs）、pgvector 直查档（下一迭代 (a)）。

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

`fastsearch-core`、`anyhow`。（hnsw_rs/量化 下一迭代再加。）

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

**已知限制 / 下一迭代：**
- 暴力余弦 O(n)：中小集合够用，大库需 **HNSW（hnsw_rs）+ RaBitQ/int8 量化 + 全精度重排**（下一迭代）。
- **pgvector 直查档 (a)**（托管省事档，ANN 在 PG 跑）待 P2 接 pg。
- 向量目前由 `engine.ingest_vector` 灌入；CDC 的向量同步（embeddings 回填）待 embed 模块（P2）。
