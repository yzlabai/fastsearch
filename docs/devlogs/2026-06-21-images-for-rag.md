# 2026-06-21 · 文档内嵌图片 → 可被 RAG 召回与引用

计划文档：[docs/plans/2026-06-21-images-for-rag.md](../plans/2026-06-21-images-for-rag.md)。
目标：让 PDF/DOCX 图片在 RAG 链路里既能被检索召回，又能在生成结果里渲染/溯源。

## 背景缺口（实现前）

- PDF 端图片已解码、带 bbox/页码、可 `--image-dir` 落盘、markdown 能 `![]()` 引用。
- **但 RAG 用的 `chunks` 输出把 `Element::Image` 整个丢了** → 检索阶段看不到图。
- DOCX 完全不抽图。
- VLM caption 注入成游离文本块，与图各自漂着。

## 提交 1：图片成为一等 chunk（core）

**改动**
- [ir.rs](../../crates/docparse-core/src/ir.rs)：`ImageChunk` 加 `caption` / `caption_source`（VLM 或 caption-line 写入）；`SCHEMA_VERSION` 0.7.0 → 0.8.0。
- [chunk.rs](../../crates/docparse-core/src/chunk.rs)：
  - `ChunkKind::Image` + `Chunk.image: Option<ImageMeta>`（file/base64/media_type/caption/caption_source）。检索文本走 `Chunk.text`（caption ⊕ context），渲染/溯源走 `image`。
  - 图片按 **page coverage ≥1%** 门控（`MIN_IMAGE_COVERAGE`，与 VLM 图门一致），过滤图标/分隔线。
  - 按 bbox 把图 **splice 进阅读顺序**（与表格同一 `follows` 逻辑，统一成 `Item::bbox()`）。
  - caption 解析：VLM/enhancer caption 优先；否则就近匹配 in-document caption 行（`is_caption_line`：Figure/Fig./图/Abbildung + 邻接 ≤40pt + 水平重叠）。被绑定的 caption 行从正文流里剔除，**不重复出现**为段落 chunk。
  - context：图周边水平重叠、非 caption 的正文块，按距离拼到 300 字，喂"如图 N 所示"类召回。
- schema 重新生成（`schemas/document.json`、`chunk.json`）。

**测试**
- 单测 3 个：caption+context 合成、coverage gate 过滤、VLM caption 优先。全绿。
- 真实回归 `1901.03003.pdf -f chunks --image-dir`：120 chunks 中 11 个 image chunk，caption 正确从 "Figure 2./3." 绑定、file 导出、section_id 命中。
- 文本三件套（lorem/bialetti/1901）输出未变。clippy 零 warning，fmt 通过。

## 提交 2：VLM caption 写回 + 输出层 caption

**改动**
- [vlm/lib.rs](../../crates/docparse-vlm/src/lib.rs)：`annotate_pictures` 不再 push 游离 `Element::Text("[figure] …")`，而是把描述**写回对应 `ImageChunk.caption`**（`caption_source: "vlm:<model>"`）。改为按 element 索引遍历，网络往返后回写同一元素。`TextChunk` import 移除。
- [output.rs](../../crates/docparse-core/src/output.rs)：旧路径下 VLM 文本会出现在 text/markdown，写回后需在输出层显式呈现 caption：
  - markdown：caption 作为 `![alt](file)` 的 alt；无 file 但有 caption → `*caption*` 一行，不丢描述。
  - text：caption 单独成行。
  - `PageContent.images` 过滤放宽为 `file.is_some() || caption.is_some()`。

**效果**：图与描述合一 —— `--vlm` 的 caption 经 commit 1 的链路自动进 image chunk（chunk 层 `vlm_caption_on_imagechunk_wins` 已覆盖），同时 markdown/text 仍可见。

**测试**：output 3 个新单测（alt 文本、caption-only 斜体行、无 file 无 caption 不渲染）。全绿，clippy/fmt 通过。注：`annotate_pictures` 本身需 raster+网络，沿用既有惯例不单测（与 `refine_tables` 一致），契约由 chunk/output 层单测锁定。

## 提交 3：DOCX 抽图

**docx-rs 探查**：`Docx.images: Vec<(rId, path, Image(bytes), Png)>` 公开；`RunChild::Drawing(Box<Drawing>)` → `DrawingData::Pic(Pic)`，`Pic.id` 是 `r:embed` 的 rId、`Pic.size` 是 EMU。default feature 启用 `image` crate，故元组 `.2`（`Image`）是**原始字节**（png/jpeg/…），mime 由 path 后缀定。

