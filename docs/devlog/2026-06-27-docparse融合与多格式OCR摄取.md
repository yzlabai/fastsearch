# Devlog · docparse 融合 + 多格式/OCR 摄取（2026-06-27）

> 一条主线的连续增量：把 docparse 并进本仓，并把"文件资料处理"从仅 PDF 扩到多格式 + 扫描件 OCR。
> 代码是真源。上游：[融合方案评估](../plans/2026-06-26-docparse融合方案评估.md)、[职责划分](../plans/2026-06-25-多模态职责划分-docparse与fastsearch.md)、[17-cli spec](../specs/17-cli.md)。

---

## 一句话

docparse 经 `git subtree` 并入 `vendor/docparse`（保历史），其重 ONNX 依赖经根 `exclude` 与
fastsearch 精简构建隔离；`fastsearch ingest <file>` 现进程内解析 **9 种格式 + 图片**，扫描件经
**PP-OCR（真 ONNX 模型，已端到端验证）** 抽文本入索引。**搜索热路径零 docparse 依赖**。

## 1. 融合（step 2+3）

- **subtree**：`git subtree add --prefix=vendor/docparse <本地 docparse-rs> main`（保历史，464 文件含
  vendored tract 213 个）。
- **隔离**：根 `Cargo.toml` `exclude=["vendor/docparse"]` → docparse 保留**自有 workspace**（含 tract +
  OCR/VLM/raster 重 ONNX），**不进 fastsearch 默认 `cargo build --workspace`/收口**；cli `parse` feature
  path-依赖 `vendor/docparse/crates/{core,pdf,...}`。gitignore 其 target/models/tmp。
- **依赖隔离不变量**：`cargo tree -p fastsearch-{server,core,engine}` 零 docparse（搜索热路径干净）。
- **未做（留后续）**：真·单 workspace member 级合并（并 `[workspace.dependencies]` + 版本对齐 + 把 tract
  拖进默认构建）——风险高、收益低；`exclude` 隔离已达成"一仓共存 + 检索零依赖"。

## 2. 多格式摄取

`cmd_ingest` 用 docparse `DocumentParser` trait 注册表，按 `supports(扩展名/magic)` 派发：
**PDF/DOCX/HTML/MD/CSV/XLSX/PPTX/SRT/EML + 图片**（全轻量、无 ONNX）。解析 → `chunk_document` →
`from_docparse_chunk` 适配（这是消除"跨仓手工锁步"的焊点：改任一侧 schema 编译即报错）→ 落盘索引。
- 验证：单测 `multiformat_dispatch`（md/html/csv 解析+适配出非空 chunk）+ 实跑（md→搜"毛利率"命中、
  csv→搜"revenue"命中）。

## 3. OCR 摄取（parse-ocr，真模型端到端验证）

- **路由**：图片/扫描页无文本层 → docparse-img 标 `ScannedNoText` → `apply_ocr`（env
  `FASTSEARCH_OCR_MODELS` 指 PP-OCR 模型目录 → `PpOcrEnhancer` + `enhance::apply`）抽文本；有文本层的
  born-digital PDF **不触发 OCR**（正确——省算力）。重 tract/ONNX 仅 `parse-ocr` feature。
- **真机验证**（`docparse-rs/models/ppocr-v5` det+rec+dict，omnidocbench Murata 数据表页 1653×2339）：
  - 不开 OCR：1 chunk（仅图）；开 OCR：**"1/1 页经增强"→9 chunk**。
  - 抽出真实文本 `Spec. No. JENF243A-0003AA-01` / `Impedance (Ω)` / `BLM18AG121SN1D 120±25% ...`；
    索引后检索 `Impedance`→2、`Reference`→8、`BLM18AG121SN1D`→6 命中。
  - **路由正确性反例**：一张带文本层的 PDF/UA "Scanned" 文档 → 不开 OCR 即得 1320 chunk（text 层），
    OCR 不重复处理。
- **+1 env-gated 测试** `ocr_end_to_end_gated`（带真模型 80s 绿）；模型不进仓（待运行验证策略，同 PG）。

## 4. review / 不变量

- **#1 托管 PG 可移植 / 精简部署**：搜索二进制零 docparse/ONNX；解析/OCR 是独立 feature 档。
- **#2 真源/派生**：docparse 产 chunk（解析关注点），fastsearch 适配加权限/媒资真源 schema；适配器单测守。
- **诚实记账**：OCR 用已下载模型**真跑验证**（非编译即算）；layout/table（docparse-cli 二进制内自定义编排）
  与 VLM（HTTP 服务 + 自定义编排，非 `Enhancer` trait）**未做、需服务**，标后续；模型不进仓。

## 5. 还能接什么（同 parse-ocr 模式 / 或新编排）

- **VLM 图描述**（`docparse-vlm` `VlmClient`，OpenAI 兼容 HTTP）：图/图表 → caption；需 VLM 服务（如
  Ollama llava），非 `Enhancer` trait → 需对 image chunk 自定义编排。`parse-vlm` feature。
- **layout/table 增强**（`LayoutModel`/`UniRec`，模型已下载）：非 `Enhancer` trait，走 docparse-cli
  `parse_and_enhance` 式自定义编排 → 工作量较大。
- 这些都是**摄取侧**能力，搜索热路径不受影响。

## 状态

✅ 融合 step2+3 + 多格式摄取 + OCR 真机验证 **完成**。回写：[CLAUDE.md](../../CLAUDE.md)（命令分档 + cli 行 +
架构）、[17-cli spec](../specs/17-cli.md)、[融合评估](../plans/2026-06-26-docparse融合方案评估.md)、看板。
