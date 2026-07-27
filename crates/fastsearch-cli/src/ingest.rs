//! 解析摄取（docparse-rs 融合 Option B，`parse` feature 门控）。
//!
//! 把 `docparse_core::Chunk`（解析关注点）适配成 `fastsearch_core::Chunk`（真源 schema
//! 加权限/媒资）。**这正是融合要消除"跨仓手工锁步"的焊点**：改任一侧 schema，本适配器
//! 编译即报错（见 [docparse 融合方案 §2](../../../docs/plans/2026-06-26-docparse融合方案评估.md)）。
//!
//! 搜索热路径（core/server/engine/...）不依赖任何 docparse crate；解析能力仅在本 feature 编译。

use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use fastsearch_core::{AssetPointer, BBox, Chunk, ChunkKind, MediaRef};
use std::path::PathBuf;

/// `fastsearch ingest <file>` 选项（**客户端侧解析** → POST `/v1/index`）。
pub struct IngestOpts {
    /// 待解析文件（按扩展名分发到对应 docparse 解析器；多格式）。
    pub file: PathBuf,
    pub server: Option<String>,
    pub key: Option<String>,
    pub collection: String,
    pub doc_id: String,
    pub tenant: Option<String>,
    pub acl: Vec<String>,
}

/// docparse 多格式解析器注册表（轻量、无 ONNX）：按 `DocumentParser::supports`（扩展名/magic）
/// 派发。重增强器经 feature + env 接入：OCR=`parse-ocr`、表格=`parse-tables`（均进程内 ONNX，
/// 指模型目录）；VLM 区域识别=`parse-vlm`（外部 OpenAI 兼容服务，指 URL+模型名，见 [`apply_vlm`]）。
/// 自然图 VLM 描述（`--vlm-describe` 那条 caption 路）仍未接入，属下一迭代。
fn parsers() -> Vec<Box<dyn docparse_core::parser::DocumentParser>> {
    vec![
        Box::new(docparse_pdf::PdfParser::default()),
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

/// **客户端解析 → 适配 → POST /v1/index**（doc 级替换由 server 保证）：按扩展名选 docparse
/// 解析器 → 解析+分块 → `from_docparse_chunk` 适配 → 上传 server。返回 indexed 条数。
/// 解析在客户端（守"搜索热路径零 docparse"+ CI 门禁）；检索/嵌入/落盘归 server。
pub fn cmd_ingest(opts: &IngestOpts) -> Result<usize> {
    let registry = parsers();
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
    let doc = apply_ocr(doc)?; // parse-ocr：扫描页 PP-OCR 抽文本
    let doc = apply_vlm(doc, &opts.file)?; // parse-vlm：VLM 区域识别（需服务）
    let doc = apply_tables(doc, &opts.file)?; // parse-tables：UniRec 表格结构
    let dchunks = docparse_core::chunk::chunk_document(&doc);
    let chunks: Vec<Chunk> = dchunks
        .iter()
        .map(|d| from_docparse_chunk(d, &opts.doc_id, opts.tenant.clone(), opts.acl.clone()))
        .collect();
    if std::env::var_os("FASTSEARCH_INGEST_DEBUG").is_some() {
        for (i, c) in chunks.iter().enumerate() {
            let t: String = c.text.chars().take(60).collect();
            eprintln!("  chunk[{i}] kind={:?} text={t:?}", c.kind);
        }
    }

    let client = crate::Client::new(opts.server.clone(), opts.key.clone());
    crate::post_index(&client, &opts.collection, &opts.doc_id, None, &chunks)
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
        let registry = parsers();
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
