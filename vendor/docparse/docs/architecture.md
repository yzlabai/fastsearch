# 架构总览 · Architecture

> docparse-rs 的系统架构单一真源。**代码现状永远是真源**：本文与代码不符时以代码为准并回写本文。
> 当前进度/记分牌见 [status.md](status.md)；功能/能力清单见 [capabilities.md](capabilities.md)；接入指南见 [agent-integration.md](agent-integration.md)。

docparse-rs 是纯 Rust 的多格式文档解析系统，定位「**速度快、质量好**」：主流程走「结构提取」快路径**不渲染像素**，从文档抽取带位置的结构化内容（文本/版面/阅读顺序 → 统一 IR → JSON/Markdown/Text/RAG chunks/结构树/OKF）。难页经质量路由按需调神经增强（OCR/版面/表/公式/VLM，默认关闭）。一个 ~29MB 二进制，零运行时依赖。

---

## 1. Cargo workspace（17 个 crate）

```
docparse-cli                 ← 聚合：CLI / MCP / REST / 批量；模型装配
  ├── 12 个格式后端（各 impl DocumentParser）
  │     docparse-pdf docx html xlsx pptx md csv srt tex eml img adoc
  ├── 3 个增强子系统（opt-in，经 enhance 边界接入）
  │     docparse-ocr（OCR + 版面 + UniRec 表/公式/转写）
  │     docparse-vlm（OpenAI 兼容 VLM）
  │     docparse-raster（hayro 按需渲染，供 ocr/vlm 用）
  └── docparse-core           ← IR / 阅读顺序 / 输出 / chunk / outline / okf / enhance / quality
```

| crate | 职责 | 关键依赖 |
|---|---|---|
| **docparse-core** | IR 定义、阅读顺序、行词重建、输出序列化、RAG chunk、结构树、OKF、增强边界、质量评分、JSON Schema 生成 | serde, rayon, encoding_rs, chardetng, libc, schemars(opt) |
| **docparse-pdf** | 纯 Rust PDF：lopdf 解 COS + 自研内容流解释器 + 字体层（CMap/AFM/Encoding） | docparse-core, lopdf, hayro-ccitt/jbig2/jpeg2000 |
| **docparse-docx** | DOCX（docx-rs）→ 合成页；段落样式定标题、`w:numPr` 列表、`w:drawing` 图片 | docparse-core, docx-rs |
| **docparse-html** | HTML/XHTML（scraper/html5ever）；`<img>`（data:/相对路径）抽图、alt 作图说 | docparse-core, scraper, base64 |
| **docparse-xlsx** | Excel（calamine）；每 sheet 一页，单元格→表 | docparse-core, calamine |
| **docparse-pptx** | PPTX（quick-xml + zip）；每 slide 一页；`p:pic` 经 rels 抽图 | docparse-core, quick-xml, zip |
| **docparse-md** | Markdown（pulldown-cmark） | docparse-core, pulldown-cmark |
| **docparse-csv** | CSV（自写 RFC4180） | docparse-core |
| **docparse-srt** | SRT/WebVTT 字幕 | docparse-core |
| **docparse-tex** | LaTeX 源常用子集 | docparse-core |
| **docparse-eml** | EML 邮件（mail-parser） | docparse-core, mail-parser |
| **docparse-img** | PNG/JPEG 图像文档 | docparse-core, zune-png, zune-jpeg |
| **docparse-adoc** | AsciiDoc 常用子集 | docparse-core |
| **docparse-ocr** | tract ONNX 推理：PP-OCR、DocLayout-YOLO/PP-DocLayoutV2 版面、UniRec 表/公式/转写 | docparse-core, tract-onnx, docparse-raster |
| **docparse-raster** | 纯 Rust 按需页渲染（hayro），仅供 OCR/VLM/版面裁图 | hayro |
| **docparse-vlm** | OpenAI 兼容 VLM（图说 / 表重抽） | docparse-core, docparse-raster, ureq |
| **docparse-cli** | 四接口聚合 + 批量 + 模型惰性装配 | 以上全部 + clap, axum, tokio, indicatif |

**核心不变量：`docparse-core` 不依赖任何 PDF 库**（依赖只有 serde/rayon/encoding_rs/chardetng/libc/schemars）。阅读顺序与输出对所有格式通用——加格式 = 新建 crate `impl DocumentParser` + CLI 注册表加一行。

---

## 2. 分层与依赖方向

```
            CLI · MCP(stdio) · REST(axum) · 库          ← 四接口，输出逐字节一致
                        │ 共用 parse_path / RunModels
        ┌───────────────┼────────────────┐
        ▼               ▼                ▼
   格式后端          enhance 边界        输出
 (impl DocumentParser) (质量路由)   (json/md/text/chunks/outline/okf)
        │               │                │
        └──────────► docparse-core (IR + 阅读顺序 + 语义层) ◄──────┘
                        ▲
        ┌───────────────┼────────────────┐
   docparse-ocr     docparse-vlm     docparse-raster
   (tract ONNX)    (OpenAI API)     (hayro 渲染)
```

