# Devlog · 字重入 IR（bold）— 再提 MHS

> 日期：2026-06-09 · 类型：IR 富化 + 质量改进 · 参考 Docling/ODL（皆带字重）· 状态：✅ 完成
> 结果：MHS 0.412→**0.514**（multi_page 0→1.0）；NID 不变；零回归

---

## 1. 动机

标题检测的编号/全大写路径仍漏 **title-case 子标题**（`Baselines for Object Detection`）。这类靠**字重**区分。Docling/ODL 均在数据模型里带字重——属基础 IR 富化。

先验证可达性：`amt_handbook` 仅 1 个字体、`normal_4pages`（韩文）无 bold 命名 → 这两份字重无效（同字号/无 bold，**确定性不可达，属模型领域**）；但 `2206`/`multi_page` 等有命名 bold 字体 → 可受益。

## 2. 实现

- **IR**：`TextChunk.bold: bool`（serde default）。
- **pdf/font**：`FontInfo.bold` + `font_is_bold`——BaseFont 名含 `bold/black/heavy/semibold/-bd`，或 FontDescriptor `Flags` bit19（ForceBold）/`FontWeight≥600`（Type0 走 DescendantFonts 的 descriptor）。interpreter 据此置 `chunk.bold`。
- **layout**：`Line.bold`（行内全 bold 才算）→ `Block.bold` → `make_block`：单行短块（≤60 字符）且全 bold → 标题。

## 3. 效果（compare_docling.py）

| 文档 | MHS 前 | MHS 后 |
|---|---|---|
| multi_page | 0.000 | **1.000** |
| **平均（10 LTR）** | **0.412** | **0.514** |

- `multi_page` 标题为 bold → 从全漏到全中。其余基本持平。
- `amt_handbook`/`normal_4pages` 仍 0.000——同字号无 bold/韩文 CID，**确定性不可达**（印证路线图"难例外接模型"）。
- NID 0.626 不变；lorem/bialetti 零回归；确定性 15/15；clippy 零 warning；单测 45。

## 4. 本会话质量进展累计（参 veraPDF/ODL + harness 数据驱动）

| 改动 | 指标 |
|---|---|
| 无框表格检测 | 表格召回 1/6→3/6 |
| 词距 0.25→0.15em（参 veraPDF）| NID 0.601→**0.626** |
| 标题 编号/全大写（参 veraPDF）| MHS 0.265→0.412 |
| 字重入 IR（参 Docling/ODL）| MHS 0.412→**0.514** |

**速度**：始终领先（5MB 单二进制、<10ms、700 页/s、零依赖）。

## 5. 诚实的确定性天花板

`amt_handbook`（标题与正文同字体同字号）、`normal_4pages`（韩文）的标题**纯排版无法区分**——这正是 Docling 神经版面模型的领域，也正是本项目设计上**经 N3 可插拔边界外接模型**补的难例。继续在这些上抠确定性收益递减。后续高价值：列推断提 TEDS（表格结构仍是最大差距）、真实 enhancer（N3）。
