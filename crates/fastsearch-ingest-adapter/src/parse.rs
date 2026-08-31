//! # fastsearch-ingest-adapter
//!
//! 独立摄取 worker 与 CLI 共用的 docparse → fastsearch chunk 适配层。
//!
//! 把 `docparse_core::Chunk`（解析关注点）适配成 `fastsearch_core::Chunk`（真源 schema
//! 加权限/媒资）。**这正是融合要消除"跨仓手工锁步"的焊点**：改任一侧 schema，本适配器
//! 编译即报错（见 [docparse 融合方案 §2](../../../docs/plans/2026-06-26-docparse融合方案评估.md)）。
//!
//! 搜索热路径（core/server/engine/...）不依赖任何 docparse crate；解析能力仅在本 feature 编译。

use crate::ChunkProfile;
use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use fastsearch_core::{AssetPointer, BBox, Chunk, ChunkKind, ImageVectorStatus, MediaRef};
use std::collections::HashMap;
use std::path::PathBuf;

/// 文档图片**原始字节**的去向（CLI `--images`）。
///
/// 默认 [`ImageBytes::Object`]：字节随 `/v1/index` 上传，由 server 落对象存储（PG 真源只留
/// uri），`/v1/asset/{citation_id}` 由此能签发短时 URL 吐回原图。
/// [`ImageBytes::Inline`] 让 server 把字节内联进 PG `bytea`——整本 PDF 的图片很容易把请求体推过
/// server 的 20MB `DefaultBodyLimit`，也会把真源表撑大，且 **server 必须配了 `DATABASE_URL`
/// 才有地方内联**（未配时 `/v1/asset` 对 Inline 一律 404，真机验证 2026-08-25），所以必须显式选。
/// [`ImageBytes::None`] = 一个字节都不采（与本能力落地前**完全一致**：PDF 连 `decode_images`
/// 都不开，不付内存代价）。
///
/// **本迭代只闭环 `Jpeg`/`Encoded` 两类**（零新依赖）；`Rgb8`/`Gray8` 裸位图的代价见
/// [`ImagePayload::NeedsEncode`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ImageBytes {
    /// 采集 → 上传 → server 落对象存储（默认）。
    #[default]
    Object,
    /// 采集 → 上传 → server 内联进 PG `bytea`（**需 server 配了 PG 真源**）。
    Inline,
    /// 不采集（零回归档）。
    None,
}

impl ImageBytes {
    /// 是否需要图片字节——决定 PDF 是否打开 `decode_images`（该开关会materialize
    /// 每张 ≥16px 的图，image-heavy 文档上是实打实的内存开销，故按需开）。
    fn wants_bytes(self) -> bool {
        !matches!(self, ImageBytes::None)
    }
}

/// 纯本地解析选项；网络、collection 与 API key 不属于适配层。
pub struct ParseOptions {
    /// 待解析文件（按扩展名分发到对应 docparse 解析器；多格式）。
    pub file: PathBuf,
    pub doc_id: String,
    pub tenant: Option<String>,
    pub acl: Vec<String>,
    /// 图片原始字节的去向（默认 [`ImageBytes::Object`]）。
    pub images: ImageBytes,
    /// 本次解析分块采用的可追溯 profile。
    pub chunk_profile: ChunkProfile,
    /// Heavy enhancement switches. A compiled feature and its runtime model/service env must both
    /// be present; `false` guarantees the enhancement is skipped.
    pub enhancements: Enhancements,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Enhancements {
    pub ocr: bool,
    pub tables: bool,
    pub vlm: bool,
}

/// docparse 多格式解析器注册表（轻量、无 ONNX）：按 `DocumentParser::supports`（扩展名/magic）
/// 派发。重增强器经 feature + env 接入：OCR=`parse-ocr`、表格=`parse-tables`（均进程内 ONNX，
/// 指模型目录）；VLM 区域识别=`parse-vlm`（外部 OpenAI 兼容服务，指 URL+模型名，见 [`apply_vlm`]）。
/// 自然图 VLM 描述（`--vlm-describe` 那条 caption 路）仍未接入，属下一迭代。
///
/// `decode_images`：只在调用方要图片字节时打开（`--images` 非 `none`）。打开后 PDF 后端会
/// materialize 每张 ≥16px 的图；关着时只 materialize 整页覆盖的扫描候选（省内存，原行为）。
/// 其余格式（DOCX/PPTX/HTML）的图字节本来就随解析出来，不受此开关影响。
fn parsers(decode_images: bool) -> Vec<Box<dyn docparse_core::parser::DocumentParser>> {
    vec![
        Box::new(docparse_pdf::PdfParser { decode_images }),
        Box::new(docparse_docx::DocxParser),
        Box::new(docparse_html::HtmlParser),
        Box::new(docparse_md::MarkdownParser),
        Box::new(docparse_csv::CsvParser),
        Box::new(docparse_xlsx::XlsxParser),
        Box::new(docparse_pptx::PptxParser),
        Box::new(docparse_srt::SrtParser),
        Box::new(docparse_eml::EmlParser),
        Box::new(docparse_img::ImageParser), // 图片：扫描件/无文本层 → OCR 路由（parse-ocr）
    ]
}

/// OCR 增强（`parse-ocr` feature）：扫描件/图片无文本层的页经 PP-OCR 抽文本。仅当 env
/// `FASTSEARCH_OCR_MODELS` 指向 ONNX 模型目录（如 `docparse-rs/models/ppocr-v5`）时启用；
/// 未设则原样返回（解析层已给出的文本/图 chunk 不变）。重 ONNX 推理在此发生（非搜索热路径）。
#[cfg(feature = "parse-ocr")]
fn apply_ocr(doc: docparse_core::ir::Document) -> Result<docparse_core::ir::Document> {
    let Some(dir) = std::env::var_os("FASTSEARCH_OCR_MODELS") else {
        return Ok(doc);
    };
    let dir = std::path::PathBuf::from(dir);
    let ocr = docparse_ocr::PpOcrEnhancer::new(&dir)
        .with_context(|| format!("load PP-OCR models from {}", dir.display()))?;
    let (enhanced, routes) = docparse_core::enhance::apply(&doc, &[&ocr]);
    let applied = routes.iter().filter(|r| r.applied).count();
    eprintln!("OCR: {applied}/{} 页经增强（PP-OCR）", routes.len());
    Ok(enhanced)
}

#[cfg(not(feature = "parse-ocr"))]
fn apply_ocr(doc: docparse_core::ir::Document) -> Result<docparse_core::ir::Document> {
    Ok(doc)
}

/// 表格结构识别（`parse-tables` feature，**非 VLM 的确定性 ONNX 路**）：对解析层检测出的表格区域
/// （`Element::Table`），从源 PDF 栅格化裁剪 → UniRec 重识别为结构化 HTML 表格。仅 PDF（需源字节
/// 栅格化）+ env `FASTSEARCH_UNIREC_MODELS`（UniRec 模型目录）时启用；否则原样（解析层表格不变）。
#[cfg(feature = "parse-tables")]
fn apply_tables(
    mut doc: docparse_core::ir::Document,
    file: &std::path::Path,
) -> Result<docparse_core::ir::Document> {
    let Some(dir) = std::env::var_os("FASTSEARCH_UNIREC_MODELS") else {
        return Ok(doc);
    };
    let is_pdf = file
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("pdf"));
    if !is_pdf {
        return Ok(doc); // 非 PDF：表格已由解析器（docx/html/xlsx）结构化，无需 UniRec 栅格识别
    }
    let bytes = std::fs::read(file).with_context(|| format!("read pdf {}", file.display()))?;
    let unirec =
        docparse_ocr::unirec::UniRec::new(std::path::Path::new(&dir)).with_context(|| {
            format!(
                "load UniRec models from {}",
                std::path::Path::new(&dir).display()
            )
        })?;
    let n = docparse_ocr::table_model::refine_tables(&mut doc, bytes, &unirec)
        .context("UniRec refine_tables")?;
    eprintln!("UniRec: 重识别 {n} 个表格结构（非 VLM）");
    Ok(doc)
}

