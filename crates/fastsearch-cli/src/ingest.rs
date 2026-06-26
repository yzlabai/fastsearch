//! 解析摄取（docparse-rs 融合 Option B，`parse` feature 门控）。
//!
//! 把 `docparse_core::Chunk`（解析关注点）适配成 `fastsearch_core::Chunk`（真源 schema
//! 加权限/媒资）。**这正是融合要消除"跨仓手工锁步"的焊点**：改任一侧 schema，本适配器
//! 编译即报错（见 [docparse 融合方案 §2](../../../docs/plans/2026-06-26-docparse融合方案评估.md)）。
//!
//! 搜索热路径（core/server/engine/...）不依赖任何 docparse crate；解析能力仅在本 feature 编译。

use anyhow::{Context, Result};
use fastsearch_core::{AssetPointer, BBox, Chunk, ChunkKind, MediaRef};
use fastsearch_text::TokenizerKind;
use std::path::PathBuf;

/// `fastsearch ingest <pdf>` 选项。
pub struct IngestOpts {
    pub pdf: PathBuf,
    pub data: PathBuf,
    pub collection: String,
    pub doc_id: String,
    pub tokenizer: TokenizerKind,
    pub tenant: Option<String>,
    pub acl: Vec<String>,
}

/// **in-process 解析 → 适配 → 索引**（doc 级替换）：读 PDF → docparse 解析+分块 →
/// `from_docparse_chunk` 适配 → 灌入现有落盘索引。返回灌入条数。
/// 这是融合 Option C 预演的"打通 解析→适配→索引"端到端路径（无需 shell 出 docparse JSON）。
pub fn cmd_ingest(opts: &IngestOpts) -> Result<usize> {
    let bytes =
        std::fs::read(&opts.pdf).with_context(|| format!("reading pdf {}", opts.pdf.display()))?;
    let doc = docparse_pdf::PdfParser::default()
        .parse_bytes(&bytes)
        .context("docparse pdf parse")?;
    let dchunks = docparse_core::chunk::chunk_document(&doc);
    let chunks: Vec<Chunk> = dchunks
        .iter()
        .map(|d| from_docparse_chunk(d, &opts.doc_id, opts.tenant.clone(), opts.acl.clone()))
        .collect();

    std::fs::create_dir_all(&opts.data)?;
    let tokenizer = crate::load_or_init_meta(&opts.data, opts.tokenizer)?;
    let mut engine = crate::open_engine(&opts.data, tokenizer)?;
    engine.remove_doc(&opts.collection, &opts.doc_id)?; // 替换语义
    for c in &chunks {
        engine.ingest(&opts.collection, c)?;
    }
    engine.commit()?;
    Ok(chunks.len())
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
        tenant,
        acl,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
