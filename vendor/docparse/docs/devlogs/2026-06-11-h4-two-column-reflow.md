# Devlog · H4:双栏左列段落重排(列感知 fill_x)

> 2026-06-11 · 里程碑 H4(见 [hardening-iteration.md](../plans/hardening-iteration.md))· 状态:✅ 验收过 · ⚠️ 高风险(改核心分组)

## 问题(M3 老债)

`group_blocks` 的续行判据要求"上一行触达 `fill_x`",而 `fill_x` 是**单一页宽
右缘**(`global_right - page.width*0.05`)。双栏页左列行的 x1 只到左栏右缘,
永远 < 页宽 fill_x → 左列每行各自成块、正文不聚段(右列正常,因为它的 x1
就是页宽右缘)。

## 方案:列感知 per-line fill_x

新增 [`column_fill_edges`](../../crates/docparse-core/src/layout.rs):返回每行
的列右缘减边距。`group_blocks` 改收 `&[f32]`(每行的列 fill_x),`Acc` 记块
起始行的列右缘,续行判据 `a.x1 >= a.fill_x`(本列右缘,而非页宽)。

**关键安全性质——单栏字节级零回归**:无显著 gutter 时 `column_fill_edges`
对每行返回同一个 `global_right - margin`,与旧 `fill_x` **完全相等**,所以
单栏页分组逐字节不变(单测 `single_column_keeps_one_global_edge` 钉死)。

gutter 检测**刻意从严**(避免单栏/图密页误判):
- < 6 行不分列;
- 候选分界 = 页中部 [30%,70%] 的行左缘;取**被最少行跨越**的那个;
- 要求两侧各 ≥25% 的行、跨越率 < 12%(真双栏 gutter 几乎无行跨越);
- 跨 gutter 的全宽行(标题/横幅)保留页宽全局右缘。

## 验收(高风险,全套回归必跑)

| 项 | 结果 |
|---|---|
| 双栏左列重排(2203 第 1 页) | 左列正文聚成 814/790/720 字符长段(此前逐行 ~70c 碎块) |
| 单测 | `two_column_left_prose_reflows`(左 3 行→1 段)+ `single_column_keeps_one_global_edge`(零回归不变量) |
| NID 双记分牌 | ODL 0.792 / Docling 0.822 — **均不降**(段落聚合不改词序) |
| MHS | ODL 0.685→0.687、Docling 0.643→0.645(标题分组微升);2203 标题 0.652→0.667 |
| TEDS/TEDS_X | 不变 |
| 三件套 | 零回归 |
| 全量 | 138 单测绿(+2),clippy 0 |

## 经验

- **零回归的设计在于"退化为旧路径"**:把列感知做成"无 gutter 时 per-line
  edge 恒等于旧单值",单栏页字节级不变——高风险核心改动的安全垫是让新逻辑
  在常见情形下与旧逻辑数学等价,而非靠测试覆盖所有分支。
- **NID 对段落聚合不敏感**(词级),所以"左列成段"在 NID 上看不出收益——
  收益体现在 chunk 形态(RAG 切块质量)和 MHS;验收要看对的尺子。
- gutter 检测从严的代价是漏判窄 gutter 双栏,但漏判=退回旧行为(左列碎块),
  不会把单栏搞坏——这个不对称是故意的(误判双栏会破坏更常见的单栏页)。