#[cfg(not(feature = "parse-tables"))]
fn apply_tables(
    doc: docparse_core::ir::Document,
    _file: &std::path::Path,
) -> Result<docparse_core::ir::Document> {
    Ok(doc)
}

/// 每文档最多送给 VLM 的页数（`FASTSEARCH_VLM_MAX_PAGES` 可覆盖）。一份 100 页 PDF
/// 每页 ~15 个区域就是 1500 次调用——无闸的话一次 `ingest` 会跑成小时级。
#[cfg(feature = "parse-vlm")]
const VLM_DEFAULT_MAX_PAGES: usize = 50;

/// VLM 区域识别（`parse-vlm` feature，**需外部 OpenAI 兼容服务**，如 vLLM 起 OvisOCR2）。
/// 仅当 `FASTSEARCH_VLM_URL` + `FASTSEARCH_VLM_MODEL` 都设时启用；否则原样返回。
///
/// **能力按配置浮现**（同 `apply_ocr`/`apply_tables` 的 env 门控风格）：
/// - 恒定：表格区域经 VLM 重识别为 HTML 表（保留 rowspan/colspan 拓扑）；
/// - 额外：`FASTSEARCH_LAYOUT_MODEL` 指向版面 ONNX 时，再做整页**区域级**转写。
///
/// **坐标不丢**：区域几何来自版面/表格检测，VLM 只负责"读"——`resolve_citation`
/// 的页内高亮因此仍然成立（整页端到端模式会丢正文坐标，故不走那条路）。
///
/// 仅 PDF（需源字节栅格化，同 `apply_tables`）；图片扫描件照旧走 `apply_ocr`（PP-OCR）。
/// 任何服务失败 → 保留确定性结果，解析不失败。
#[cfg(feature = "parse-vlm")]
fn apply_vlm(
    mut doc: docparse_core::ir::Document,
    file: &std::path::Path,
) -> Result<docparse_core::ir::Document> {
    use docparse_core::region_reader::RegionReader as _; // source_tag()
    use docparse_vlm::region::VlmRegionReader;

    let (Some(url), Some(model)) = (
        std::env::var_os("FASTSEARCH_VLM_URL"),
        std::env::var_os("FASTSEARCH_VLM_MODEL"),
    ) else {
        return Ok(doc);
    };
    let is_pdf = file
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("pdf"));
    if !is_pdf {
        return Ok(doc); // 非 PDF：无源字节可栅格化；图片走 apply_ocr
    }
    let cfg = docparse_vlm::VlmConfig::new(
        url.to_string_lossy().into_owned(),
        model.to_string_lossy().into_owned(),
        std::env::var("FASTSEARCH_VLM_KEY").ok(),
    );
    let bytes = std::fs::read(file).with_context(|| format!("read pdf {}", file.display()))?;

    // 页数闸：超出的页保留确定性结果，并**明确告知**跳过了多少（不静默截断）。
    // 写坏了的 env 也要吭声——静默按默认值跑会让人以为闸生效了。
    let max_pages = match std::env::var("FASTSEARCH_VLM_MAX_PAGES") {
        Err(_) => VLM_DEFAULT_MAX_PAGES,
        Ok(v) => v.parse().unwrap_or_else(|_| {
            eprintln!(
                "VLM: FASTSEARCH_VLM_MAX_PAGES={v:?} 不是合法页数，按默认 {VLM_DEFAULT_MAX_PAGES} 处理"
            );
            VLM_DEFAULT_MAX_PAGES
        }),
    };
    let tail = doc.pages.split_off(max_pages.min(doc.pages.len()));
    if !tail.is_empty() {
        eprintln!(
            "VLM: 仅处理前 {max_pages} 页，跳过 {} 页（FASTSEARCH_VLM_MAX_PAGES 可调）",
            tail.len()
        );
    }

    let table_reader = VlmRegionReader::for_table(cfg.clone()).context("build VLM table reader")?;
    match docparse_ocr::table_model::refine_tables(&mut doc, bytes.clone(), &table_reader) {
        Ok(n) => eprintln!(
            "VLM: 重识别 {n} 个表格结构（{}）",
            table_reader.source_tag()
        ),
        Err(e) => eprintln!("VLM: 表格重识别整体失败，保留确定性结果: {e:#}"),
    }

    // 转写要区域几何，故以版面模型的存在为开关。
    if let Some(layout_path) = std::env::var_os("FASTSEARCH_LAYOUT_MODEL") {
        let layout = docparse_ocr::layout::LayoutModel::new(std::path::Path::new(&layout_path))
            .with_context(|| {
                format!(
                    "load layout model from {}",
                    std::path::Path::new(&layout_path).display()
                )
            })?;
        let text_reader = VlmRegionReader::for_text(cfg).context("build VLM text reader")?;
        match docparse_ocr::transcribe::transcribe_pages(&mut doc, bytes, &layout, &text_reader) {
            Ok(n) => eprintln!("VLM: 区域级转写 {n} 页（{}）", text_reader.source_tag()),
            Err(e) => eprintln!("VLM: 转写整体失败，保留确定性结果: {e:#}"),
        }
    }

    doc.pages.extend(tail);
    Ok(doc)
}

