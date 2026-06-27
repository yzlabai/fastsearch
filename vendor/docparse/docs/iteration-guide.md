# 迭代说明 · Iteration Guide

面向在 docparse-rs 上继续开发的人：项目怎么长、改动落在哪、怎么验证、怎么加新能力。
系统全貌见 [architecture.md](architecture.md)；能力清单见 [capabilities.md](capabilities.md)；进度见 [status.md](status.md)；协作约定见 [../CLAUDE.md](../CLAUDE.md)。

---

## 0. 快速上手

```bash
cargo build                 # 构建
cargo test                  # 全部单测（纯算法 + 端到端，目标全绿）
cargo clippy --all-targets  # lint —— 零 warning
cargo build --release       # 优化构建（lto=thin, codegen-units=1）
./target/release/docparse <file> -f json|markdown|text|chunks|outline|okf [-o out]

# 跨真实样例快速回归
S=../opendataloader-pdf/samples/pdf
for f in lorem 1901.03003 issue-336-conto-economico-bialetti; do
  ./target/debug/docparse $S/$f.pdf -f text 2>/dev/null | head -3
done
```

参考素材：veraPDF 源码已克隆在 [`../opendataloader-pdf/reference/verapdf/`](../../opendataloader-pdf/reference/verapdf/)，
PDF 算法以 `veraPDF-parser`、`veraPDF-wcag-algs` 两个仓库为主。

---

## 1. 心智模型：数据怎么流动

```
lopdf::Document                                 [docparse-pdf/lib.rs]
   │  get_pages / get_page_content / Resources
   ▼
PageInput { content: Vec<u8>, fonts: HashMap<String,FontInfo>, w, h }
   │  （顺序构建，cheap I/O）
   ▼  rayon par_iter（CPU 密集，页间无共享）
interpret(&PageInput)                            [interpreter.rs]
   │  操作符状态机：矩阵栈 + 文本状态
   │  show 文本时 → font.decode → (text, advance)
   ▼
Page { elements: Vec<Element::Text(TextChunk{text,bbox,font_size})> }
   ▼
Document                                          [docparse-core/ir.rs]
   │
   ├─ reading_order  递归 XY-cut                [reading_order.rs]
   └─ output::to_*   行/词重建 + 序列化          [output.rs]
```

**关键不变量**：
- 坐标统一为 **PDF 用户空间**（原点左下，y 向上，单位 pt）。所有格式后端都要产出这个约定。
- 字形宽度 / advance 统一为 **1/1000 em**（PDF 字形空间），输出时再乘 `font_size/1000`。
- `core` **不依赖任何 PDF 库**——阅读顺序与输出对所有格式通用。

---

## 2. 改动落点速查

| 想做的事 | 改哪 |
|---|---|
| 支持新的内容流操作符（如 `Tc`/`Tw`/`Tz` 字间距） | `interpreter.rs` 的 `match op.operator` |
| 改进文本解码 / 新字体类型 | `font.rs`（`build_font` / `FontInfo`），CMap 相关在 `cmap.rs` |
| 改阅读顺序算法 | `reading_order.rs` |
| 改输出格式 / 行词重建规则 | `output.rs` |
| 加 IR 字段（如颜色、字体名解析） | `ir.rs`（注意 serde 派生）+ 产出方 |
| 加新文件格式（DOCX/HTML） | 新建 crate `docparse-<fmt>`，实现 `DocumentParser`，在 `cli/main.rs` 注册表加一行 |
| 加 CLI 选项 | `cli/main.rs` 的 `Cli` struct（clap derive） |

> 完整落点表（含 OCR/版面/UniRec/VLM/chunk/outline/okf/MCP/REST 等）见 [../CLAUDE.md §2](../CLAUDE.md) 与 [architecture.md](architecture.md)。

---

## 3. 三类典型迭代怎么做

