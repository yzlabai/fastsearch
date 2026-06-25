# spec · fastsearch-rerank

> 模块 #11，依赖：fastsearch-core。阶段 P2/P3。上游：[产品设计 §3.4 排序管线](../plans/2026-06-24-产品设计文档.md)、需求 F15。
> 状态：**开发中**。

## 1. 目的与范围

排序管线的"宽召回 → rerank → top-K"最后一环。

- `Reranker` trait：对 (query, 候选文本列表) 打分重排。
- `LexicalOverlapReranker`：**确定性、零依赖**的词项重叠（Jaccard）reranker——可测、作基线/fallback。
- 引擎集成：`req.rerank` 存在时，对融合后的候选取文本 → rerank → 重排 top-K。

**架构决策（2026-06-25，从第一性原理）**：**RAG 主路径默认不上神经 cross-encoder**。理由：rerank 本质是"用不可索引的高保真打分器只重排 N 个候选"，而本产品答案层是**外部 LLM**——它读 top-K 时本就做全交叉注意力的联合打分（最高保真），检索侧再放神经 rerank 多为**重复劳动**。正确做法：stage-1 拉满 recall@N（融合 + 真语义嵌入）+ 略大 top-K 交给 LLM 做最终精排。`Reranker` trait 保留为**可选精度档**，服务"无 LLM 兜底"的入口（CLI/库/REST 直接给人/非-LLM 客户端看精确 top-3），届时优先**纯 Rust 轻量 LTR**（特征化、可解释、用 eval golden 训练），而非神经 cross-encoder。

**不做**：进程内神经 cross-encoder（Candle/ort）；rerank 批处理/缓存（后续）。

## 2. 公开接口

```rust
pub trait Reranker {
    /// 返回每个候选的相关分（与输入同序）。
    fn rerank(&self, query: &str, candidates: &[String]) -> anyhow::Result<Vec<f64>>;
}
pub struct LexicalOverlapReranker;
```

## 3. 行为规约

- `LexicalOverlapReranker`：分 = |query_tokens ∩ doc_tokens| / |query_tokens ∪ doc_tokens|（Jaccard，小写、按非字母数字切词）。query 空 → 全 0。
- 确定性、不 panic；候选空 → 空。
- 引擎用法：rerank 分**替换**最终排序键（重排），但保留原 bm25/vector/fused 分在命中里；同分 tie-break 按 gid。

## 4. 依赖

`fastsearch-core`、`anyhow`。

## 5. 测试用例

1. 词项重叠：query 与候选完全重叠→1，无重叠→0，部分→Jaccard 值对照。
2. 重排：候选按 rerank 分降序；同分按 gid。
3. 空 query / 空候选不 panic。
4. 引擎集成：req.rerank 时，与 query 词项更重叠的命中被提前。

## 6. 验收标准与状态

- [x] v1 完成：Reranker trait + LexicalOverlapReranker（3 单测）+ 引擎接入（`set_reranker`、req.rerank 时宽召回后重排、rerank 分写入命中）+ engine/server 透出 + 活服务验证（"apple banana" → chunk2 rerank=1.0 居首）。clippy 净、fmt 净。

**下一迭代（仅"无 LLM 兜底"入口需要时）：** 纯 Rust **轻量 LTR**（线性/小 GBDT over bm25/vector/heading 命中/精确短语/proximity 等特征，用 eval golden 训练，确定性、可解释、可 CI）经 `set_reranker` 注入；rerank 批处理/缓存。**不做**进程内神经 cross-encoder。