#[cfg(not(feature = "parse-vlm"))]
fn apply_vlm(
    doc: docparse_core::ir::Document,
    _file: &std::path::Path,
) -> Result<docparse_core::ir::Document> {
    Ok(doc)
}

// ==================== 图片字节闭环（KB-1.1） ====================
//
// 背景：`docparse_core::chunk::ImageMeta.data_base64` 在本条摄取路径上**永远是空的**
// （只有 docparse-cli 的 `--image-embed` 才填它，而那是它自己 bin crate 里的私有函数），
// 于是文档图片过去只剩坐标 + caption 文本，`media_bytes` 恒 None。原始字节其实够得着：
// `docparse_core::ir::ImageChunk.data` 是 pub 的（`#[serde(skip)]`，只在进程内可见）。
// 这里在 CLI 侧自己搭桥，**不动 vendor**（vendor 是被根 `exclude` 的独立 workspace）。

/// 图片字节与 chunk 的连接键：`(page, bbox)`。
///
/// `chunk_document` 把 `Element::Image` 的 `page`/`bbox` **原样**抄进 image chunk（同一份
/// 数据的拷贝，中间没有任何浮点运算），所以按位比较 f32 是精确且无歧义的。
type ImageKey = (usize, [u32; 4]);

fn image_key(page: usize, b: &docparse_core::ir::BBox) -> ImageKey {
    (
        page,
        [
            b.x0.to_bits(),
            b.y0.to_bits(),
            b.x1.to_bits(),
            b.y1.to_bits(),
        ],
    )
}

/// 单张图的字节采集结果。
enum ImagePayload {
    /// 字节**可原样使用**：`Jpeg`（PDF DCTDecode 直通）/ `Encoded`（DOCX/PPTX/HTML 的媒体
    /// 文件字节）。零新依赖，本迭代支持的就是这两类。
    Ready { media_type: String, bytes: Vec<u8> },
    /// 有像素但**需 PNG 编码**才能用：`Rgb8` / `Gray8`（PDF 里 Flate/CCITT/JBIG2/JPX 等解出
    /// 来的裸位图）。**本迭代不支持**——现成编码器是 `docparse_vlm::encode_png_rgb`，引它会把
    /// `docparse-vlm`(+`docparse-raster`) 拉进 `parse` **轻档**，使"轻档无 ONNX/无渲染"的
    /// 依赖面变大；自己写 PNG 编码器则要新增 `png`/`flate2` 依赖。两条路都得单独评估收口，
    /// 故先落 Jpeg/Encoded 并**如实标注**（见下：状态标 `missing_bytes`，不伪装成已嵌入）。
    NeedsEncode,
    /// 解析层就没给字节：`ImageKind::None`（不支持的编码：CMYK JPEG、多通道 JPX…）或
    /// 低于后端的尺寸闸。
    NoBytes,
}

/// 采集到的图片字节，按 `(page, bbox)` 索引。同键可挂多张（PDF 允许两张图完全重叠），
/// 按出现顺序排队消费。
#[derive(Default)]
struct ImageBytesIndex {
    by_pos: HashMap<ImageKey, Vec<ImagePayload>>,
}

impl ImageBytesIndex {
    fn take(&mut self, page: usize, bbox: &docparse_core::ir::BBox) -> Option<ImagePayload> {
        let slot = self.by_pos.get_mut(&image_key(page, bbox))?;
        (!slot.is_empty()).then(|| slot.remove(0))
    }
}

/// 从解析后的文档里**取走**图片原始字节（`std::mem::take`，不复制、早释放）。
/// 必须在所有增强器之后、`chunk_document` 之前调用：增强器可能重建页面。
fn harvest_image_bytes(doc: &mut docparse_core::ir::Document) -> ImageBytesIndex {
    use docparse_core::ir::{Element, ImageKind};
    let mut idx = ImageBytesIndex::default();
    for page in &mut doc.pages {
        for el in &mut page.elements {
            let Element::Image(img) = el else { continue };
            let payload = match img.kind {
                ImageKind::Jpeg if !img.data.is_empty() => ImagePayload::Ready {
                    media_type: "image/jpeg".to_string(),
                    bytes: std::mem::take(&mut img.data),
                },
                // 已编码媒体（DOCX/PPTX/HTML）：字节原样直通，MIME 用源声明的。
                ImageKind::Encoded if !img.data.is_empty() => ImagePayload::Ready {
                    media_type: img
                        .data_media_type
                        .clone()
                        .unwrap_or_else(|| "application/octet-stream".to_string()),
                    bytes: std::mem::take(&mut img.data),
                },
                ImageKind::Rgb8 | ImageKind::Gray8 => {
                    img.data = Vec::new(); // 用不上（见 NeedsEncode 注释），立刻还内存
                    ImagePayload::NeedsEncode
                }
                _ => ImagePayload::NoBytes,
            };
            idx.by_pos
                .entry(image_key(img.page, &img.bbox))
                .or_default()
                .push(payload);
        }
    }
    idx
}

