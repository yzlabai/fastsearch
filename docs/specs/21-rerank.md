# spec · fastsearch-rerank

> 模块 #7，依赖：fastsearch-core。阶段 P2/P3。上游：[产品设计 §3.4 排序管线](../plans/2026-06-24-产品设计文档.md)、需求 F15。
> 状态：**FS-203 已完成**（类型化输入 + capability + 显式无信息量 + Engine 保序降级）。神经 cross-encoder **决定不做**（ADR）；轻量 LTR 仅“无 LLM 兜底”入口需要时做。

## 1. 目的与范围

排序管线的"宽召回 → rerank → top-K"最后一环。

- `Reranker` trait：声明可接受的 query/candidate 类型，对类型化输入打分或显式跳过。
- `LexicalOverlapReranker`：确定性、零依赖、**text-only** 的词项重叠基线。
- 引擎集成：只有能力匹配且后端返回完整有限分数时才提交重排；其他路径保留融合结果。

**架构决策（2026-06-25，从第一性原理）**：**RAG 主路径默认不上神经 cross-encoder**。理由：rerank 本质是"用不可索引的高保真打分器只重排 N 个候选"，而本产品答案层是**外部 LLM**——它读 top-K 时本就做全交叉注意力的联合打分（最高保真），检索侧再放神经 rerank 多为**重复劳动**。正确做法：stage-1 拉满 recall@N（融合 + 真语义嵌入）+ 略大 top-K 交给 LLM 做最终精排。`Reranker` trait 保留为**可选精度档**，服务"无 LLM 兜底"的入口（CLI/库/REST 直接给人/非-LLM 客户端看精确 top-3），届时优先**纯 Rust 轻量 LTR**（特征化、可解释、用 eval golden 训练），而非神经 cross-encoder。

**不做**：进程内神经 cross-encoder（Candle/ort）、真实图片 reranker、rerank 批处理/缓存。

## 2. 公开接口

```rust
pub enum RerankInputKind { Text, Image, TextImage }
pub struct RerankCaps { pub text: bool, pub image: bool, pub cross_modal: bool }
pub struct RerankInput<'a> { /* kind + optional text/image bytes */ }
pub enum RerankOutcome { Scores(Vec<f64>), Skipped(RerankSkipReason) }

pub trait Reranker {
    fn caps(&self) -> RerankCaps;
    fn rerank(
        &self,
        query: RerankInput<'_>,
        candidates: &[RerankInput<'_>],
    ) -> anyhow::Result<RerankOutcome>;
}
pub struct LexicalOverlapReranker;
```

`RerankInput` 用 `text`、`image`、`text_image` 构造器创建。图片字节允许暂缺，使编排层可先做 capability 裁定；真实图片后端接入时必须自行拒绝缺少的必要输入。

## 3. 行为规约

- `text=true` 允许 Text→Text，`image=true` 允许 Image→Image，`cross_modal=true` 允许不同类型或 TextImage 组合。
- `RerankCaps::admit` 统一区分 unsupported query/candidate；空白文本且无视觉输入仍按 Text 交给后端分词器裁定。
- Lexical caps 固定 `{text:true,image:false,cross_modal:false}`；ASCII/数字按非字母数字切词，连续 CJK 用重叠 bigram。
- query 分词为空返回 `Skipped(EmptyQueryTokens)`，不是全零分；候选空返回 `Scores([])`。
- Engine 先保留完整融合结果，只复制 `rerank.top_k` 窗口。capability 不匹配、显式 Skipped、后端 error、分数数量错误或 NaN/Inf 时丢弃临时窗口，保持未重排结果。
- 成功时 rerank 分为主排序键，融合分为二级键，GID 为最终键；等分不会覆盖融合相对顺序。
- rerank 游标额外编码融合分二级键，确保等分分页无重复/遗漏；旧游标继续接受，未重排游标格式不变。
- `explain=true` 且请求 rerank 时返回 `rerank_explain`；稳定 reason 为 `unsupported_query_modality`、`unsupported_candidate_modality`、`empty_query_tokens`、`backend_error`、`invalid_backend_output`。

## 4. 依赖

`fastsearch-core`、`anyhow`。

## 5. 测试用例

1. 词项重叠：query 与候选完全重叠→1，无重叠→0，部分→Jaccard 值对照。
2. image-only、纯标点和 text+image 混合候选均与未重排结果逐位一致，并给出稳定 skip 原因。
3. 后端 error、短数组和 NaN/Inf 输出不失败、不部分应用、不提前截断。
4. 有效重排仍能前移相关文本；等分沿用融合序并可用 search_after 完整平铺。
5. OpenAPI/REST/TS 类型与 Python/TS agent metadata 保留 `rerank_explain`。
6. 真实 server：纯标点基线与 rerank 均 `[3,2,1]`，正常文本 status=applied。

## 6. 验收标准与状态

- [x] v1 完成：Reranker trait + LexicalOverlapReranker（3 单测）+ 引擎接入（`set_reranker`、req.rerank 时宽召回后重排、rerank 分写入命中）+ engine/server 透出 + 活服务验证（"apple banana" → chunk2 rerank=1.0 居首）。clippy 净、fmt 净。

- [x] v1.1（2026-08-24）：明确"空文本 query 不重排"的**编排层**契约（见 §3 末条），engine 侧落地 +1 回归测试。

- [x] v1.2（2026-08-31，FS-203）：以类型化输入/caps 取代空 query 特判；整批准入由 rerank 模块统一裁定，空白纯文本仍进入 tokenizer；显式无信息量、事务式保序降级、类型化稳定 explain 原因、短数组/NaN/Inf 防护、融合二级键与等分可分页游标全部落地并经活服务验证。

**同族发现的谱系**（2026-08-24 复核时理清，供后来人判断残余风险）：`全 0 分 → 全同分 → 按 gid
tie-break → 真实排名被压平` 这一机制，在 [2026-07-05 代码审查报告](../reviews/2026-07-05-代码审查报告.md)
的 **H1「请求 rerank 反而摧毁排序」**里已被认定为高危，但当时只列出两条链路：

| 链路 | 触发 | 状态 |
|---|---|---|
| H1-A | 向量独有命中不在 `text_map` → 候选文本为空串 | 已修（rerank 前回真源取 `stored_text`） |
| H1-B | CJK 整句成单 token → 中文候选 Jaccard 恒 0 | 已修（CJK 字符 bigram 切分） |
| **H1-C** | **query 本身为空**（纯图片检索） | 2026-08-24 特判修；FS-203 升级为 caps |
| **H1-D** | **query 分词为空 / 候选模态不适用 / 后端错误或非法输出** | **FS-203 统一保序降级** |

H1-C 之所以从那轮 40+ 处发现的深度审查里漏掉，是因为它恰好落在两个 finding 的**交集**上：
同轮的 **M7** 修的是"空 query + query_image 在 Hybrid 下 keyword 路退化成 match-all"——**召回侧**的
空 query；H1 修的是**非空 query** 下的 rerank 打分。"空 query × rerank"两边都不覆盖，于是存活。

**已知限制 / 下一迭代：** 当前只有 Lexical text-only adapter；image/cross-modal capability 是可执行接口，但没有伪装成已接真实模型。`RerankSpec.model` 仍是请求/解释标签，Engine 每实例只有一个由 `set_reranker` 注入的 adapter，不做逐请求模型路由。真实图片 adapter 需要批量取得候选媒资字节；轻量 LTR、批处理、缓存和运行指标等待真实入口/负载证明需要。
