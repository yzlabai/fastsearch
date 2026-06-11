# docparse-rs · 项目协作约定

> 通用开发规范见 [./AI_AGENT_DEV_SPEC.md](./AI_AGENT_DEV_SPEC.md)。本文件只写 docparse-rs 特有的、与技术栈/领域绑定的约定；冲突时本文件优先于通用规范。
>
> **代码现状永远是真源**：本文件描述的结构/不变量若与代码不符，以代码为准并回写本文件。

docparse-rs 是纯 Rust 的多格式文档解析系统，定位"**速度快、质量好**"：主流程走"结构提取"快路径不渲染像素；难页经路由用纯 Rust 渲染器按需画页、交给神经 enhancer（默认关闭）。从文档抽取带位置的结构化内容（文本/版面/阅读顺序 → JSON/Markdown/Text）。背景与里程碑见 [README.md](README.md)、[docs/phase-1-summary.md](docs/phase-1-summary.md)；怎么迭代见 [docs/iteration-guide.md](docs/iteration-guide.md)。

## 1. 命令

```bash
cargo build                      # 构建
cargo test                       # 全部单测（纯算法：CMap / matrix / XY-cut）
cargo clippy --all-targets       # lint —— 目标零 warning
cargo fmt                        # 格式化（默认风格）
cargo build --release            # 优化构建（lto=thin, codegen-units=1）
./target/release/docparse <file.pdf> -f json|markdown|text|chunks [-o out]
./target/release/docparse mcp                # MCP stdio server（agent 直连）
./target/release/docparse serve --port 8642  # REST（绑 127.0.0.1）
./target/release/docparse <scan.pdf> --ocr    # 扫描件 OCR（需 models/ppocr，数字页零模型）
./target/release/docparse <hard.pdf> --layout # 版面模型重排（需 models/layout，opt-in）
```

**跨真实样例回归**（字体/解码/输出改动必跑）：

```bash
S=../opendataloader-pdf/samples/pdf
for f in lorem 1901.03003 issue-336-conto-economico-bialetti; do
  ./target/debug/docparse $S/$f.pdf -f text 2>/dev/null | head -3
done
```

`lorem`（CID 子集）+ `bialetti`（简单字体+重音）+ `1901.03003`（混合）是最小回归三件套。

## 2. 架构与改动落点

Cargo workspace，十六个 crate（core/pdf/docx/html/ocr/raster/vlm/xlsx/pptx/md/csv/srt/tex/eml/img/cli）。**`core` 不依赖任何 PDF 库**——阅读顺序与输出对所有格式通用，加格式只需实现 `DocumentParser` trait 并在 CLI 注册表加一行。

| 想做的事 | 改哪 |
|---|---|
| 新内容流操作符（`Tc`/`Tw`/`Tz` 等） | [crates/docparse-pdf/src/interpreter.rs](crates/docparse-pdf/src/interpreter.rs) 的 `match op.operator` |
| 文本解码 / 新字体类型 | [crates/docparse-pdf/src/font.rs](crates/docparse-pdf/src/font.rs)（`build_font`/`FontInfo`），CMap 在 [cmap.rs](crates/docparse-pdf/src/cmap.rs) |
| 阅读顺序算法 | [crates/docparse-core/src/reading_order.rs](crates/docparse-core/src/reading_order.rs) |
| 输出格式 / 行词重建 | [crates/docparse-core/src/output.rs](crates/docparse-core/src/output.rs) |
| 加 IR 字段 | [crates/docparse-core/src/ir.rs](crates/docparse-core/src/ir.rs)（注意 serde 派生）+ 产出方 |
| 加新文件格式 | 新建 crate `docparse-<fmt>`，`impl DocumentParser`，在 [cli/main.rs](crates/docparse-cli/src/main.rs) 注册表加一行 |
| 加 CLI 选项 | [cli/main.rs](crates/docparse-cli/src/main.rs) 的 `Cli` struct（clap derive） |
| 加 MCP tool / REST 路由 | [cli/mcp.rs](crates/docparse-cli/src/mcp.rs)（手写 JSON-RPC）/ [cli/server.rs](crates/docparse-cli/src/server.rs)（axum）；共用 `main.rs::parse_path` |
| OCR / enhancer | [crates/docparse-ocr/src/lib.rs](crates/docparse-ocr/src/lib.rs)（tract 推理管线）；边界在 [core/enhance.rs](crates/docparse-core/src/enhance.rs)；图抽取在 [pdf/images.rs](crates/docparse-pdf/src/images.rs) |
| VLM 任务 / 服务接入 | [crates/docparse-vlm/src/lib.rs](crates/docparse-vlm/src/lib.rs)（OpenAI 兼容协议 + 图片描述;协议变更先改 mock 单测） |
| 版面模型 / 阅读组 / 按需渲染 | [ocr/layout.rs](crates/docparse-ocr/src/layout.rs)（区域→`TextChunk.group`）；渲染在 [docparse-raster](crates/docparse-raster/src/lib.rs)（hayro，仅难页 opt-in）；分组重排在 [core/layout.rs](crates/docparse-core/src/layout.rs) `reconstruct_lines` |

## 3. 关键不变量（跨格式后端都要守）