/// 图片字节闭环的记账（如实汇报，别让"没进系统"静默）。
#[derive(Default, Debug, PartialEq, Eq)]
struct ImageBytesStats {
    /// 字节已闭环（`Jpeg`/`Encoded` 直通）。
    attached: usize,
    /// 有像素但需 PNG 重编码（`Rgb8`/`Gray8`）——本迭代不支持。
    needs_encode: usize,
    /// 解析层无字节 / 未匹配到源图。
    no_bytes: usize,
    /// 已附字节合计（用于 20MB 请求体上限提醒）。
    total_bytes: usize,
}

/// 把采集到的字节挂到已适配的 fastsearch chunk 上。
///
/// **坐标不动**：只写 `media_bytes` / `media.asset` / `media.media_type` /
/// `image_vector_status`，`page`/`bbox`/`region` 一概不碰——`resolve_citation` 的页内高亮
/// 因此不受影响。拿不到字节的图**如实**标 `missing_bytes`，绝不留成"看起来已嵌入"。
fn attach_image_bytes(
    chunks: &mut [Chunk],
    dchunks: &[docparse_core::chunk::Chunk],
    idx: &mut ImageBytesIndex,
) -> ImageBytesStats {
    let mut st = ImageBytesStats::default();
    for (c, dc) in chunks.iter_mut().zip(dchunks) {
        if c.kind != ChunkKind::Image {
            continue;
        }
        match idx.take(dc.page, &dc.bbox) {
            Some(ImagePayload::Ready { media_type, bytes }) => {
                st.attached += 1;
                st.total_bytes += bytes.len();
                c.media_bytes = Some(bytes);
                if let Some(m) = &mut c.media {
                    // Inline = "字节随本请求走"。server 在 object/auto 档会把它改写成
                    // Object{uri}（上传后），inline 档则原样保留、字节落 PG bytea。
                    m.asset = AssetPointer::Inline;
                    m.media_type = Some(media_type);
                }
                // 状态交给 server 判定（有 embedder → Embedded，无则 Pending）。
                c.image_vector_status = None;
            }
            Some(ImagePayload::NeedsEncode) => {
                st.needs_encode += 1;
                c.image_vector_status = Some(ImageVectorStatus::MissingBytes);
            }
            _ => {
                st.no_bytes += 1;
                c.image_vector_status = Some(ImageVectorStatus::MissingBytes);
            }
        }
    }
    st
}

/// server 的 `DefaultBodyLimit` 是 20MB；越过这条软线就提醒改 `--images none` 或
/// 走 `/v1/images` 单张上传，别等 413 才发现。
const BODY_SOFT_LIMIT: usize = 16 * 1024 * 1024;

/// **解析 → 增强 → 分块 → 适配**（纯本地，**不碰网络**）。`cmd_ingest` = 本函数 + POST。
/// 单独成函数是为了让图片字节闭环能在无 server / 无外部模型的条件下被单测覆盖。
pub fn chunks_for_file(opts: &ParseOptions) -> Result<Vec<Chunk>> {
    let registry = parsers(opts.images.wants_bytes());
    let parser = registry
        .iter()
        .find(|p| p.supports(&opts.file))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no docparse parser supports {}（支持：pdf/docx/html/md/csv/xlsx/pptx/srt/eml）",
                opts.file.display()
            )
        })?;
    let doc = parser
        .parse(&opts.file)
        .with_context(|| format!("docparse {} parse {}", parser.name(), opts.file.display()))?;
    // 增强器串联，每一段都由 feature + env 双重门控，未配则恒等。
    //
    // 顺序上 VLM 在 UniRec **之前**：两者作用于同一批 `Element::Table`，都配上时
    // VLM 优先（显式配了服务就是想用它），UniRec 再兜底 VLM 失败/拒收的表——
    // `refine_tables` 跳过 source 已是 `table:vlm:` 的表，故不会连跑两次推理、
    // 也不会互相盲目覆盖。
    let doc = if opts.enhancements.ocr {
        apply_ocr(doc)?
    } else {
        doc
    };
    let doc = if opts.enhancements.vlm {
        apply_vlm(doc, &opts.file)?
    } else {
        doc
    };
    let mut doc = if opts.enhancements.tables {
        apply_tables(doc, &opts.file)?
    } else {
        doc
    };
    // 图片字节在增强器之后、分块之前取走（增强器可能重建页面）。`none` 档完全不进这条路。
    let mut images = opts
        .images
        .wants_bytes()
        .then(|| harvest_image_bytes(&mut doc));
    let dchunks = docparse_core::chunk::chunk_document_with(
        &doc,
        docparse_core::chunk::ChunkOptions {
            target_chars: opts.chunk_profile.target_chars(),
            overlap_chars: opts.chunk_profile.overlap_chars(),
            table_markdown: opts.chunk_profile.table_markdown(),
        },
    );
    let mut chunks: Vec<Chunk> = dchunks
        .iter()
        .map(|d| {
            let mut chunk =
                from_docparse_chunk(d, &opts.doc_id, opts.tenant.clone(), opts.acl.clone());
            opts.chunk_profile.attach_to(&mut chunk, "docparse");
            chunk
        })
        .collect();
    if let Some(idx) = &mut images {
        let st = attach_image_bytes(&mut chunks, &dchunks, idx);
        if st.attached + st.needs_encode + st.no_bytes > 0 {
            eprintln!(
                "images: {} 张字节已随索引上传（{} KiB）；{} 张需 PNG 重编码（Rgb8/Gray8，本版不支持）；\
                 {} 张解析层无字节——后两类如实标 image_vector_status=missing_bytes",
                st.attached,
                st.total_bytes / 1024,
                st.needs_encode,
                st.no_bytes
            );
        }
        if st.total_bytes > BODY_SOFT_LIMIT {
            eprintln!(
                "images: 媒资合计 {} MiB，逼近 server 的 20MB 请求体上限——若被 413 拒，\
                 改用 --images none 或对大图走 `fastsearch upload-image`（/v1/images 单张上传）",
                st.total_bytes / (1024 * 1024)
            );
        }
    }
    if std::env::var_os("FASTSEARCH_INGEST_DEBUG").is_some() {
        for (i, c) in chunks.iter().enumerate() {
            let t: String = c.text.chars().take(60).collect();
            eprintln!(
                "  chunk[{i}] kind={:?} page={} bbox={:?} media_bytes={:?} img_status={:?} text={t:?}",
                c.kind,
                c.page,
                c.bbox,
                c.media_bytes.as_ref().map(|b| b.len()),
                c.image_vector_status.map(|s| s.as_str()),
            );
        }
    }
    Ok(chunks)
}

