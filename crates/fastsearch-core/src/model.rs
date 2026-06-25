//! 文档/chunk 数据模型 + 引用模型。
//!
//! 字段与 docparse-rs 的 chunk schema 对齐（kind / page / bbox / heading_path /
//! section_id / char_len），额外加入 fastsearch 需要的 `tenant` / `acl`（访问控制）
//! 与向量元数据钩子。坐标系沿用 docparse：PDF 用户空间，原点左下、单位 pt。

use crate::error::{CoreError, Result};
use serde::{Deserialize, Serialize};

/// 轴对齐包围盒（PDF 用户空间，原点左下）。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BBox {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

/// chunk 内容类型。serde 用 snake_case，与 docparse 取值一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChunkKind {
    Heading,
    Paragraph,
    Table,
    Code,
    ListItem,
    Image,
}

/// 图片 chunk 的渲染/审计元数据（非图片 chunk 为 None）。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ImageMeta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caption_source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
}

fn default_acl() -> Vec<String> {
    vec!["public".to_string()]
}

/// 一条检索单元（= docparse 的一个 chunk + 访问控制）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Chunk {
    pub doc_id: String,
    pub chunk_id: u64,
    pub kind: ChunkKind,
    pub text: String,
    pub page: u32,
    pub bbox: BBox,
    #[serde(default)]
    pub heading_path: Vec<String>,
    #[serde(default)]
    pub section_id: u64,
    pub char_len: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_meta: Option<ImageMeta>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    #[serde(default = "default_acl")]
    pub acl: Vec<String>,
}

impl Chunk {
    /// 在某集合下的稳定全局标识。
    pub fn global_id(&self, collection: &str) -> GlobalId {
        GlobalId {
            collection: collection.to_string(),
            doc_id: self.doc_id.clone(),
            chunk_id: self.chunk_id,
        }
    }
}

/// `(collection, doc_id, chunk_id)` 的稳定标识，回指 PG / 去重 / 反解引用。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct GlobalId {
    pub collection: String,
    pub doc_id: String,
    pub chunk_id: u64,
}

impl GlobalId {
    /// 文本形式：`collection:doc_id:chunk_id`。doc_id 允许含 `:`，反解时
    /// 取首段为 collection、末段为 chunk_id、中间全部为 doc_id。
    pub fn to_citation_id(&self) -> String {
        format!("{}:{}:{}", self.collection, self.doc_id, self.chunk_id)
    }

    /// 反解 citation_id。
    pub fn parse(s: &str) -> Result<GlobalId> {
        let first = s
            .find(':')
            .ok_or_else(|| CoreError::InvalidCitation(s.to_string()))?;
        let last = s
            .rfind(':')
            .ok_or_else(|| CoreError::InvalidCitation(s.to_string()))?;
        if first == last {
            // 只有一个 ':'，缺字段
            return Err(CoreError::InvalidCitation(s.to_string()));
        }
        let collection = &s[..first];
        let doc_id = &s[first + 1..last];
        let chunk_id: u64 = s[last + 1..]
            .parse()
            .map_err(|_| CoreError::InvalidCitation(s.to_string()))?;
        if collection.is_empty() || doc_id.is_empty() {
            return Err(CoreError::InvalidCitation(s.to_string()));
        }
        Ok(GlobalId {
            collection: collection.to_string(),
            doc_id: doc_id.to_string(),
            chunk_id,
        })
    }
}

/// 命中的引用锚点（端到端溯源到 PDF 精确区域）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Citation {
    pub collection: String,
    pub doc_id: String,
    pub chunk_id: u64,
    pub page: u32,
    pub bbox: BBox,
    #[serde(default)]
    pub heading_path: Vec<String>,
    #[serde(default)]
    pub section_id: u64,
}

impl Citation {
    pub fn citation_id(&self) -> String {
        format!("{}:{}:{}", self.collection, self.doc_id, self.chunk_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_kind_serde_snake_case() {
        assert_eq!(
            serde_json::to_string(&ChunkKind::ListItem).unwrap(),
            "\"list_item\""
        );
        let k: ChunkKind = serde_json::from_str("\"table\"").unwrap();
        assert_eq!(k, ChunkKind::Table);
        // 未知值报错
        assert!(serde_json::from_str::<ChunkKind>("\"unknown\"").is_err());
    }

    #[test]
    fn chunk_defaults_acl_public() {
        let json = r#"{"doc_id":"a.pdf","chunk_id":1,"kind":"paragraph","text":"hi",
            "page":1,"bbox":{"x0":0.0,"y0":0.0,"x1":1.0,"y1":1.0},"char_len":2}"#;
        let c: Chunk = serde_json::from_str(json).unwrap();
        assert_eq!(c.acl, vec!["public".to_string()]);
        assert_eq!(c.section_id, 0);
        assert!(c.heading_path.is_empty());
    }

    #[test]
    fn global_id_and_citation_roundtrip() {
        let c = Chunk {
            doc_id: "report.pdf".into(),
            chunk_id: 152,
            kind: ChunkKind::Table,
            text: "x".into(),
            page: 23,
            bbox: BBox {
                x0: 0.0,
                y0: 0.0,
                x1: 1.0,
                y1: 1.0,
            },
            heading_path: vec![],
            section_id: 17,
            char_len: 1,
            image_meta: None,
            tenant: None,
            acl: default_acl(),
        };
        let gid = c.global_id("kb");
        let cid = gid.to_citation_id();
        assert_eq!(cid, "kb:report.pdf:152");
        assert_eq!(GlobalId::parse(&cid).unwrap(), gid);
    }

    #[test]
    fn citation_id_handles_docid_with_colon() {
        // doc_id 含 ':'，应正确反解（首段=collection，末段=chunk_id）
        let gid = GlobalId {
            collection: "kb".into(),
            doc_id: "dir:sub:file.pdf".into(),
            chunk_id: 9,
        };
        let cid = gid.to_citation_id();
        assert_eq!(cid, "kb:dir:sub:file.pdf:9");
        assert_eq!(GlobalId::parse(&cid).unwrap(), gid);
    }

    #[test]
    fn citation_id_invalid() {
        assert!(GlobalId::parse("nocolons").is_err());
        assert!(GlobalId::parse("only:one").is_err()); // 缺第三段
        assert!(GlobalId::parse("kb:doc:notanumber").is_err());
        assert!(GlobalId::parse(":doc:1").is_err()); // 空 collection
    }
}