| 不变量 | 约定 |
|---|---|
| 坐标系 | **PDF 用户空间**：原点左下、y 向上、单位 pt。无真实坐标的格式用合成布局折算到此约定 |
| 字形宽度 / advance | **1/1000 em**（PDF 字形空间），输出时再乘 `font_size/1000` |
| 分层 | `core` 不 `use` 任何 PDF 库；PDF 专属逻辑全留在 `docparse-pdf` |
| 并行粒度 | 逐页 `rayon` 并行——内容流解释 CPU 密集、页间无共享状态 |

## 4. Rust / 健壮性约定

| 维度 | 约定 |
|---|---|
| 错误处理 | 边界用 `anyhow::Result`；**解析失败的页返回空 `Page`，不 panic**；位置缺失要有显式回退。`unwrap`/`expect` 只用于不变量已保证处 |
| 近似必须标注 | 任何估算/兜底（0.5em advance、US Letter 回退…）写明 `TODO` + 影响，不静默 |
| 不静默吞数据 | 见 AI_AGENT_DEV_SPEC §7 红旗：不 `try/swallow`、不删测试绿 CI |
| 风格 | `cargo fmt` 默认风格；clippy 零 warning；模块级 `//!` doc 说明"是什么、为什么" |
| 依赖 | 版本集中在根 `Cargo.toml` 的 `[workspace.dependencies]`，crate 用 `dep.workspace = true` 继承；新依赖按通用规范先问 |

## 5. veraPDF 参考与许可边界（落实通用规范"外部参考与许可底线"）

- 本项目 **Apache-2.0**；veraPDF 是 **GPLv3+/MPLv2**。**参考其算法可以，拷贝其代码不行**——只按算法独立重写。
- 参考源码已克隆在 `../opendataloader-pdf/reference/verapdf/`（主要 `veraPDF-parser`、`veraPDF-wcag-algs`）。
- 移植时在源码 `//!` / 注释注明对应的 veraPDF 类/文件（如 `cmap.rs` 注明参考 `CMapParser`/`CodeSpace`）。
- 字体 bug 标准诊断顺序：dump show 字符串原始字节（1/2 字节？CID？）→ 看字体 `Subtype`/有无 `ToUnicode`/`Widths` → 对照 `reference/verapdf` 同名类。

## 6. 测试约定

- **纯算法**（CMap、matrix、XY-cut）必有单测，与代码同 crate。
- **端到端**用 `../opendataloader-pdf/samples/pdf/` 回归，不进 repo。
- **字体/解码类改动必须跨样例回归**（§1 三件套）——这类改动最易顾此失彼（修好 CID 却让简单字体回归）。
- 临时诊断放 `crates/docparse-pdf/examples/diag.rs`，跑完即删，不提交。

## 7. 文档落点（SDD 八步的项目映射，详见通用规范 §4）

`docs/` 现有 `roadmap.md`（战略）、`plans/beating-docling.md`（执行里程碑 M1–M7）、`iteration-guide.md`、`phase-1-summary.md`、`devlogs/`。新工作按需补 `docs/{plans,testcases,testresults,devlogs,modules,architecture,lessonlearned}/`，命名与模板见 AI_AGENT_DEV_SPEC §4–5。

## 8. 路线图

战略见 [docs/roadmap.md](docs/roadmap.md)；近期里程碑 N1–N6 见 [docs/plans/next-iteration.md](docs/plans/next-iteration.md)（M1–M7 历史见 [docs/plans/beating-docling.md](docs/plans/beating-docling.md)）。**全部完成**（截至 2026-06-11，见 docs/devlogs/ 双部会话总结）：M1–M7、N1–N5、Phase 4 可自主项——G1 格式 3→11、G2/G4/G8a、G3-R（`--table-model` 内嵌 UniRec-0.1B×tract 0.23 重抽表结构，合并格语义端到端正确）、G6（Python 客户端+LangChain 验收）、G7（压测 1847 输入 + fuzz ~1020 万次零 panic）、G8b/G8c（`--vlm-describe/--vlm-tables` mock 验收、`--formula-model` 公式→LaTeX、MCP/REST 全增强透传）、G9 全部（G9d TEDS 验收门过）；IR 0.7.0（Cell span 语义 + 图片 base64 内嵌）。**记分牌**（born-digital LTR，默认确定性路径）：vs ODL NID 0.792/MHS 0.685/TEDS 0.419；vs Docling NID 0.822/MHS 0.643/TEDS 0.474。**待办**：候外部输入——PyPI/crates.io/MCP-registry 发布（账号）、Ollama 真实服务验收（VLM 域：图片描述/页型判官/整页转写）、arXiv 千份压测与 fuzz 24h（资源/排期）；候设计/按需——UniRec OCR 档、行内公式、G3b 确定性 span 推断、AsciiDoc/JATS/RTL、Markdown-span 输出。详见 [docs/plans/closing-docling-gaps.md](docs/plans/closing-docling-gaps.md)。⚠️ 经验：①记分牌大跳几乎全是评测/输出管线 bug——分数可疑先怀疑管线；②参照系口径会反噬更忠实的输出（span vs 压扁），产品价值用语义样例验收；③依赖版本本身可以是性能特性（tract 0.21→0.23=17×）；④管道退出码会掩盖失败；⑤新格式 e2e 是共享层的免费测试矩阵。
