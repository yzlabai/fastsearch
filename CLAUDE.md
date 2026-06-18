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
                                              # 交互 TTY 默认显示进度+结束速度小结(stderr,不污染 stdout)
                                              # --progress auto|always|never / --quiet 控制
./target/release/docparse <dir> [-r] --out-dir out/ [--report-json r.json] [--report-csv r.csv]
                                              # 批量:文件夹/多输入(或带 --out-dir)→ 每文件落 <原名>.<后缀> + 聚合报告
                                              # 坏文件不中断整批;串行处理(每文件内部已页并行)。见 docs/cli-batch-and-progress.md
./target/release/docparse mcp                # MCP stdio server（agent 直连）
./target/release/docparse serve --port 8642  # REST（绑 127.0.0.1）
./target/release/docparse <scan.pdf> --ocr    # 扫描件 OCR（默认 PP-OCRv6 tiny；数字页零模型）
                                              # 缺 models/ppocr-v6 时 TTY 下 y/N 确认自动下载~7MB(非TTY报错;DOCPARSE_OCR_DOWNLOAD=1 预确认)
                                              # raw HF ONNX 直载(tract ignore_value_info,无 Python 静态化)、字典从 rec yml 抽；比 v4 更准(顿号)+快 2×+小
./target/release/docparse <scan.pdf> --ocr --ocr-models models/ppocr
                                              # 回退 PP-OCRv4
./target/release/docparse <hard.pdf> --layout # 版面模型重排（需 models/layout，opt-in；默认 DocLayout-YOLO）
./target/release/docparse <hard.pdf> --layout --layout-model models/layout-ppv2/PP-DoclayoutV2_simp.onnx
                                              # PP-DocLayoutV2 后端（按 ONNX 输入数自动识别；杂版面表检测 ≈3× YOLO）
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

Cargo workspace，十七个 crate（core/pdf/docx/html/ocr/raster/vlm/xlsx/pptx/md/csv/srt/tex/eml/img/adoc/cli）。**`core` 不依赖任何 PDF 库**——阅读顺序与输出对所有格式通用，加格式只需实现 `DocumentParser` trait 并在 CLI 注册表加一行。

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
| UniRec 任务（表/公式/转写） | 推理在 [ocr/unirec.rs](crates/docparse-ocr/src/unirec.rs)（OpenOCR 官方 ONNX，宿主驱动 AR+KV-cache，退化守卫）；任务编排各在 [ocr/table_model.rs](crates/docparse-ocr/src/table_model.rs) / [ocr/formula.rs](crates/docparse-ocr/src/formula.rs) / [ocr/transcribe.rs](crates/docparse-ocr/src/transcribe.rs)；模型来源/选型见 docs/refer/openocr-0.1b-evaluation.md |
| VLM 任务 / 服务接入 | [crates/docparse-vlm/src/lib.rs](crates/docparse-vlm/src/lib.rs)（OpenAI 兼容协议 + 图片描述;协议变更先改 mock 单测） |
| 版面模型 / 阅读组 / 按需渲染 | [ocr/layout.rs](crates/docparse-ocr/src/layout.rs)：**双后端**（DocLayout-YOLO / PP-DocLayoutV2，按 ONNX 输入数自动识别），`RegionKind` 统一两者语义，区域→`TextChunk.group`（PPV2 有原生 `order` 直用，否则 XY-cut）、标题类→`TextChunk.tag`；渲染在 [docparse-raster](crates/docparse-raster/src/lib.rs)（hayro，仅难页 opt-in）；分组重排在 [core/layout.rs](crates/docparse-core/src/layout.rs) `reconstruct_lines`。加版面模型/改类别映射改这里 |

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
| **vendored tract 补丁** | 根 `Cargo.toml` `[patch.crates-io]` 把 `tract-hir`/`tract-core` 指向 `vendor/`，内含 2 处最小修复（GatherNd 推断 + TopK 收 TDim）让 PP-DocLayoutV2 跑通。**决定（2026-06-15）：长期 vendored 留 main，不发上游 PR**——为什么/bump tract 时如何重打补丁/何时可删全在 [vendor/README.md](vendor/README.md)（每处 diff 见 [vendor/PATCHES.md](vendor/PATCHES.md)）。**bump tract 前必读 vendor/README.md §4** |

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

## 8. 路线图与状态

- **战略**：[docs/roadmap.md](docs/roadmap.md)
- **当前状态 / 记分牌 / 待办 / 跨阶段经验教训**：[docs/status.md](docs/status.md) ← 单一真源，开工前先读
- **执行里程碑历史**：[docs/plans/](docs/plans/) 各计划 + [docs/devlogs/](docs/devlogs/)