/// docparse ChunkKind → fastsearch ChunkKind（前 6 类同构；Audio/Video 来自媒资预处理，非 PDF）。
fn map_kind(k: docparse_core::chunk::ChunkKind) -> ChunkKind {
    use docparse_core::chunk::ChunkKind as D;
    match k {
        D::Heading => ChunkKind::Heading,
        D::Paragraph => ChunkKind::Paragraph,
        D::Table => ChunkKind::Table,
        D::Code => ChunkKind::Code,
        D::ListItem => ChunkKind::ListItem,
        D::Image => ChunkKind::Image,
    }
}

fn map_bbox(b: docparse_core::ir::BBox) -> BBox {
    BBox {
        x0: b.x0,
        y0: b.y0,
        x1: b.x1,
        y1: b.y1,
    }
}

/// docparse `ImageMeta` → fastsearch `MediaRef`（融合 §2 映射）：
/// `data_base64`→`Inline`（字节走 PG bytea，MM2）/ `file`→`Object{uri}` / 皆无→`DocRegion`（跳原文）。
fn map_image(im: &docparse_core::chunk::ImageMeta, page: u32, bbox: BBox) -> MediaRef {
    let asset = if im.data_base64.is_some() {
        AssetPointer::Inline
    } else if let Some(file) = &im.file {
        AssetPointer::Object { uri: file.clone() }
    } else {
        AssetPointer::DocRegion { page, bbox }
    };
    MediaRef {
        asset,
        media_type: im.media_type.clone(),
        time: None, // PDF 图无时间维
        region: Some(bbox),
        caption_source: im.caption_source.clone(),
        thumbnail: None,
    }
}

fn decode_image_bytes(im: &docparse_core::chunk::ImageMeta) -> Option<Vec<u8>> {
    im.data_base64.as_ref().and_then(|s| {
        let raw = s.rsplit_once(',').map(|(_, b64)| b64).unwrap_or(s);
        B64.decode(raw.trim()).ok()
    })
}

