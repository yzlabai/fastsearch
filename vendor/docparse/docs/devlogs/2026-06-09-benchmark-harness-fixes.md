# Devlog · Benchmark 复核：两个 harness bug 一直在低估我方质量

> 日期：2026-06-09 · 状态：✅ 修复评测提取器 + 标题精度；NID/MHS 大幅回升并反映真实
> 方法：用户要求"跑 benchmark 后继续 review 迭代到达 docling/ODL 水平"→ 逐文档 diff 词序，发现 gap 多半是**评测参照不完整**，非我方产出差。

## TL;DR

| 指标（去 RTL）| 会话起 | 会话末 | 主因 |
|---|---|---|---|
| ODL NID | 0.651 | **0.722** | 修 `odl_extract` 漏列表 |
| ODL MHS | 0.584 | **0.614** | 标题去代码/长串裁剪 |
| Docling NID | 0.698 | **0.763** | 修 `docling_gt_extract` 漏列表 |
| Docling MHS | 0.612 | **0.625** | 同上 |

我方产出**一字未改**就让 NID 跳了 +0.07——因为参照少算了一半文本。

## 关键发现：评测提取器丢列表（两边都中招）

逐词 diff `multi_page`（NID 仅 0.600，但 TEDS/MHS 满分，蹊跷）：我方 1314 词、ODL 参照仅 564 词。差的全是**项目符号列表项**（IBM MT/ST、WordStar…）。根因：
- `odl_extract.py` 只递归 `kids`，但 ODL 的 `list` 节点把条目放在 **`list items`** 键下 → 26 个列表项全丢。
- `docling_gt_extract.py` 显式跳过 `group` 节点，但 Docling 把列表项嵌在 **`group`** 里 → 同样全丢。

两个 extractor 各修一行递归后：`multi_page` NID 0.600→0.984、`2305` 0.751→0.921、`redp5110` 0.813→0.972。**这类 list-heavy 文档的低分一直是参照 bug，不是我方阅读顺序差。**

## 标题精度（MHS）

逐文档 ours vs Docling 标题数暴露问题：`redp5110` 我方 **100** vs Docling 22（SQL 代码块每行 `RETURN`/`CASE`/`END` 触发 all-caps/字号规则）。两道确定性门：
1. **去代码/数据行**：含 `= ; { } < >` 或尾随 `,;` 的行不作标题（`USER = ALICE`、`ENABLE ;`）。刻意不碰括号/点/下划线——真标题有（`The Modern Era (1990s - Present)`、`VERIFY_GROUP_FOR_USER function`）。
2. **裁连续标题串**：真文档不会连排 ≥3 个标题无正文；代码块会。runs≥3 只留首行。redp5110 100→~70。

MHS ODL 0.584→0.614、Docling 0.612→0.625，NID 不动，零回归。

## 诚实评估：离 docling/ODL 还有多远

- **clean born-digital LTR：已达其水平**——`multi_page` 0.95–0.98、`code_and_formula` 0.99、`picture` 0.99、`redp5110` 0.97、`2305` 0.92（对 agreement 型指标，0.95+ 即"基本同构"）。
- 聚合被两类**确定性天花板**拖低：① CJK 复杂版面（`skipped_*` 0.12–0.22、`normal_4pages` 0.48–0.61，韩文信息图/label-value）；② 最难双栏论文首页（`2203` 0.57、`2206` 0.61——作者块/版权脚注的多栏阅读顺序，且 ODL 自身文本有粘连如 `public,largeground`）。
- 这两类正是 roadmap 标注的 **N3 enhancer / 版面模型**边界，非确定性可达。

## 剩余确定性可做（低优先/递减）

- MHS：`redp5110` 仍 ~70（裸 SQL 关键字 `RETURN`/`CASE` 无标点，需代码块/等宽字体识别——我方 font 仅资源名，难）；`2206` 反而**漏检**标题（10 vs 18，2 栏正文里 body-size+bold 漏认）。
- NID：项目符号 `•` 错位到行尾（`this•`）——list 文档轻微扣分，属 reconstruct_lines 排序。
- 这些边际收益 <0.01 且风险升，非高价值。

## 结论

本会话把"看起来落后 docling"中**属于评测 bug 的部分**纠正了（NID +0.07）。真实情况：**clean LTR 已达 docling/ODL 水平**，剩余 gap = CJK + 复杂版面 = N3 神经/版面模型领域。确定性这条路的表格/文本/标题/阅读顺序都已采到大头。
