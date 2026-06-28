# spec · fastsearch-eval

> 模块 #8，依赖：fastsearch-core。阶段 P1+（贯穿）。上游：[产品设计 §3.7](../plans/2026-06-24-产品设计文档.md)、需求 F39（评测护栏）。
> 状态：**已落地**（nDCG/recall/MRR/precision + golden 集 + `assert_no_regression` CI 门禁；中英双 golden 接入 engine）。

## 1. 目的与范围

相关性质量评测体系——"完善产品"必备、MVP 最爱砍的部分。

- 评测指标：**nDCG@k / recall@k / MRR / precision@k**（纯函数，可测）。
- golden 集模型：`QueryJudgments`（query → 相关 doc 的 {gid → 相关度等级}）。
- `evaluate(results, judgments)`：对一批查询算各指标的均值。
- **回归门禁**辅助：`assert_no_regression(baseline, current, tol)` → 指标掉超过容差则失败（CI 用）。

**不做**：跑检索本身（engine）；golden 集的人工标注 UI；A/B 框架（后续）。

## 2. 数据结构

```rust
pub struct Judgments { /* query -> (gid -> grade u8) */ }   // grade: 0 不相关, 1.., 越大越相关
pub struct RankedResults { /* query -> [gid]（按排名） */ }
pub struct Metrics { pub ndcg, recall, mrr, precision: f64 }  // @k 均值
pub fn evaluate(results: &RankedResults, judg: &Judgments, k: usize) -> Metrics;
pub fn ndcg_at_k(ranked: &[Gid], grades: &HashMap<Gid,u8>, k) -> f64;
pub fn recall_at_k(...) -> f64; pub fn mrr(...) -> f64; pub fn precision_at_k(...) -> f64;
```
（用 core::GlobalId 作 gid。）

## 3. 行为规约

- **nDCG@k**：DCG = Σ (2^grade-1)/log2(i+2)（i 从 0），IDCG 用理想排序；nDCG=DCG/IDCG，IDCG=0 时 nDCG=0。
- **recall@k**：top-k 命中的相关数 / 总相关数（grade>0）；无相关→0。
- **precision@k**：top-k 中相关数 / k。
- **MRR**：第一个相关命中的 1/rank（rank 从 1）；无→0。
- `evaluate`：对每个有判定的 query 算指标，取均值（无判定的 query 跳过）。
- **确定性**、不 panic（空输入→0）。

## 4. 依赖

`fastsearch-core`。

## 5. 测试用例

1. nDCG：已知排序 + 等级 → 手算对照；完美排序 nDCG=1；逆序 <1；IDCG=0→0。
2. recall@k：部分命中比例正确；k 截断生效；无相关→0。
3. precision@k、MRR：已知用例对照；第一个相关在第 3 位→MRR=1/3。
4. evaluate：多 query 求均值；跳过无判定 query。
5. assert_no_regression：掉点超容差→Err，未超→Ok。
6. 边界：空 ranked / 空 judgments / k=0 不 panic。

## 6. 验收标准与状态

- [x] v1 完成：nDCG@k / recall@k / precision@k / MRR + evaluate（多 query 均值）+ assert_no_regression 门禁 + 7 单测绿（含手算对照、完美/逆序、边界）。clippy 净、fmt 净。
- [x] v2 完成（F39 闭环）：**golden 集 JSON 加载** `GoldenSet { collection, corpus: Vec<Chunk>, queries: Vec<GoldenQuery{query, relevant: cid→grade}> }`（复用 `Chunk` serde + `GlobalId::parse` 解 citation_id key）+ `from_json` / `judgments()`；`Metrics` 加 serde 便于落 baseline。**接入 engine 跑真检索**落在 `engine::golden::run`（守住分层：eval 不跑检索）。**CI 回归门禁**：固定 golden 集（`crates/fastsearch-engine/tests/golden/zh_finance.json` 中文财报语料 + Jieba）+ 提交 baseline，`relevance_gate.rs::no_regression_against_baseline` 断言不掉点（tol 0.02）；改语料/排序后用 `--ignored` 的 `probe_print_metrics` 重算 baseline。eval +2 单测（加载/坏 citation），engine +1 门禁测试。

**已知限制 / 下一迭代：**
- 门禁用 `SearchMode::Keyword`（确定性、零重依赖，适合 CI）；hybrid/vector 门禁待真语义嵌入落地（HashEmbedder 非语义，纳入门禁无意义）。
- golden 集仍小（演示闭环为主）；扩充语料/查询、CLI 暴露 `fastsearch eval`、CI workflow 显式跑门禁为后续。