### A. 加一个内容流操作符
1. 在 `interpreter.rs` 的 match 里加 arm，读 `op.operands`（用 `num` / `name_of` / `matrix_from` 辅助）。
2. 更新对应的文本/图形状态。若影响 advance（如 `Tc` 字间距），在 `show_text` 的宽度累加里体现。
3. 加一个最小单测或拿样例回归。

### B. 改进字体文本解码（最常见）
`font.rs::build_font` 决定每个字体怎么解码。当前覆盖：ToUnicode CMap（主）、CID `W`/`DW`、简单 `Widths`、Latin-1 兜底。新增一类时：
1. 在 `build_font` 里识别字体特征（`Subtype` / `Encoding` 等），填充 `FontInfo`。
2. 若需新的码→文本映射，扩展 `FontInfo::decode`；若需新的码切分，扩展 `next_code`。
3. **务必跨样例回归**——字体改动最容易顾此失彼（CID 修好却让简单字体回归）。至少跑 `lorem.pdf`（CID）+ `bialetti.pdf`（简单+重音）+ `1901.03003.pdf`（混合）。
4. 对照 `reference/verapdf/veraPDF-parser/.../pd/font/` 的对应类核对算法。

### C. 加一个新文件格式
1. `cargo new --lib crates/docparse-docx`，依赖 `docparse-core`。
2. `impl DocumentParser for DocxParser`：`supports` 看扩展名/magic，`parse` 产出 `Document`（坐标按 PDF 约定折算，无真实坐标时用合成布局）。
3. `cli/main.rs`：`parsers` 注册表里加 `Box::new(DocxParser)`。
4. 阅读顺序/输出**自动复用**，无需改 `core`。

---

## 4. 约定

- **风格**：`cargo fmt` 默认风格；模块级 `//!` doc 说明"这是什么、为什么"，对照 veraPDF 的地方注明出处。
- **近似要标注**：任何估算/兜底（如 0.5em advance、US Letter 回退）写明 `TODO` 与影响，不要静默。
- **不静默吞数据**：解析失败的页返回空 `Page` 而非 panic；位置缺失有显式回退。
- **测试**：纯算法（CMap、matrix、XY-cut）写单测；端到端用 `samples/pdf/` 回归。字体/解码类改动必须跨样例验证。
- **许可**：本项目 Apache-2.0。**参考 veraPDF 算法可以，复制其代码不行**（veraPDF 是 GPLv3+/MPLv2）。

---

## 5. 路线图与进度

本文不再内置路线图清单（曾经的 M1–M7 待办已全部完成，PPTX/XLSX、图片抽取、服务化、真实 enhancer 等均已落地）。**当前进度、记分牌、待办的单一真源是 [status.md](status.md)**；战略/愿景见 [roadmap.md](roadmap.md)；已实现能力清单见 [capabilities.md](capabilities.md)；过程记录见 [devlogs/](devlogs/)。

挑下一步：想量化质量→评测集 + NID/TEDS/MHS；想拓能力→看 status.md 的「待续」；想补难例→新增 `Enhancer` 接入 `core::enhance` 边界。

---

## 6. 调试技巧

```bash
# 看某 PDF 的操作符分布 / 字体特征：临时 example
# 放 crates/docparse-pdf/examples/diag.rs，用 lopdf 直接 dump，跑完即删：
cargo run -q --example diag -p docparse-pdf

# 只看 stdout（绕开 cargo/编译 warning 噪声）
./target/debug/docparse x.pdf -f text 2>/dev/null

# JSON 配 python 快速统计 chunk 数 / bbox
./target/debug/docparse x.pdf -f json 2>/dev/null | python3 -c "import sys,json;d=json.load(sys.stdin);print(sum(len(p['elements']) for p in d['pages']))"
```

字体类 bug 的标准诊断顺序：dump 该字 show 字符串的原始字节（判断 1/2 字节、是否 CID）→ 看字体 `Subtype`/有无 `ToUnicode`/有无 `Widths` → 对照 `reference/verapdf` 同名类。