依赖方向单向向下：`cli → 后端/增强 → core`。增强子系统（ocr/vlm）依赖 core，绝不反向。

`DocumentParser` trait（[core/parser.rs](../crates/docparse-core/src/parser.rs)）：

```rust
pub trait DocumentParser: Send + Sync {
    fn name(&self) -> &'static str;                       // "pdf" / "docx" / …
    fn supports(&self, path: &Path) -> bool;             // 按扩展名
    fn parse(&self, path: &Path) -> anyhow::Result<Document>;
}
```

注册表与分发：[cli/main.rs](../crates/docparse-cli/src/main.rs) 的 `parsers_with()`（每后端一行）+ `parse_path_with()`（取首个 `supports()` 命中的后端）。MCP/REST/库共用同一入口。

---

## 3. 数据流主线（文件 → 输出）

```
文件
 │ ① parse —— 选后端 → 后端解析
 ▼
统一 IR：Document → Page → Element(Text|Image|Table)        [SCHEMA_VERSION 0.8.0]
 │ ② enhance（可选，质量驱动）—— quality 评分 → 按页路由 → OCR/版面/UniRec/VLM
 │    增强产物带 source（"ocr:ppocr"/"vlm:<model>"/"layout:<model>"/"table:unirec-0.1b"…）
 ▼
 │ ③ 阅读顺序 + 行词重建 —— reading_order(XY-cut/分组) → layout(行→段/标题/列表/代码)
 │ ④ 语义层 —— table 检测 / outline 建结构树
 ▼
 │ ⑤ 输出 —— json | markdown | text | chunks | outline | okf
 ▼
输出（chunks/okf 携 page+bbox+section_id，可溯源）
```

| 阶段 | 落点 |
|---|---|
| 分发 | [cli/main.rs](../crates/docparse-cli/src/main.rs) `parse_path_with` |
| PDF 解析 | [pdf/interpreter.rs](../crates/docparse-pdf/src/interpreter.rs)（内容流状态机）、[pdf/font.rs](../crates/docparse-pdf/src/font.rs)、[pdf/images.rs](../crates/docparse-pdf/src/images.rs) |
| 合成页（非 PDF） | [core/synth.rs](../crates/docparse-core/src/synth.rs) `PageBuilder`（DOCX/HTML/XLSX/PPTX/…折算到 PDF 用户空间） |
| 质量评分 | [core/quality.rs](../crates/docparse-core/src/quality.rs) |
| 增强边界/路由 | [core/enhance.rs](../crates/docparse-core/src/enhance.rs)（`Enhancer` trait + `plan`/`apply`，内存受限页并行池） |
| 阅读顺序 | [core/reading_order.rs](../crates/docparse-core/src/reading_order.rs)（XY-cut，支持 group 宏序） |
| 行词/块重建 | [core/layout.rs](../crates/docparse-core/src/layout.rs) `page_blocks`（词距 0.15em、段/标题/列表/代码/页眉页脚/去连字） |
| 表检测 | [core/table.rs](../crates/docparse-core/src/table.rs)（bordered/ruled/cluster/borderless） |
| 结构树 | [core/outline.rs](../crates/docparse-core/src/outline.rs) `build`（标题层级建树，id=出现序） |
| RAG chunk | [core/chunk.rs](../crates/docparse-core/src/chunk.rs)（chunk + bbox + heading_path + section_id；图片成一等 chunk） |
| 输出 | [core/output.rs](../crates/docparse-core/src/output.rs)、[core/okf.rs](../crates/docparse-core/src/okf.rs) |

---

## 4. 统一 IR（[core/ir.rs](../crates/docparse-core/src/ir.rs)，`SCHEMA_VERSION = "0.8.0"`）

```
Document { source, provenance?, pages[] }
  Page { number(1-based), width, height, elements[] }
    Element = Text(TextChunk) | Image(ImageChunk) | Table(Table)
```

- **TextChunk**：text, bbox, font_size, font?, page, confidence(模型<1.0), bold, hidden(审计), source?(增强溯源), group?(读序组), tag?(结构角色 H1..H6/P/LI/Caption…)
- **ImageChunk**：bbox, page, width_px/height_px, turns(90°×n), kind(ImageKind), data(`#[serde(skip)]`), file?/data_base64?/data_media_type?(导出/嵌入), caption?/caption_source?(图说+来源)
- **ImageKind**：None / Gray8 / Rgb8 / Jpeg(透传) / **Encoded**(PNG/GIF/… 已编码透传，DOCX/PPTX/HTML 媒体)
- **Table**：bbox, page, rows[][]（行优先，等长），source?；**Cell**：text, bbox, row_span/col_span, merged
- **Provenance**：schema_version + parser + parser_version（每文档一份；元素级溯源是各自 bbox+page+source）

