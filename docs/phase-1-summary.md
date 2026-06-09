# 阶段总结 · Phase 1：可用的纯 Rust PDF 文本抽取骨架

> 时间：2026-06-09 · 状态：✅ 端到端可用，6 单测通过，零 warning
> 代码规模：1,501 行 Rust（3 crate，11 源文件）

---

## 1. 缘起

本项目源于对 [opendataloader-pdf](../../opendataloader-pdf)（ODL，Java/veraPDF）的架构分析
（见 [opendataloader-pdf/docs/architecture-analysis.md](../../opendataloader-pdf/docs/architecture-analysis.md)）。两条关键结论决定了本项目的方向：

1. **ODL 快的本质**：默认从不把页面光栅化，只解析内容流拿坐标，逐页并行做版面分析。
2. **ODL 的护城河是 veraPDF 的 `wcag-algorithms`**（语义结构推断，~2 万行），跨语言无等价物——重写它是数人年工程，不该碰。

由此确定 docparse-rs 的策略：**用纯 Rust 复刻 ODL 的"快路径"**（结构提取，非渲染），底层 PDF 解析用 lopdf + 自研内容流解释器，**不引入 JVM / C++ 依赖**；语义层留到远期。

## 2. 本阶段交付

### 架构（Cargo workspace，3 crate）

```
docparse-rs/
├── crates/docparse-core/     格式无关核心（230 行）
│   ├── ir.rs                 IR：Document/Page/Element/TextChunk/BBox
│   ├── parser.rs             DocumentParser trait（格式后端接口）
│   ├── reading_order.rs      递归 XY-cut 阅读顺序
│   └── output.rs             JSON / Markdown / Text + 行词重建
├── crates/docparse-pdf/      纯 Rust PDF 后端（1,039 行）
│   ├── lib.rs                PdfParser：lopdf 装载 + 逐页 rayon 并行
│   ├── matrix.rs             PDF 仿射变换（行向量约定）
│   ├── interpreter.rs        内容流操作符状态机
│   ├── cmap.rs               ToUnicode CMap 解析 + codespace 切分（参考 veraPDF）
│   └── font.rs               字体模型：ToUnicode + 字形宽度（参考 veraPDF）
└── crates/docparse-cli/      docparse 命令行（54 行）
```

### 处理管线

```
PDF ──lopdf──▶ COS 对象 / 内容流字节 + 字体资源
            ──字体层──▶ ToUnicode 解码 + 字形宽度
            ──解释器──▶ TextChunk{text, bbox, font_size}   (逐页 rayon 并行)
            ──XY-cut──▶ 阅读顺序
            ──行/词重建──▶ 按几何间距还原单词
            ──serializer──▶ JSON / Markdown / Text
```

### 两个里程碑

**M1 — 端到端骨架**：lopdf 装载 → 自研内容流解释器（操作符 `q Q cm BT ET Tf TL Td TD Tm T* Tj ' TJ`，维护图形/文本矩阵栈，发射带坐标 chunk）→ 递归 XY-cut（横/纵间隙取大者，正确处理多列）→ 三种输出。逐页 rayon 并行对应 ODL 的 ForkJoinPool。

**M2 — 参考 veraPDF 的字体层**：解决了头号限制——CID 子集字体读不出文字。移植 veraPDF `pd.font.cmap` 三块到纯 Rust：

| 移植内容 | veraPDF 源 | docparse-rs |
|---|---|---|
| ToUnicode CMap 解析（`bfchar`/`bfrange`） | `CMapParser` / `ToUnicodeInterval` | `cmap.rs` |
| codespace 变长码切分（1/2 字节） | `CMap.getCodeFromStream` / `CodeSpace` | `cmap.rs::next_code` |
| 字形宽度（简单 `Widths`、CID `W`/`DW`） | `PDSimpleFont` / `PDCIDFont` / `CIDWArray` | `font.rs` |

真实字形宽度让 x 坐标精确，输出层据此**按几何间距还原单词**（子集字体每字形一个 chunk，不能无脑加空格）。

## 3. 验证结果

跨 5 个真实样例（`opendataloader-pdf/samples/pdf/`）：

| 样例 | 类型 | 结果 |
|---|---|---|
| `lorem.pdf` | 嵌入子集 CID 字体 | ✅ 387 chunks，正确解码 "Lorem Ipsum"（M2 前为 **0**） |
| `1901.03003.pdf` | 学术论文 15 页 | ✅ 10,728 带坐标文本块，正文间距正确 |
| `2408.02509v1.pdf` | 14 页 | ✅ 14,991 chunks |
| `issue-336-...-bialetti.pdf` | 意大利财报 | ✅ 3,829 chunks，重音字符/数字/间距完美 |
| `chinese_scan.pdf` | 扫描件 | ✅ 0 chunks（无文本层，需 OCR，符合预期） |

单测 6 个：XY-cut（单列/双列）、矩阵乘法、CMap（bfchar+codespace / bfrange）。

## 4. 过程中修正的真实缺陷

- **XY-cut 双列误切**：原先固定"先横切"导致双列被按行切分（L1,R1,L2,R2）。改为**比较横/纵间隙取最大者**，列间距(250) > 行间距(40) → 整列优先正确。
- **逐字形加空格**："L o r e m"。改为**按 bbox 水平间距 > 0.25em 才插空格**，得到 "Lorem"。

## 5. 已知缺口（按优先级，详见 [iteration-guide.md](iteration-guide.md) 路线图）

| # | 缺口 | 现象 | 参考 veraPDF |
|---|---|---|---|
| 1 | **标准 14 字体度量** | 无内嵌 `Widths` 的字体（论文标题）回退 0.5em，词间距不准、连字丢失（`Rectified`→`Rectied`） | `StandardFontMetrics` / `AFMParser` |
| 2 | 简单字体 Encoding/Differences | 无 ToUnicode 的简单字体仅 Latin-1 兜底 | `Encoding` / `AdobeGlyphList` |
| 3 | 图片像素抽取 | 仅建模 `ImageChunk` 位置 | — |
| 4 | 语义层（表格/列表/标题） | 仅有文本 chunk，无结构推断 | `wcag-algorithms`（最大、最难） |
| 5 | 更多格式 DOCX/HTML | 仅 PDF | — |

## 6. 关键设计决策回顾

| 决策 | 选择 | 理由 |
|---|---|---|
| 范围 | 通用多格式（PDF 先行） | 用户选定；`core` 格式无关，加格式只需实现 trait |
| PDF 后端 | 纯 Rust（lopdf + 自研解释器） | 无 C/JVM 依赖、单二进制、最贴合"高效+可移植" |
| 不渲染 | 只解析结构 | 复刻 ODL 快路径；图片像素留到需要时 |
| 并行粒度 | 逐页（rayon） | 内容流解释 CPU 密集、页间无共享状态 |
| 字体策略 | 移植 veraPDF 算法，不绑定其代码 | veraPDF 是 GPL/MPL；本项目 Apache-2.0，仅参考算法独立实现 |

## 7. 下一步

进行中：**移植标准 14 字体 AFM 度量**（缺口 #1），修复论文标题这类无内嵌宽度字体的间距/连字问题。
