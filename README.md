# docparse-rs

高效、纯 Rust 的**多格式文档解析系统**。目标：从文档中抽取**带位置的结构化内容**（文本/版面/阅读顺序 → JSON / Markdown / Text），走"结构提取"而非"光栅渲染"的快路径。

> 设计动机来自对 [opendataloader-pdf](../opendataloader-pdf) 的架构分析
> （见 [docs/architecture-analysis.md](../opendataloader-pdf/docs/architecture-analysis.md)）：
> 该项目快，是因为它默认从不把页面渲染成像素，只解析内容流拿坐标，再逐页并行做版面分析。
> docparse-rs 用纯 Rust 复刻这条快路径——无 JVM、无 C++ 依赖、单二进制。

## 当前状态：可用的端到端骨架 ✅

```
PDF ──lopdf──▶ COS 对象 / 内容流字节 + 字体资源
            ──字体层──▶ ToUnicode CMap 解码 + 字形宽度   (参考 veraPDF 移植)
            ──自研解释器──▶ TextChunk{text, bbox, font_size}   (逐页 rayon 并行)
            ──XY-cut──▶ 阅读顺序
            ──行/词重建──▶ 按几何间距还原单词
            ──serializer──▶ JSON / Markdown / Text
```

**实测**（`cargo run -p docparse-cli -- <pdf> -f <fmt>`）：

| 样例 | 结果 |
|---|---|
| `lorem.pdf`（嵌入子集 CID 字体） | ✅ 387 chunks，正确解码出 "Lorem Ipsum" 正文 |
| `1901.03003.pdf`（学术论文，15 页） | ✅ 10,728 带坐标文本块，正文间距正确 |
| `2408.02509v1.pdf`（14 页） | ✅ 14,991 chunks |
| `issue-336-...-bialetti.pdf`（意大利财报） | ✅ 3,829 chunks，含重音字符/数字，间距完美 |
| `chinese_scan.pdf`（扫描件） | ✅ 0 chunks（无文本层，需 OCR，符合预期） |

> CID 子集字体（`lorem.pdf`）此前解码为空，已通过**参考 veraPDF 实现 ToUnicode CMap 解码**修复（见下）。

## 文档

- [docs/phase-1-summary.md](docs/phase-1-summary.md) —— 阶段总结：已完成的工作、验证结果、设计决策。
- [docs/iteration-guide.md](docs/iteration-guide.md) —— 迭代说明：项目怎么长、改动落点、如何加新能力、路线图。

## 架构

Cargo workspace，三个 crate：

| crate | 职责 | 关键依赖 |
|---|---|---|
| [`docparse-core`](crates/docparse-core) | 格式无关核心：IR（`Document/Page/Element/TextChunk`）、`DocumentParser` trait、XY-cut 阅读顺序、JSON/MD/Text 输出 | serde |
| [`docparse-pdf`](crates/docparse-pdf) | 纯 Rust PDF 后端：lopdf 解析 + **自研内容流解释器**（`matrix.rs` 仿射变换 + `interpreter.rs` 操作符状态机）+ **字体层**（`cmap.rs` ToUnicode CMap + `font.rs` 字形宽度，参考 veraPDF）+ rayon 逐页并行 | lopdf, rayon |
| [`docparse-cli`](crates/docparse-cli) | `docparse` 命令行，含 parser 注册表（未来加 DOCX/HTML 后端的挂载点） | clap |

**为什么这样分层**：`core` 不依赖任何 PDF 库——阅读顺序和输出对所有格式通用。新增格式只需实现 `DocumentParser` trait 并在 CLI 注册表里加一行。

### 内容流解释器（项目的核心）

这是 opendataloader-pdf 委托给 veraPDF 的那一层，这里用 Rust 自己实现：lopdf 给出已解析的操作符列表，[`interpreter.rs`](crates/docparse-pdf/src/interpreter.rs) 维护图形/文本矩阵栈，走文本显示操作符发射带坐标的 chunk。**全程不光栅化。**

已处理操作符：`q Q cm` · `BT ET` · `Tf TL Td TD Tm T*` · `Tj ' TJ`。

### 字体层（参考 veraPDF 移植）

`lorem.pdf` 这类嵌入子集 CID 字体的 show 字符串是字形索引（如 `00 03`），不靠字体信息读不出文字。参考 veraPDF `pd.font.cmap` 包移植了三块到纯 Rust：

| 移植内容 | veraPDF 对应 | docparse-rs |
|---|---|---|
| ToUnicode CMap 解析（`bfchar`/`bfrange`） | `CMapParser` / `ToUnicodeInterval` | [`cmap.rs`](crates/docparse-pdf/src/cmap.rs) |
| codespace 变长码切分（1/2 字节） | `CMap.getCodeFromStream` / `CodeSpace` | `cmap.rs` `next_code` |
| 字形宽度（简单 `Widths`、CID `W`/`DW`） | `PDSimpleFont` / `PDCIDFont` / `CIDWArray` | [`font.rs`](crates/docparse-pdf/src/font.rs) |

真实字形宽度让 x 坐标精确，输出层据此**按几何间距还原单词边界**（子集字体每字形一个 chunk，不能无脑加空格）。

## 用法

```bash
cargo build --release
./target/release/docparse input.pdf -f json        # 完整 IR
./target/release/docparse input.pdf -f markdown    # Markdown（含轻量标题启发式）
./target/release/docparse input.pdf -f text -o out.txt
```

```bash
cargo test          # 单元测试（XY-cut 双列排序、矩阵乘法）
```

## 已知限制 / 路线图

按优先级：

- [x] **ToUnicode CMap 文本解码** —— 已参考 veraPDF 实现，CID 子集字体可读（`lorem.pdf` ✅）。
- [x] **字形宽度** —— 已读 `Widths` / `W`/`DW`，x 坐标精确，单词按几何间距重建。
- [x] **MediaBox 继承** —— 已沿 Pages 树向上继承（`font.rs` `resolve_resources` 同款遍历）。
- [ ] **标准 14 字体度量（次头号）** —— 无内嵌 `Widths` 的标准字体（如论文标题）目前回退 0.5 em，导致词间距不准、连字丢失（`Recti**fi**ed`→`Rectied`）。需移植 veraPDF `StandardFontMetrics`/AFM。
- [ ] **简单字体 Encoding/Differences** —— 无 ToUnicode 的简单字体目前 Latin-1 兜底。应支持 WinAnsi/MacRoman/Standard + `Differences` + AGL 字形名→Unicode（veraPDF `Encoding` + `AdobeGlyphList`）。
- [ ] **图片** —— 目前只建模 `ImageChunk`（位置），未抽取像素。需要时实现 XObject 流提取。
- [ ] **语义层** —— 表格识别、列表层级、标题分级（即 veraPDF wcag-algorithms 的等价物）。最大、最有价值、也最难的一层，建在 chunk 之上。
- [ ] **更多格式** —— DOCX / HTML / PPTX，各实现 `DocumentParser` 汇入统一 IR。

## 许可

Apache-2.0。本项目为独立实现，不包含 veraPDF 代码（veraPDF 为 GPLv3+/MPLv2）。