**改动**
- [ir.rs](../../crates/docparse-core/src/ir.rs)：新增 `ImageKind::Encoded` —— 已编码图片字节透传（DOCX/PPTX/HTML 媒体共用的形态），`data_media_type` 记格式。schema 重新生成。
- [main.rs](../../crates/docparse-cli/src/main.rs)：`export_images`/`embed_images` 支持 `Encoded`（字节原样写盘/base64，扩展名/mime 由 `mime_ext`/`data_media_type` 决定）。
- [synth.rs](../../crates/docparse-core/src/synth.rs)：`PageBuilder::image(data, w_pt, h_pt, media_type)` —— 按当前流位置放合成 bbox 的 `Element::Image`（坐标合成，守 PDF 用户空间不变量）。
- [docx/lib.rs](../../crates/docparse-docx/src/lib.rs)：遍历段落 run 的 drawing → Pic，按 rId 查 `docx.images` 取字节，EMU→pt（`/12700`），mime 由 path 后缀，落到 `PageBuilder::image`。

**范围**：段落内（inline/anchored）图片已覆盖；**表格单元格内的图片暂不处理**（TODO）。

**测试**
- 单测：drawing→ImageChunk（kind=Encoded、原始字节、image/png、EMU→pt 尺寸）。
- 真实端到端：python-docx 造含图 docx → `docparse -f chunks --image-dir`：得 1 个 image chunk，caption "Figure 1: …" 由邻接段落绑定、context 来自前后正文、PNG 经 Encoded 透传导出 `p1-1.png`。

## 收口

- 全量 `cargo test` 全绿（34 套）、`cargo clippy --all-targets` 零 warning、`cargo fmt` 通过。
- 文本三件套（lorem/bialetti/1901）回归未变。
- schema：`SCHEMA_VERSION` 0.7.0→0.8.0；`ImageChunk` 加 caption/caption_source、`ImageKind` 加 Encoded、`Chunk` 加 image —— 均为**新增可选字段/枚举变体，向后兼容**。

## 提交 4：PPTX 抽图 + 共享 helper 去重

接续后续清单，把 PPTX 拉齐到 DOCX 的抽图能力，并消除重复。

**改动**
- [core/synth.rs](../../crates/docparse-core/src/synth.rs)：把 `emu_to_pt`、`image_mime_from_path` 提为 **pub 共享 helper**（合成 OOXML 后端通用：EMU→pt、路径后缀→MIME）。
- [docx/lib.rs](../../crates/docparse-docx/src/lib.rs)：删本地 `emu_to_pt`/`mime_from_path`，改用 synth 共享版（去重，单一真源）。
- [pptx/lib.rs](../../crates/docparse-pptx/src/lib.rs)：
  - `load_media` 预载所有 `ppt/media/*` 字节；`slide_rels_path` + `parse_rels` + `resolve_target` 解析每张 slide 的 `_rels/slideN.xml.rels`（rId→相对 Target→规范化包内路径，处理 `.`/`..`/绝对 `/`）。
  - `parse_slide` 扩展事件机：`<p:pic>` 内捕获 xfrm `<a:ext cx cy>`（EMU）+ `<a:blip r:embed>` 的 rId，在 `</p:pic>` 处按 rId→path→bytes 内联调 `PageBuilder::image`（流位置正确；无 ext 兜底 3in×2in）。

**测试**
- 单测：`resolve_target` 相对/绝对解析；`slide_picture_becomes_image_element`（真 zip：slide+rels+media → ImageKind::Encoded、字节一致、image/png、EMU→pt 尺寸）。
- 真实端到端：python-pptx 造含图 deck → `-f chunks --image-dir`：得 image chunk、PNG 经 Encoded 导出 `p1-1.png`、context 取到标题。
- 全量 34 套绿、clippy 0、fmt。**排查记录**：首次端到端"无 image"实为跑了未重建的旧 CLI binary（rebuild 后正常）——非逻辑问题。

## 后续（仍未做）

- DOCX/PPTX 表格单元格内图片；**HTML 抽图**（`<img>` data: URI 自洽；相对路径需 base dir 接线；远程 URL 不取，离线信封外）。
- 版面模型 `RegionKind::Caption` 区域绑定（当前 caption 走文本 pattern + 邻接；模型路径需把 tag 透到 Block 层）。
