# image-only query 跳过词项 rerank（保住视觉召回序）

> 日期：2026-08-24
> 状态：**已实施 + 活服务验证 done**（2026-08-24）
> 来源：[FastGPT 知识库与多模态检索参考建议](2026-08-24-fastgpt知识库与多模态检索参考建议.md) §4.4 / P0-3
> 相关 spec：[14-engine](../specs/14-engine.md)、[21-rerank](../specs/21-rerank.md)

## 1. 要做什么

`Engine::run()` 的 rerank 段在 **文本 query 为空**（以图搜图：`query=""` + `query_image`/`vector`）
时，必须**跳过重排、保持融合序**，而不是拿空 query 去打分再按分排序。

## 2. 为什么（当前的真实故障）

1. `LexicalOverlapReranker::rerank` 对空 query 直接返回全 0 分
   （[rerank/src/lib.rs:77-81](../../crates/fastsearch-rerank/src/lib.rs#L77-L81)：`q.is_empty()` → `vec![0.0; n]`）。
2. `run()` 随后无条件按 rerank 分降序排序，同分按 `GlobalId` 升序 tie-break
   （[engine/src/lib.rs:1744-1750](../../crates/fastsearch-engine/src/lib.rs#L1744-L1750)）。
3. 全 0 分 ⇒ 全部同分 ⇒ 最终顺序 = **gid 升序**，视觉相似度排名被完全摧毁。

这与仓内已修的 H1-A 是同一类故障（向量独有命中拿空串打分 → 全 0 → gid 序），
当时的修法是"回真源取正文"；但**空 query 这一侧没有修**——正文再全也救不了没有 query。

`Reranker` trait 的签名是 `rerank(&self, query: &str, candidates: &[String])`，
**结构上就只能吃文本**：因此"没有文本 query"时，任何该 trait 的实现都不可能给出有意义的分。
这不是某个 reranker 的实现缺陷，是编排层该拦的。

FastGPT 的同类编排显式规定"视觉结果不进文本 reranker、重排失败保持原召回序"，可作旁证。

## 3. 怎么做

`run()` 中把 rerank 段整体门控在"文本 query 非空"之下：

```rust
if let Some(spec) = &req.rerank {
    if req.query.trim().is_empty() { /* 跳过：保持融合序，rerank 保持 None */ }
    else { ...原逻辑... }
}
```

- **整段跳过**（含 `hits.truncate(spec.top_k)` 重排窗口）：`rerank.top_k` 是"喂给昂贵重排器的候选数"
  这一**成本钮**，重排器没被调用就不该用它裁结果；结果条数仍由 `req.top_k` 收口。
- 命中的 `rerank` 字段保持 `None`（响应里是 `null`）——**可观测的诚实**：调用方能看出这次没重排。
- 不报错：以图搜图的调用方带着 rerank 一起发是常见用法，报 400 是破坏性 API 变更。

## 4. 不做什么（明确排除）

- **不改** `GlobalId` tie-break 约定（CLAUDE.md 不变量 §4）。
- **不加**"全部同分则保持原序"的通用兜底：query 非空但被分词器切空（如纯标点、
  分词器不认的语种）时仍会退化为 gid 序。这是**同源但更宽**的问题，改法要动 tie-break 语义，
  留作下一迭代（见 §7）。
- **不引入** `Reranker` 的 capability 协商（多模态 reranker 落地时再做，届时按 caps 显式开启）。
- 不动 server / CLI / SDK 契约。

## 5. 用户使用例子

```bash
# 以图搜图 + 顺手带了 rerank：修复前视觉排名被 gid 序覆盖，修复后保持视觉相似度序
curl -s localhost:8642/v1/search -H 'x-api-key: dev' -H 'content-type: application/json' \
  -d '{"query":"","mode":"vector","vector":[...],"top_k":5,
       "rerank":{"model":"lexical","top_k":10}}' | jq '.hits[] | {chunk_id, vector, rerank}'
# 期望：hits 按 .vector 降序；.rerank 全为 null
```

## 6. 测试用例（验收标准）

`crates/fastsearch-engine/src/lib.rs` 单测，与既有 `rerank_uses_real_text_for_vector_only_hits`（H1-A）并列：

1. `image_only_query_skips_lexical_rerank`：
   - 灌入 ≥3 个 chunk，构造"向量序 ≠ gid 序"的局面（**必须**如此，否则测试无法证伪）。
   - `query=""` + `mode=Vector` + `req.vector` + `rerank=Some(...)`。
   - 断言：命中顺序 == 不带 rerank 时的顺序（视觉序），且每条 `hit.rerank.is_none()`。
2. 回归保持：`rerank_reorders_by_overlap`、`rerank_uses_real_text_for_vector_only_hits`、
   `rerank_top_k_caps_window` 三条既有测试**不变、仍绿**（证明只动了空 query 这一支）。
3. 收口：`cargo fmt --all --check` + `cargo clippy --workspace --all-targets -- -D warnings`
   + `cargo test --workspace` 全过。

## 6.1 实际验收结果（2026-08-24）

- 单测：`image_only_query_skips_lexical_rerank` 绿；**去掉修复即红**（视觉序 `[3,1,2]` 塌成 gid 序
  `[1,2,3]`），证伪力已验证。三条既有 rerank 测试不变仍绿。
- 收口：fmt 净、clippy `-D warnings` **0 告警**、`cargo test --workspace` **343 passed / 0 failed**
  （带 `DATABASE_URL`，PG/CDC 集成测试实跑非跳过）。
- **活服务验证**（实跑 `fastsearch-server` 二进制 + curl，三条预计算向量令"向量序≠gid 序"）：

  | 臂 | 请求 | 结果 |
  |---|---|---|
  | A 基线 | `query=""` + vector，不带 rerank | `[3, 2, 1]`（视觉序） |
  | B 修复 | 同上 + `rerank{lexical,10}` | `[3, 2, 1]`，`rerank` 全 `null` ✅ 与 A 逐条相同 |
  | C 对照 | `query="gamma"` + rerank | chunk3 得 1.0 居首 ✅ 正常重排路径未被误伤 |

- **C 臂顺带给出了 §7 那条已知限制的实证**：chunk 1 与 2 都得 0 分（与 "gamma" 无词项重叠），
  二者顺序退化为 gid 序、向量序丢失。这正是"分词器切空"那类问题的同一形态——
  当前修复只覆盖 query 为空这一支，此处如实记账。
- OpenAPI 契约已同步（请求侧说明空 query 忽略 rerank、响应侧说明 `rerank: null` 的两种含义），
  经 `GET /openapi.json` 实取确认渲染正确。

## 7. 下一迭代

- query 非空但分词后为空 ⇒ 仍退化 gid 序。要根治需把"重排无信息量"变成显式信号
  （reranker 返回 `Option<Vec<f64>>`，或 trait 加 `informative(query) -> bool`），
  再统一"无信息量 → 保持原序"。属契约变更，单独立项。
- 多模态 reranker 落地后，把本次的"query 为空则跳过"升级为"按 reranker caps 选择"。
