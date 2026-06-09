# Devlog · M3 版面可读（段落聚合 + 页眉页脚识别）

> 日期：2026-06-09 · 里程碑：[plans/beating-docling.md](../plans/beating-docling.md) M3 · 状态：✅ 完成
> 结果：新 `core::layout` 模块；1901 摘要重排为段落、bialetti 表格保持逐行不糊；确定性 30/30；零回归

---

## 1. 目标

把"逐行文本流"升成"可读段落"，直接抬 NID，对位 Docling"输出比纯 Markdown 更适合结构化"。两件事：段落聚合、页眉页脚识别。

## 2. 设计与最大坑：段落聚合不能糊掉表格

新建 `core/layout.rs`（模块 3 落点），把行重建从 output 移入并加：
- `reconstruct_lines`：chunk→行（共基线 + 几何词距），保留 `x0/x1/cy`。
- `group_blocks`：行→段/标题。
- `detect_header_footer`：跨页页眉页脚。

**坑**：朴素"相邻行垂直间距小就合并"会把 **bialetti 财报表格糊成一坨**（每行等距的标签/数字被并成一段）——这是结构化文档的严重回归。修法是把合并条件收紧到**强散文信号**：

```
合并当且仅当：垂直间距 ≤1.8×行高 ∧ 字号相近 ∧ 上一行触达列右缘(fill_x) ∧ 上下行都非数字行
```

- `fill_x = 页内最宽 body 行 x1 − 5%页宽`：散文换行的非末行会触达右缘；表格短标签/标题不触达 → 不并。
- 数字行守卫（非空字符 >40% 数字）：财报数字行不并。

实测：bialetti 标签逐行保留（x1≤270 ≪ fill_x=522），1901 摘要正常重排为整段。

## 3. 落点

| 文件 | 改动 |
|---|---|
| `core/layout.rs`（新） | `Line`/`Block`、`reconstruct_lines`、`group_blocks(lines,median,fill_x)`、`detect_header_footer`、`median_font_size`；6 单测 |
| `core/output.rs` | 改为消费 `layout` blocks：逐页重建行→去页眉页脚→分段；per-page 算 `fill_x` |
| `core/lib.rs` | `pub mod layout` |

## 4. 验证

- **段落聚合**：1901 摘要重排为一段连贯文本（原逐行）；bialetti 表格**逐行保留不糊**（修复了开发中一度出现的糊块）。
- **页眉页脚**：逻辑单测覆盖（4 页"Page #"页脚被识别、正文不误删、单页不触发）。真实样例 1901/2408 **无 running head**（arXiv 预印本本就没有，戳只在 p1）→ 检测器**正确不误删**（零假阳性）。
- **确定性**：markdown 30/30 逐字节一致。
- **零回归**：四样例 chunk 数不变（IR 未动，仅输出层分组）；clippy 零 warning；单测 core 12 + pdf 14 全过。

## 5. 已知限制（诚实标注，非本次回归）

- **多栏左列不重排**：`fill_x` 是整页右缘，多栏左列行触不到 → 其散文暂不合并（不回归，只是未改善）。需**列检测**（M4）。
- **旋转戳干扰**：arXiv 侧边竖排戳被阅读顺序插进摘要中段，并因大字号误生成 `## decoder` 假标题。需旋转文本处理，未做。
- **标题词内粘连**（`MORAN:AMulti-Object...`）：标题字体字形间距 < 0.25em 阈值致行内不插空格，属词切分而非分段，留待按需调阈/字距。

## 6. 对记分牌（roadmap §6）

- **NID**：散文成段、表格不糊、页眉页脚可去——可读性结构化提升，待建评测集后回填 born-digital NID。
- reading-order 异常分（M2 留空项）有了 `layout` 信号后可在后续补入 `QualityReport`。

## 7. 下一步

进入 **M4 语义结构起步：有框表格检测**（TEDS 入口）——需从内容流抽矢量线段建网格，对照 wcag-algs 独立实现。这也会顺带产出**列右缘**，回头修 M3 的多栏左列限制。