/// 把 docparse chunk 适配成 fastsearch chunk，注入摄取期元数据（`doc_id`/`tenant`/`acl`）。
pub fn from_docparse_chunk(
    dc: &docparse_core::chunk::Chunk,
    doc_id: &str,
    tenant: Option<String>,
    acl: Vec<String>,
) -> Chunk {
    let bbox = map_bbox(dc.bbox);
    Chunk {
        doc_id: doc_id.to_string(),
        chunk_id: dc.id as u64,
        kind: map_kind(dc.kind),
        text: dc.text.clone(),
        page: dc.page as u32,
        bbox,
        heading_path: dc.heading_path.clone(),
        section_id: dc.section_id as u64,
        char_len: dc.char_len as u32,
        // 媒资统一走 media（融合后的单一目标）；不再用遗留 image_meta。
        media: dc
            .image
            .as_ref()
            .map(|im| map_image(im, dc.page as u32, bbox)),
        media_bytes: dc.image.as_ref().and_then(decode_image_bytes),
        image_vector_status: None,
        tenant,
        acl,
        metadata: Default::default(),
        searchable: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T6 — 未配服务时 `apply_vlm` 必须是**恒等变换**：不建 client、不发请求、
    /// 不改文档。这是"重依赖 opt-in"的可执行断言——没配 env 的用户（含 CI）
    /// 走的路径必须与没编译这个 feature 时完全一致。
    ///
    /// 无 `parse-vlm` feature 时同样跑：那条 `#[cfg(not(...))]` 分支也要守恒等。
    #[test]
    fn apply_vlm_is_identity_without_service_env() {
        // 本测试只在 env 未设时有意义；设了就跳过（不去动进程全局 env，
        // 那会与并行跑的其他测试打架）。
        if std::env::var_os("FASTSEARCH_VLM_URL").is_some() {
            eprintln!("skip apply_vlm_is_identity_without_service_env: FASTSEARCH_VLM_URL set");
            return;
        }
        let doc = docparse_core::ir::Document {
            source: "r.pdf".into(),
            provenance: None,
            pages: vec![docparse_core::ir::Page {
                number: 1,
                width: 612.0,
                height: 792.0,
                elements: Vec::new(),
            }],
        };
        // 存在的 .pdf 路径：若真去读文件/发请求，这里会露馅（文件是空的，
        // 栅格化必失败）——恒等分支必须在此之前就返回。
        let dir = tempfile::tempdir().unwrap();
        let pdf = dir.path().join("r.pdf");
        std::fs::write(&pdf, b"").unwrap();

        let out =
            apply_vlm(doc.clone(), &pdf).expect("no service configured → no-op, not an error");
        assert_eq!(out.pages.len(), doc.pages.len());
        assert_eq!(out.source, doc.source);
    }

    fn dc_chunk(
        id: usize,
        kind: docparse_core::chunk::ChunkKind,
        text: &str,
    ) -> docparse_core::chunk::Chunk {
        docparse_core::chunk::Chunk {
            id,
            kind,
            text: text.into(),
            page: 3,
            bbox: docparse_core::ir::BBox {
                x0: 1.0,
                y0: 2.0,
                x1: 3.0,
                y1: 4.0,
            },
            heading_path: vec!["第3章".into(), "财务".into()],
            section_id: 7,
            char_len: text.chars().count(),
            image: None,
        }
    }

    #[test]
    fn adapts_text_chunk() {
        let dc = dc_chunk(5, docparse_core::chunk::ChunkKind::Paragraph, "毛利率提升");
        let c = from_docparse_chunk(&dc, "rep.pdf", Some("acme".into()), vec!["team-a".into()]);
        assert_eq!(c.chunk_id, 5);
        assert_eq!(c.doc_id, "rep.pdf");
        assert_eq!(c.kind, ChunkKind::Paragraph);
        assert_eq!(c.text, "毛利率提升");
        assert_eq!(c.page, 3);
        assert_eq!(c.bbox.x1, 3.0);
        assert_eq!(c.heading_path, vec!["第3章", "财务"]);
        assert_eq!(c.section_id, 7);
        assert_eq!(c.tenant.as_deref(), Some("acme"));
        assert_eq!(c.acl, vec!["team-a".to_string()]);
        assert!(c.media.is_none());
        // 模态由 kind 派生
        assert_eq!(c.kind.modality(), fastsearch_core::Modality::Text);
    }

    #[test]
    fn adapts_image_to_mediaref() {
        // data_base64 存在 → Inline
        let mut dc = dc_chunk(1, docparse_core::chunk::ChunkKind::Image, "图1 营收趋势");
        dc.image = Some(docparse_core::chunk::ImageMeta {
            file: None,
            data_base64: Some("AAAA".into()),
            media_type: Some("image/png".into()),
            caption: Some("营收趋势".into()),
            caption_source: Some("caption-line".into()),
        });
        let c = from_docparse_chunk(&dc, "r.pdf", None, vec!["public".into()]);
        assert_eq!(c.kind, ChunkKind::Image);
        let m = c.media.as_ref().unwrap();
        assert!(matches!(m.asset, AssetPointer::Inline));
        assert_eq!(m.media_type.as_deref(), Some("image/png"));
        assert_eq!(m.caption_source.as_deref(), Some("caption-line"));
        assert!(m.region.is_some());

        // 只有 file → Object
        dc.image = Some(docparse_core::chunk::ImageMeta {
            file: Some("figs/1.png".into()),
            data_base64: None,
            media_type: Some("image/png".into()),
            caption: None,
            caption_source: None,
        });
        let c2 = from_docparse_chunk(&dc, "r.pdf", None, vec!["public".into()]);
        assert!(
            matches!(&c2.media.as_ref().unwrap().asset, AssetPointer::Object { uri } if uri == "figs/1.png")
        );

        // 皆无 → DocRegion（跳原文位置）
        dc.image = Some(docparse_core::chunk::ImageMeta {
            file: None,
            data_base64: None,
            media_type: None,
            caption: None,
            caption_source: None,
        });
        let c3 = from_docparse_chunk(&dc, "r.pdf", None, vec!["public".into()]);
        assert!(matches!(
            c3.media.as_ref().unwrap().asset,
            AssetPointer::DocRegion { page: 3, .. }
        ));
    }

    /// 多格式分发：md/html/csv 各写一个临时文件 → 注册表按扩展名选解析器 → 解析+分块 → 适配出
    /// 非空 fastsearch chunk。证明 `parse` feature 的多格式摄取端到端（轻量、无 ONNX、无网络）。
    #[test]
    fn multiformat_dispatch_parses_and_adapts() {
        let dir = tempfile::tempdir().unwrap();
        let cases = [
            ("doc.md", "# 标题\n\n正文段落，含**毛利率**。\n"),
            (
                "page.html",
                "<html><body><h1>纪要</h1><p>净利润上升。</p></body></html>",
            ),
            ("data.csv", "name,val\n甲,1\n乙,2\n"),
        ];
        let registry = parsers(false);
        for (fname, content) in cases {
            let path = dir.path().join(fname);
            std::fs::write(&path, content).unwrap();
            let parser = registry
                .iter()
                .find(|p| p.supports(&path))
                .unwrap_or_else(|| panic!("no parser supports {fname}"));
            let doc = parser
                .parse(&path)
                .unwrap_or_else(|e| panic!("parse {fname}: {e}"));
            let dchunks = docparse_core::chunk::chunk_document(&doc);
            assert!(!dchunks.is_empty(), "{fname} 应产出 chunk");
            let chunks: Vec<_> = dchunks
                .iter()
                .map(|d| from_docparse_chunk(d, fname, None, vec!["public".into()]))
                .collect();
            assert!(
                chunks.iter().any(|c| !c.text.is_empty()),
                "{fname} 适配后应有非空文本 chunk"
            );
        }
    }

    // ==================== 图片字节闭环（KB-1.1）====================
    //
    // 夹具（`tests/fixtures/`，一次性生成、已入库；**无需外部模型/服务/网络**）：
    // - `figure.jpg` / `figure.png`：同一张 240×180 图的两种编码，作"源图"基准；
    // - `with-image.pdf`：手写 PDF（612×792），Im0 = 上面的 JPEG（DCTDecode，绘制成
    //   300×250pt ≈ 15.5% 页覆盖 → 高于分块的 1% 闸、低于 PDF 后端"整页扫描候选"的 50% 闸，
    //   所以**只有打开 `decode_images` 才拿得到字节**，正好验证按需开关）；
    //   Im1 = Flate 原始 RGB 位图 40×32（→ `ImageKind::Rgb8`，本版不支持的那类）；
    // - `with-image.docx`：python-docx 造的 DOCX，内嵌 `figure.png`（→ `ImageKind::Encoded`）。

    fn fixture(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../fastsearch-cli/tests/fixtures")
            .join(name)
    }

    fn ingest_opts(file: &str, images: ImageBytes) -> ParseOptions {
        ParseOptions {
            file: fixture(file),
            doc_id: file.into(),
            tenant: None,
            acl: vec!["public".into()],
            images,
            chunk_profile: ChunkProfile::docparse_default(),
            enhancements: Enhancements::default(),
        }
    }

    #[test]
    fn chunks_for_file_records_the_selected_chunk_profile() {
        let mut opts = ingest_opts("with-image.docx", ImageBytes::None);
        opts.chunk_profile = ChunkProfile::new("annual-report", 3, 512, 64, true).unwrap();
        let chunks = chunks_for_file(&opts).expect("parse fixture");
        assert!(!chunks.is_empty());
        for chunk in chunks {
            assert_eq!(chunk.metadata["chunking"]["chunker"], "docparse");
            assert_eq!(chunk.metadata["chunking"]["profile"], "annual-report");
            assert_eq!(chunk.metadata["chunking"]["version"], 3);
            assert_eq!(chunk.metadata["chunking"]["target_chars"], 512);
            assert_eq!(chunk.metadata["chunking"]["overlap_chars"], 64);
            assert_eq!(chunk.metadata["chunking"]["table_markdown"], true);
        }
    }

    /// 夹具断言的**可读指纹**（FNV-1a 128 位）——只在断言失败时打印，用来一眼看出
    /// "字节完全不同"还是"差几个字节"。相等本身由 `assert_eq!(bytes, source)` 保证，
    /// 不需要密码学强度哈希，故不为测试引入 sha2 依赖。
    fn fingerprint(bytes: &[u8]) -> String {
        let mut h: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d_u128;
        for b in bytes {
            h ^= *b as u128;
            h = h.wrapping_mul(0x0000_0000_0001_0000_0000_0000_0000_013b_u128);
        }
        format!("{h:032x}")
    }

    /// 验收 1+2+4（PDF）：含图 PDF 的 `Jpeg` 图字节闭环，与源图**逐字节一致**；page/bbox 不变；
    /// 同页那张 `Rgb8` 裸位图**如实**标 `missing_bytes`，不伪装成已嵌入。
    #[test]
    fn pdf_jpeg_bytes_close_the_loop_and_rgb8_is_honest() {
        let source = std::fs::read(fixture("figure.jpg")).expect("read figure.jpg");
        let chunks = chunks_for_file(&ingest_opts("with-image.pdf", ImageBytes::Object))
            .expect("ingest pdf");
        let images: Vec<&Chunk> = chunks
            .iter()
            .filter(|c| c.kind == ChunkKind::Image)
            .collect();
        assert_eq!(images.len(), 2, "PDF 应产出 2 个图 chunk（JPEG + 裸位图）");

        // JPEG：字节非空且与源图完全一致。
        let jpeg = images
            .iter()
            .find(|c| c.media_bytes.is_some())
            .expect("应有一张图带回原始字节");
        let got = jpeg.media_bytes.as_ref().unwrap();
        assert_eq!(
            got.len(),
            source.len(),
            "字节长度不符：{} vs 源图 {}",
            fingerprint(got),
            fingerprint(&source)
        );
        assert_eq!(got, &source, "JPEG 字节必须与源图逐字节一致（hash 一致）");
        let m = jpeg.media.as_ref().expect("图 chunk 必有 media");
        assert!(
            matches!(m.asset, AssetPointer::Inline),
            "带字节的图 asset 应是 Inline（server 在 object 档再改写成 Object）"
        );
        assert_eq!(m.media_type.as_deref(), Some("image/jpeg"));
        // 坐标不变：region/page/bbox 仍指向页内位置 → resolve_citation 高亮不受影响。
        assert_eq!(jpeg.page, 1);
        assert_eq!(m.region, Some(jpeg.bbox));
        assert!(jpeg.bbox.x1 > jpeg.bbox.x0 && jpeg.bbox.y1 > jpeg.bbox.y0);
        assert!(
            jpeg.image_vector_status.is_none(),
            "有字节 → 状态交给 server"
        );

        // Rgb8 裸位图：本版不支持 → 无字节、状态如实。
        let raw = images
            .iter()
            .find(|c| c.media_bytes.is_none())
            .expect("应有一张图没有字节（Rgb8）");
        assert_eq!(
            raw.image_vector_status,
            Some(ImageVectorStatus::MissingBytes),
            "Rgb8/Gray8 必须如实标 missing_bytes"
        );
        assert!(
            !matches!(raw.media.as_ref().unwrap().asset, AssetPointer::Inline),
            "没有字节就不能声称 Inline"
        );
    }

    /// 验收 1+2（DOCX）：含图 DOCX 的 `Encoded` 图字节闭环，与源 PNG 逐字节一致。
    /// DOCX 的图字节本来就随解析出来，故不依赖 `decode_images`。
    #[test]
    fn docx_encoded_bytes_close_the_loop() {
        let source = std::fs::read(fixture("figure.png")).expect("read figure.png");
        let chunks = chunks_for_file(&ingest_opts("with-image.docx", ImageBytes::Object))
            .expect("ingest docx");
        let img = chunks
            .iter()
            .find(|c| c.kind == ChunkKind::Image)
            .expect("DOCX 应产出图 chunk");
        let got = img.media_bytes.as_ref().expect("DOCX 图应带回原始字节");
        assert_eq!(
            got,
            &source,
            "PNG 字节必须与源图逐字节一致（{} vs {}）",
            fingerprint(got),
            fingerprint(&source)
        );
        let m = img.media.as_ref().unwrap();
        assert!(matches!(m.asset, AssetPointer::Inline));
        assert_eq!(m.media_type.as_deref(), Some("image/png"));
        assert_eq!(m.region, Some(img.bbox));
        assert!(img.bbox.x1 > img.bbox.x0);
    }

    /// 验收 3：`--images none` 与本能力落地前**完全一致**——一个 `media_bytes` 都不采、
    /// `image_vector_status` 全 None、asset 仍是 DocRegion，且文本/坐标逐一相同。
    #[test]
    fn images_none_is_zero_regression() {
        for file in ["with-image.pdf", "with-image.docx"] {
            let off = chunks_for_file(&ingest_opts(file, ImageBytes::None)).expect("ingest none");
            let on =
                chunks_for_file(&ingest_opts(file, ImageBytes::Object)).expect("ingest object");
            assert_eq!(off.len(), on.len(), "{file}: 开关不该改变 chunk 数");
            for c in &off {
                assert!(c.media_bytes.is_none(), "{file}: none 档不得有字节");
                assert!(
                    c.image_vector_status.is_none(),
                    "{file}: none 档不得写 image_vector_status"
                );
                if c.kind == ChunkKind::Image {
                    assert!(
                        matches!(
                            c.media.as_ref().unwrap().asset,
                            AssetPointer::DocRegion { .. }
                        ),
                        "{file}: none 档图 asset 仍是 DocRegion（跳原文）"
                    );
                }
            }
            // 文本与坐标两档一致：字节闭环不得动检索/引用面。
            for (a, b) in off.iter().zip(&on) {
                assert_eq!(a.text, b.text, "{file}: 文本必须不变");
                assert_eq!((a.page, a.bbox), (b.page, b.bbox), "{file}: 坐标必须不变");
                assert_eq!(a.chunk_id, b.chunk_id);
            }
        }
    }

    #[test]
    fn image_mode_defaults_to_object_and_controls_byte_harvesting() {
        assert!(ImageBytes::Object.wants_bytes() && !ImageBytes::None.wants_bytes());
        assert_eq!(ImageBytes::default(), ImageBytes::Object);
    }

    /// `(page, bbox)` 连接键：同位置多图按出现顺序排队消费，用光后不再返回。
    #[test]
    fn image_index_queues_overlapping_images() {
        let bb = docparse_core::ir::BBox {
            x0: 1.0,
            y0: 2.0,
            x1: 3.0,
            y1: 4.0,
        };
        let mut idx = ImageBytesIndex::default();
        idx.by_pos.insert(
            image_key(2, &bb),
            vec![
                ImagePayload::Ready {
                    media_type: "image/jpeg".into(),
                    bytes: vec![1, 2, 3],
                },
                ImagePayload::NeedsEncode,
            ],
        );
        assert!(matches!(
            idx.take(2, &bb),
            Some(ImagePayload::Ready { bytes, .. }) if bytes == vec![1, 2, 3]
        ));
        assert!(matches!(idx.take(2, &bb), Some(ImagePayload::NeedsEncode)));
        assert!(idx.take(2, &bb).is_none(), "用光后不再返回");
        assert!(idx.take(1, &bb).is_none(), "页号不同不得串图");
    }

    /// OCR 端到端（env-gated，需运行时 ONNX 模型——同 PG 集成测试策略）：设
    /// `FASTSEARCH_OCR_MODELS`（PP-OCR 模型目录，如 `docparse-rs/models/ppocr-v5`）+
    /// `FASTSEARCH_OCR_TEST_IMAGE`（一张含文字的图片）才跑。验证：图片经 OCR 增强后产出**非空文本**
    /// chunk（vs 不开 OCR 仅 1 个图 chunk）。**真机验证 2026-06-27**：omnidocbench 数据表页 →
    /// "Impedance/Reference/BLM18AG121SN1D" 等 OCR 文本可检索。
    #[cfg(feature = "parse-ocr")]
    #[test]
    fn ocr_end_to_end_gated() {
        let (Some(models), Some(img)) = (
            std::env::var_os("FASTSEARCH_OCR_MODELS"),
            std::env::var_os("FASTSEARCH_OCR_TEST_IMAGE"),
        ) else {
            eprintln!("skip ocr_end_to_end_gated: FASTSEARCH_OCR_MODELS / _TEST_IMAGE not set");
            return;
        };
        use docparse_core::parser::DocumentParser;
        let path = std::path::PathBuf::from(&img);
        let doc = docparse_img::ImageParser.parse(&path).expect("parse image");
        // 不开 OCR：无文本（仅图）。
        let base: usize = docparse_core::chunk::chunk_document(&doc)
            .iter()
            .filter(|c| !c.text.trim().is_empty())
            .count();
        // 开 OCR：抽出文本 chunk。
        let ocr = docparse_ocr::PpOcrEnhancer::new(std::path::Path::new(&models))
            .expect("load PP-OCR models");
        let (enhanced, routes) = docparse_core::enhance::apply(&doc, &[&ocr]);
        assert!(routes.iter().any(|r| r.applied), "OCR 应至少增强一页");
        let with_text: usize = docparse_core::chunk::chunk_document(&enhanced)
            .iter()
            .filter(|c| !c.text.trim().is_empty())
            .count();
        assert!(
            with_text > base,
            "OCR 后应多出非空文本 chunk（base={base} ocr={with_text}）"
        );
    }

    /// 表格识别端到端（env-gated，**非 VLM** UniRec ONNX；需模型）：设 `FASTSEARCH_UNIREC_MODELS`
    /// （UniRec 模型目录，如 `docparse-rs/models/unirec`）+ `FASTSEARCH_TABLE_TEST_PDF`（PDF）才跑。
    /// 验证 `refine_tables` 路径端到端成立（解析→栅格化→UniRec→替换），返回精炼计数 ≥0 不报错。
    /// 注：CPU 上 UniRec 是 2000-token 自回归解码，单表可能耗时数分钟——故对**小表/无表 PDF** 验证。
    /// **真机验证 2026-06-28**：lorem.pdf（0 表）路径快速跑通；财务损益表单表精炼出结构化 HTML。
    #[cfg(feature = "parse-tables")]
    #[test]
    fn tables_refine_gated() {
        let (Some(models), Some(pdf)) = (
            std::env::var_os("FASTSEARCH_UNIREC_MODELS"),
            std::env::var_os("FASTSEARCH_TABLE_TEST_PDF"),
        ) else {
            eprintln!(
                "skip tables_refine_gated: FASTSEARCH_UNIREC_MODELS / _TABLE_TEST_PDF not set"
            );
            return;
        };
        use docparse_core::parser::DocumentParser;
        let path = std::path::PathBuf::from(&pdf);
        let mut doc = docparse_pdf::PdfParser::default()
            .parse(&path)
            .expect("parse pdf");
        let bytes = std::fs::read(&path).expect("read pdf");
        let unirec =
            docparse_ocr::unirec::UniRec::new(std::path::Path::new(&models)).expect("load UniRec");
        // 路径端到端成立（小表/无表即可快速验证 load+rasterize+recognize 链路无错）。
        let n = docparse_ocr::table_model::refine_tables(&mut doc, bytes, &unirec)
            .expect("refine_tables ok");
        eprintln!("tables_refine_gated: 精炼 {n} 个表格");
    }
}
