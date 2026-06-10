# docparse-rs

高效、纯 Rust 的**多格式文档解析系统**。目标：从文档中抽取**带位置的结构化内容**（文本/版面/阅读顺序 → JSON / Markdown / Text），走"结构提取"而非"光栅渲染"的快路径。

> 设计动机来自对 [opendataloader-pdf](../opendataloader-pdf) 的架构分析
> （见 [docs/architecture-analysis.md](../opendataloader-pdf/docs/architecture-analysis.md)）：
> 该项目快，是因为它默认从不把页面渲染成像素，只解析内容流拿坐标，再逐页并行做版面分析。
> docparse-rs 用纯 Rust 复刻这条快路径——无 JVM、无 C++ 依赖、单二进制。

## 当前状态：完整系统,记分牌见下 ✅

PDF/DOCX/HTML → 版本化 IR(provenance + 每 chunk 置信度)→ 版面(段落/页眉页脚/多栏阅读顺序)→ 语义层(表格四检测器/标题分级)→ JSON / Markdown / Text / **RAG chunks(chunk↔bbox 双向引用)**,经 **CLI / 库 / MCP / REST** 四个接口面输出**逐字节一致**的结果。安全预检(隐藏文本过滤防 prompt injection + zip-bomb/页数资源守卫)已内置;难例经可插拔 `Enhancer` 边界外接(数字页零模型)。

**质量记分牌**(2026-06-10,born-digital LTR,与确定性同类 ODL / 神经管线 Docling 的一致度,非人工真值):

| 同台 | NID 阅读顺序 | MHS 标题 | TEDS 表格 |
|---|---|---|---|
| vs OpenDataLoader(15 份)| **0.764** | **0.627** | 0.098 |
| vs Docling(10 份)| **0.833** | **0.645** | 0.187 |

clean 文档 0.94–1.00(与两者结构同构);聚合被 CJK 复杂版面与无框表格检出拖低(属神经/外接域,见 roadmap)。

**差异化记分牌**(Docling 结构上无法同台):单二进制 **6.6MB**、运行时依赖 **0**、冷启 **<10ms**、**700 页/s**、同输入逐字节确定、chunk 引用可定位率 **100%**。

## 文档

- [docs/phase-1-summary.md](docs/phase-1-summary.md) —— 阶段总结：已完成的工作、验证结果、设计决策。
- [docs/iteration-guide.md](docs/iteration-guide.md) —— 迭代说明：项目怎么长、改动落点、如何加新能力、路线图。

## 架构

Cargo workspace，五个 crate：

| crate | 职责 | 关键依赖 |
|---|---|---|
| [`docparse-core`](crates/docparse-core) | 格式无关核心：版本化 IR + provenance、`DocumentParser` trait、XY-cut 阅读顺序、版面/段落/页眉页脚、表格四检测器、RAG 切块与 `locate` 反查、质量评分与 `Enhancer` 外接边界、资源守卫（`limits`）、JSON/MD/Text 输出 | serde |
| [`docparse-pdf`](crates/docparse-pdf) | 纯 Rust PDF 后端：lopdf 解析 + **自研内容流解释器**（`matrix.rs` 仿射变换 + `interpreter.rs` 操作符状态机）+ **字体层**（`cmap.rs` ToUnicode CMap + `font.rs` 字形宽度，参考 veraPDF）+ rayon 逐页并行 | lopdf, rayon |
| [`docparse-docx`](crates/docparse-docx) | DOCX 后端：docx-rs 结构 → 合成坐标汇入同一 IR；含 zip-bomb 预检 | docx-rs |
| [`docparse-html`](crates/docparse-html) | HTML 后端：DOM 前序遍历 → 标题/段落/列表/表格 | scraper |
| [`docparse-cli`](crates/docparse-cli) | `docparse` 命令行 + **MCP stdio server**（手写 JSON-RPC，零 SDK 依赖）+ **REST**（axum） | clap, axum, tokio |

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
./target/release/docparse input.pdf -f chunks      # RAG 切块（page+bbox+标题面包屑）
./target/release/docparse mcp                      # MCP stdio server（agent 直连）
./target/release/docparse serve --port 8642        # REST：POST /parse + GET /healthz
```

```bash
cargo test          # 73 单测（CMap/矩阵/XY-cut/表格/切块/MCP/限额…）
```

## 进度 / 路线图

近期里程碑全部完成（细节见 [docs/roadmap.md](docs/roadmap.md) 与 [docs/plans/next-iteration.md](docs/plans/next-iteration.md)，过程见 [docs/devlogs/](docs/devlogs/)）：

- [x] **M1–M7**：文本保真（AFM/Encoding/CMap/字距）、IR 脊梁（版本化+provenance+质量分）、版面可读、有框表格、DOCX/HTML、RAG 切块+引用、质量路由+外接边界。
- [x] **N1 评测**：NID/TEDS/MHS 与 ODL/Docling 同台（上表）；差异化指标自动化。
- [x] **N2 服务化**：MCP stdio + REST，四接口逐字节一致。
- [x] **N4 大部**：表格四检测器（bordered→ruled→cluster→borderless）、标题分级、词距。
- [x] **N5 安全预检**：隐藏文本过滤（Tr 3/7/页外/微字 → 标注+排除+可审计）、zip-bomb/页数资源守卫。
- [ ] **N3 真实 enhancer**（近期仅剩）：扫描页外接 OCR/VLM 端到端——待部署选型决策。
- [ ] 远期：复杂度画像（N5c）、小模型 ONNX 内嵌（P4）、人工真值评测集。

## 许可

Apache-2.0。本项目为独立实现，不包含 veraPDF 代码（veraPDF 为 GPLv3+/MPLv2）。