**不变量**：坐标=PDF 用户空间（原点左下、y 向上、pt）；字形 advance=1/1000 em（输出乘 `font_size/1000`）；3 层无深嵌套；解析失败页返回空 `Page` 不 panic。

派生而非入 IR：**outline 结构树**与 **OKF bundle** 在 emit 期从 chunk/标题推导，`-f json` 字节不受影响。

---

## 5. 增强子系统（opt-in，经 enhance 边界）

| 子系统 | flag | 模型 / 后端 | 落点 |
|---|---|---|---|
| OCR | `--ocr` | PP-OCRv6 tiny 默认（v4 回退），tract ONNX，缺模型 TTY 下确认自动下载 ~7MB | [ocr/lib.rs](../crates/docparse-ocr/src/lib.rs) |
| 版面 | `--layout` | **双后端**：DocLayout-YOLO（默认）/ PP-DocLayoutV2（按 ONNX 输入数自动识别），区域→读序组 + 标题/Caption tag | [ocr/layout.rs](../crates/docparse-ocr/src/layout.rs) |
| 表结构 | `--table-model` | UniRec-0.1B（合并单元格/多行表头），失败保留确定性网格 | [ocr/table_model.rs](../crates/docparse-ocr/src/table_model.rs) |
| 公式 | `--formula-model` | UniRec-0.1B → LaTeX | [ocr/formula.rs](../crates/docparse-ocr/src/formula.rs) |
| 整页转写 | `--transcribe-model` | UniRec-0.1B（CJK/难版面整页） | [ocr/transcribe.rs](../crates/docparse-ocr/src/transcribe.rs) |
| VLM 图说 | `--vlm-describe` | OpenAI 兼容服务，写回 `ImageChunk.caption` | [vlm/lib.rs](../crates/docparse-vlm/src/lib.rs) |
| VLM 表重抽 | `--vlm-tables` | 同上，替换确定性网格 | [vlm/lib.rs](../crates/docparse-vlm/src/lib.rs) |

边界：[core/enhance.rs](../crates/docparse-core/src/enhance.rs) 的 `Enhancer` trait——core 只认 trait，模型代码全在 ocr/vlm crate。数字页零模型；增强按页触发；模型经 `RunModels`/`OnceLock` 整批/整服务只载一次。

**vendored tract 补丁**（[vendor/](../vendor/)）：根 `Cargo.toml` `[patch.crates-io]` 指向 `vendor/`，含 2 处最小修复（GatherNd 推断 + TopK 收 TDim）让 PP-DocLayoutV2(RT-DETR) 在 tract 跑通。决定长期留 main、不发上游 PR——为什么/如何 bump 见 [vendor/README.md](../vendor/README.md)。

---

## 6. 四接口（输出逐字节一致）

| 接口 | 落点 | 形态 |
|---|---|---|
| CLI | [cli/main.rs](../crates/docparse-cli/src/main.rs) | `docparse <file> -f <fmt>`；批量（文件夹/`--out-dir`/`--jobs`）；子命令 `mcp`/`serve`/`schema` |
| MCP | [cli/mcp.rs](../crates/docparse-cli/src/mcp.rs) | stdio JSON-RPC，协议 2025-06-18；工具 parse_document/get_chunks/outline/export_okf/locate；resources(schema+指南) + prompts |
| REST | [cli/server.rs](../crates/docparse-cli/src/server.rs) | axum 绑 127.0.0.1；`POST /parse`、`GET /healthz`、`GET /openapi.json`、`GET /schema/{name}` |
| 库 | core 公开 API | `DocumentParser::parse` → `chunk`/`outline`/`okf`/`enhance` |

**机器可读契约**：[core/schema.rs](../crates/docparse-core/src/schema.rs) 从 IR 类型（`#[cfg_attr(feature="schema", derive(JsonSchema))]`）生成 draft-2020-12 schema，入库 [schemas/](../schemas/)，golden 测试防漂移；REST 出 OpenAPI 3.1、MCP 以 resources 暴露。三面同源（共用 `parse_path` + `okf::build` 等），同输入同格式 → CLI=MCP=REST 字节一致。

---

## 7. 并行 · 健壮 · 确定性

- **并行**：PDF 逐页 rayon（内容流 CPU 密集、页间无共享）；OCR 增强页并行（内存受限池，`desired_parallelism(cores,ram)`）；批量文件级 `--jobs`（仅确定性档；任一模型 flag 强制串行防爆内存）。
- **健壮**：失败页返回空 `Page`；批量坏文件记一行 error 不中断整批；增强失败降级为「不增强」，确定性结果始终成立。
- **安全预检**：隐藏文本过滤（标记可审计、不静默丢，渲染输出排除）；zip-bomb 预检（OOXML 只读中央目录）；页数早停。
- **确定性**：同文件多次跑逐字节一致；OKF tar 固定 mtime/权限；无 wall-clock（时间戳取源 mtime）。
