# 文档内嵌图片 → 可被 RAG 召回与引用

> **状态：已实施（2026-06-21）**，见 [devlog](../devlogs/2026-06-21-images-for-rag.md)。§3 范围全部完成 + **PPTX 抽图已补**（commit 4）。仍列入后续："表格单元格内图片 / HTML 抽图 / 版面 Caption 区域绑定"。
>
> 让 PDF/DOCX 等文档中的图片在 RAG 链路里既能**被检索召回**、又能**在生成结果中渲染/溯源**。

## 1. 需求三件套

- **背景**：当前图片这条链只通了一半——PDF 能解码图片并带 bbox/页码、能 `--image-dir` 落盘、markdown 能 `![]()` 引用；但 **RAG 用的 `chunks` 输出把 `Element::Image` 整个丢掉**，DOCX 则**完全不抽图**。结果：RAG 检索阶段看不到图，生成内容自然带不回相关图片。
- **目标**：让图片成为**一等 chunk**，携带可检索文本（caption/上下文/所属小节）+ 可渲染定位（file/base64 + page/bbox），使 RAG 能"靠文字召回图、靠路径渲染图、靠 bbox 溯源"。
- **价值**：顺着仓库"带位置的结构化内容"定位，把图文统一进 chunk 契约，多格式通用（PDF/DOCX/后续 PPTX 等）。

## 2. 现状（真源：代码，2026-06-21 核对）

| 环节 | 状态 | 位置 |
|---|---|---|
| PDF 解码图片（带 bbox/页/旋转/编码） | ✅ | `crates/docparse-pdf/src/images.rs`；`ImageChunk` in `ir.rs:137` |
| 落盘 / base64 嵌入 | ✅ `--image-dir` / `--image-embed` | `output.rs` |
| Markdown `![image pN](path)` | ✅ 仅当已落盘 | `output.rs:125` |
| 版面 `RegionKind::Figure` / `Caption` | ✅ 已检测 | `crates/docparse-ocr/src/layout.rs:42` |
| VLM 图片描述 | ✅ 渲染→裁剪→caption→**注入为独立文本块** | `crates/docparse-vlm/src/lib.rs:106` |
| **DOCX 抽图** | ❌ 忽略 drawing/blip/word\_media | `crates/docparse-docx/src/lib.rs:63` |
| **chunks 含图** | ❌ 只处理文本/表格，`Element::Image` 被丢 | `chunk.rs` |

核心缺口两处：**(A) chunks 不含图**、**(B) DOCX 不抽图**；外加 **(C) caption 现在是飘着的独立文本块，没和图绑一起**。

## 3. 范围

### 做什么
1. **图片成为一等 chunk**：chunk 输出为每张（达阈值的）图产出 `kind:"image"` chunk。
2. **caption 两档来源**（用户已定：两者都要）：
   - 默认（零模型）：就近 `Caption` 区域 + 周边文本/标题面包屑兜底。
   - `--vlm` 时升级：VLM 描述**写回该图 chunk 的 caption 字段**（而非当前的注入独立文本块）。
3. **DOCX 抽图**：解析 `w:drawing`/`a:blip r:embed` → 顺 rels 找 `word/media/*` → 造 `ImageChunk` 插入阅读位置。
4. **图文关联**：每个 image chunk 带 `section_id`（复用结构树回指）+ 周边文本锚点 `context`。

### 不做什么
- 不做图片内容向量化/CLIP 多模态 embedding——本仓库只产出 chunk，embedding 交给下游 RAG。
- 不做图片去重/相似聚类。
- 不改 PDF 图片解码本身（已足够）。
- PPTX/HTML 抽图本期不做（接口留好，后续按同模式加）。

## 4. 设计

### 4.1 IR 改动（`crates/docparse-core/src/ir.rs`）
`ImageChunk` 增补（serde 注意：`data` 仍 `#[serde(skip)]`）：
- `caption: Option<String>`
- `caption_source: Option<&'static str>`（`"layout-caption"` / `"context"` / `"vlm"`，遵守"近似必须标注"）
- `section_id: Option<String>`（与文本 chunk 的结构树回指同源）
- `context: Option<String>`（图前后若干行文本锚点，用于"如图N所示"召回）

### 4.2 chunk 输出（`chunk.rs`）
- 新增分支处理 `Element::Image`：达阈值（复用 VLM 的 `MIN_FIGURE_COVERAGE≈1%` 面积过滤，避免图标/分隔线刷屏）的图产出独立 chunk。
- chunk 结构加 `kind`（区分 text/table/image）、图片的 `file`/`data_base64`/`media_type`、`page`/`bbox`/`section_id`/`caption`/`context`。
- 召回字段 = `caption` ⊕ `context`（向量化对象）；渲染字段 = `file`/`base64`。

