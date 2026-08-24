# spec · fastsearch-rerank

> 模块 #7，依赖：fastsearch-core。阶段 P2/P3。上游：[产品设计 §3.4 排序管线](../plans/2026-06-24-产品设计文档.md)、需求 F15。
> 状态：**已落地基线**（`Reranker` trait + `LexicalOverlapReranker` 确定性基线 + `set_reranker` 注入）。神经 cross-encoder **决定不做**（ADR）；轻量 LTR 仅"无 LLM 兜底"入口需要时做（下一迭代）。

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
- **本 trait 结构上只吃文本**（`rerank(query: &str, candidates: &[String])`）：**没有文本 query 时，
  任何实现都给不出有意义的分**。因此"要不要重排"的判断归**编排层**，不归 reranker——
  reranker 对空 query 返回全 0 是正确行为，`Engine::run()` 负责在 `req.query` 去空白为空时
  （纯图片检索）**整段跳过重排**，保持融合/视觉序、命中 `rerank` 留 `None`。
  否则全 0 → 全同分 → 按 gid tie-break → 视觉相似度序被整体压平（2026-08-24 已修，见
  [14-engine spec v2.9](14-engine.md)、[plan](../plans/2026-08-24-image-only-query跳过词项rerank.md)）。

## 4. 依赖

`fastsearch-core`、`anyhow`。

## 5. 测试用例

1. 词项重叠：query 与候选完全重叠→1，无重叠→0，部分→Jaccard 值对照。
2. 重排：候选按 rerank 分降序；同分按 gid。
3. 空 query / 空候选不 panic。
4. 编排层：纯图片 query（`query=""` + 查询向量）带 rerank 时，结果顺序与不带 rerank **逐条相同**，
   且命中 `rerank` 全为 `None`（engine 侧 `image_only_query_skips_lexical_rerank`）。
4. 引擎集成：req.rerank 时，与 query 词项更重叠的命中被提前。

## 6. 验收标准与状态

- [x] v1 完成：Reranker trait + LexicalOverlapReranker（3 单测）+ 引擎接入（`set_reranker`、req.rerank 时宽召回后重排、rerank 分写入命中）+ engine/server 透出 + 活服务验证（"apple banana" → chunk2 rerank=1.0 居首）。clippy 净、fmt 净。

- [x] v1.1（2026-08-24）：明确"空文本 query 不重排"的**编排层**契约（见 §3 末条），engine 侧落地 +1 回归测试。

**同族发现的谱系**（2026-08-24 复核时理清，供后来人判断残余风险）：`全 0 分 → 全同分 → 按 gid
tie-break → 真实排名被压平` 这一机制，在 [2026-07-05 代码审查报告](../reviews/2026-07-05-代码审查报告.md)
的 **H1「请求 rerank 反而摧毁排序」**里已被认定为高危，但当时只列出两条链路：

| 链路 | 触发 | 状态 |
|---|---|---|
| H1-A | 向量独有命中不在 `text_map` → 候选文本为空串 | 已修（rerank 前回真源取 `stored_text`） |
| H1-B | CJK 整句成单 token → 中文候选 Jaccard 恒 0 | 已修（CJK 字符 bigram 切分） |
| **H1-C** | **query 本身为空**（纯图片检索） | **2026-08-24 修（本次）** |

H1-C 之所以从那轮 40+ 处发现的深度审查里漏掉，是因为它恰好落在两个 finding 的**交集**上：
同轮的 **M7** 修的是"空 query + query_image 在 Hybrid 下 keyword 路退化成 match-all"——**召回侧**的
空 query；H1 修的是**非空 query** 下的 rerank 打分。"空 query × rerank"两边都不覆盖，于是存活。

**已知限制：** query 非空但被分词器切空（纯标点、分词器不认的语种）时，仍返回全同分 → 退化 gid 序。
**同一形态在正常路径上也可观察到**（2026-08-24 活服务实测）：query="gamma" 时，两条与之无词项重叠的
候选同得 0 分，其相对顺序退化为 gid 序、向量序丢失——即"部分候选同分"与"全部候选同分"是一个问题的
两种程度，当前只修了后者中 query 为空的那一支。
根治需把"重排无信息量"变成显式信号（`Option<Vec<f64>>`，或 trait 加 `informative(query) -> bool`），
属 trait 契约变更，单独立项。多模态 reranker 落地后，"query 为空则跳过"应升级为"按 reranker caps 选择"。

**下一迭代（仅"无 LLM 兜底"入口需要时）：** 纯 Rust **轻量 LTR**（线性/小 GBDT over bm25/vector/heading 命中/精确短语/proximity 等特征，用 eval golden 训练，确定性、可解释、可 CI）经 `set_reranker` 注入；rerank 批处理/缓存。**不做**进程内神经 cross-encoder。