### 4.3 caption 绑定（零模型，新逻辑放 core，建议 `core/layout.rs` 或新 `core/figure.rs`）
1. 版面开启时，把 `RegionKind::Caption` 区域按 bbox 邻接（通常在 Figure 正下方/上方、水平重叠）匹配到最近的 `Figure`/`ImageChunk` → 填 `caption`，`caption_source="layout-caption"`。
2. 无 caption 区时：取图 bbox 上下相邻文本行拼成 `context`，`caption_source="context"`。
3. `section_id` 由图 bbox 命中的结构树小节决定（复用现有结构树/heading 面包屑）。

### 4.4 VLM 改造（`crates/docparse-vlm/src/lib.rs`）
- `annotate_pictures` 由"注入 `Element::Text`"改为"**写回对应 `ImageChunk.caption`**"，`caption_source="vlm"`，覆盖/补充零模型 caption（默认 VLM 优先，可配）。
- 触发不变：`--vlm`，opt-in。

### 4.5 DOCX 抽图（`crates/docparse-docx/src/lib.rs`）
- 扩展 `ParagraphChild` 处理：识别 drawing → `a:blip r:embed` 的 rId → 查 `word/_rels/document.xml.rels` → 读 `word/media/*` 字节。
- 造 `ImageChunk`：DOCX 无真实坐标，按**合成布局**折算到 PDF 用户空间（守不变量 §3：原点左下、pt）；尺寸用 EMU→pt（drawing 的 `wp:extent`）。
- 插入到 `Page.elements` 对应阅读顺序位置。

## 5. 用户使用例子

```bash
# 抽图 + 图进 chunk（零模型 caption），图落盘供前端渲染
docparse report.pdf -f chunks --image-dir ./assets

# DOCX 同样产出 image chunk
docparse spec.docx -f chunks --image-dir ./assets

# 开 VLM：caption 升级为神经描述
docparse report.pdf -f chunks --image-dir ./assets --vlm
```

期望 image chunk（chunks 输出）：
```json
{
  "kind": "image",
  "page": 4,
  "bbox": [72.0, 320.0, 523.0, 540.0],
  "section_id": "2.3",
  "file": "assets/p4-img2.png",
  "media_type": "image/png",
  "caption": "图3 系统总体架构",
  "caption_source": "layout-caption",
  "context": "如图3所示，系统分为采集、解析、检索三层…"
}
```

RAG 链路：对 `caption ⊕ context` 向量化检索 → 命中后把 `file`/`base64` 随上下文喂给 LLM → 生成结果用 markdown 引用该图（带 page/bbox 可溯源）。

## 6. 测试用例

- **单测**：caption↔figure 邻接匹配算法（多 caption、上下位、无 caption 兜底）；EMU→pt 换算；阈值过滤。
- **跨样例回归**（§1 三件套 + 带图样例）：
  - 含图 PDF：image chunk 数 = 达阈值图数；每个 chunk 有 file 且可打开；有 caption 或 context。
  - DOCX 含图：media 字节正确取出、不串图（rId 映射正确）。
  - 文本/解码回归不退化（lorem/bialetti/1901.03003）。
- **VLM mock 单测**：caption 写回 ImageChunk（不再产生游离文本块）。

## 7. 验收标准

1. `-f chunks` 对含图 PDF/DOCX 产出 `kind:"image"` chunk，含 file/page/bbox/section_id 与 caption|context。
2. 零模型默认即有可检索文本（caption 或 context），`--vlm` 升级为神经 caption。
3. DOCX 图片字节正确抽取并定位。
4. `cargo test` 全绿、`cargo clippy` 零 warning、跨样例文本回归不退化。
5. 输出契约变更同步更新 contract/schema 与相关文档（chunks 字段说明、CLAUDE.md CLI 注释、status.md）。

## 8. 风险 / 待确认

- **chunk 契约新增字段**：machine-readable schema（contract）需同步，注意下游兼容（新增字段、不改旧义）。
- **base64 体积**：图嵌 chunks 会撑大输出；建议默认引用 file，base64 走显式开关。
- **DOCX 坐标合成**：无真实版面，阅读顺序按文档流即可，bbox 为合成值需 TODO 标注。
- **caption 优先级**：VLM 是覆盖还是仅在零模型缺失时补——默认覆盖，留配置位。
