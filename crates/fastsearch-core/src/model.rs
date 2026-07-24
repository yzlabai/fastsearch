//! 文档/chunk 数据模型 + 引用模型。
//!
//! 字段与 docparse-rs 的 chunk schema 对齐（kind / page / bbox / heading_path /
//! section_id / char_len），额外加入 fastsearch 需要的 `tenant` / `acl`（访问控制）
//! 与向量元数据钩子。坐标系沿用 docparse：PDF 用户空间，原点左下、单位 pt。

use crate::error::{CoreError, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// 调用方透传的 chunk 元数据。FastSearch 只存储和返回，不解释、不索引、不参与过滤。
pub type Metadata = Map<String, Value>;

/// 单个 chunk 元数据的最大 JSON 编码大小。
pub const MAX_METADATA_BYTES: usize = 64 * 1024;
/// 单个 chunk 元数据对象允许的最大总键数（含嵌套对象）。
pub const MAX_METADATA_KEYS: usize = 256;
/// 单个 chunk 元数据允许的最大 JSON 容器嵌套深度。
pub const MAX_METADATA_DEPTH: usize = 16;

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
    Audio,
    Video,
}

impl ChunkKind {
    /// 模态（嵌入路由 + 过滤下推用，见多模态计划 D4）。文本类→Text，图→Image，音→Audio，视频→Video。
    pub fn modality(self) -> Modality {
        match self {
            ChunkKind::Image => Modality::Image,
            ChunkKind::Audio => Modality::Audio,
            ChunkKind::Video => Modality::Video,
            _ => Modality::Text,
        }
    }
}

/// 检索/嵌入模态。serde snake_case，可作 `Filter` 字段值下推（普通元数据，非新搜索参数）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Modality {
    Text,
    Image,
    Audio,
    Video,
}

impl Modality {
    /// 落库/过滤用的稳定字符串（与 serde snake_case 一致）。
    pub fn as_str(self) -> &'static str {
        match self {
            Modality::Text => "text",
            Modality::Image => "image",
            Modality::Audio => "audio",
            Modality::Video => "video",
        }
    }

    /// 由 `kind` 的字符串形式派生模态（供只持有 kind 字符串的后端做过滤后过滤）。
    /// 未知 kind → Text。
    pub fn of_kind_str(kind: &str) -> Modality {
        match kind {
            "image" => Modality::Image,
            "audio" => Modality::Audio,
            "video" => Modality::Video,
            _ => Modality::Text,
        }
    }
}

/// 音视频时间区间（毫秒）。用于深链与时间过滤。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeSpan {
    pub start_ms: u64,
    pub end_ms: u64,
}

/// 如何取到媒资字节。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum AssetPointer {
    /// 字节在 PG `bytea`（小裁图，随逻辑复制走）。
    Inline,
    /// 对象存储 key/uri（大媒资）。
    Object { uri: String },
    /// 仅坐标无字节：跳转到原文位置（不能直接产出可显示字节）。
    DocRegion { page: u32, bbox: BBox },
}

/// 对外可暴露的媒资指针。Object 只暴露种类，不暴露 uri/bucket/key/etag/sha256 等内部定位信息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum PublicAssetPointer {
    Inline,
    Object,
    DocRegion { page: u32, bbox: BBox },
}

/// 对外 API/MCP/SDK 使用的媒资 DTO。由 [`MediaRef`] 脱敏得到。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PublicMediaRef {
    pub asset: PublicAssetPointer,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<TimeSpan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<BBox>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<PublicAssetPointer>,
}

impl From<&AssetPointer> for PublicAssetPointer {
    fn from(asset: &AssetPointer) -> Self {
        match asset {
            AssetPointer::Inline => PublicAssetPointer::Inline,
            AssetPointer::Object { .. } => PublicAssetPointer::Object,
            AssetPointer::DocRegion { page, bbox } => PublicAssetPointer::DocRegion {
                page: *page,
                bbox: *bbox,
            },
        }
    }
}

/// 媒资引用（图/音/视频的渲染与取字节所需；替换原 `ImageMeta` 的超集，迁移见多模态计划 §6）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaRef {
    pub asset: AssetPointer,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<TimeSpan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<BBox>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<AssetPointer>,
}

impl MediaRef {
    /// 统一媒资脱敏出口：Object 裸 uri 和缩略图 uri 都不外泄。
    pub fn to_public(&self) -> PublicMediaRef {
        PublicMediaRef {
            asset: PublicAssetPointer::from(&self.asset),
            media_type: self.media_type.clone(),
            time: self.time,
            region: self.region,
            caption_source: self.caption_source.clone(),
            thumbnail: self.thumbnail.as_ref().map(PublicAssetPointer::from),
        }
    }
}

/// 图片视觉向量状态。PG 是真源；派生索引和 explain 只能读/透出该状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageVectorStatus {
    Pending,
    Embedded,
    TextFallback,
    MissingBytes,
    AssetMissing,
    Error,
}

impl ImageVectorStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ImageVectorStatus::Pending => "pending",
            ImageVectorStatus::Embedded => "embedded",
            ImageVectorStatus::TextFallback => "text_fallback",
            ImageVectorStatus::MissingBytes => "missing_bytes",
            ImageVectorStatus::AssetMissing => "asset_missing",
            ImageVectorStatus::Error => "error",
        }
    }
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
    pub data_base64: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
}

impl ImageMeta {
    /// 迁移到统一的 [`MediaRef`]（`file`→`Object`，否则 `DocRegion` 跳原文；带 page+bbox）。
    /// 供消费旧 docparse `image` 字段的入口（如 CLI JSON index）映射到 `media`。
    pub fn to_media(&self, page: u32, bbox: BBox) -> MediaRef {
        let asset = match &self.file {
            Some(f) => AssetPointer::Object { uri: f.clone() },
            None => AssetPointer::DocRegion { page, bbox },
        };
        MediaRef {
            asset,
            media_type: self.media_type.clone(),
            time: None,
            region: Some(bbox),
            caption_source: self.caption_source.clone(),
            thumbnail: None,
        }
    }
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
    /// **可检索文本表示**（正文 / caption / 转录）；媒资无文本时为 `""`（空串=不进 BM25）。
    pub text: String,
    pub page: u32,
    pub bbox: BBox,
    #[serde(default)]
    pub heading_path: Vec<String>,
    #[serde(default)]
    pub section_id: u64,
    pub char_len: u32,
    /// 媒资引用（图/音/视频；统一目标，遗留 `image_meta` 已迁移至此，见 MM2b）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media: Option<MediaRef>,
    /// inline 媒资字节（小裁图，`AssetPointer::Inline` 时有值；落 PG `media_bytes` 真源，MM2c-bytes）。
    /// 写侧通道：不进 JSON 线缆/不上 Citation（字节是真源内容，由媒资网关按需从 PG 取）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_bytes: Option<Vec<u8>>,
    /// 图片向量状态（PG 真源）。非图片 chunk 通常为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_vector_status: Option<ImageVectorStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    #[serde(default = "default_acl")]
    pub acl: Vec<String>,
    /// 调用方透传元数据。仅存储/返回，不建立索引，也不参与 FastSearch 业务语义。
    #[serde(default)]
    pub metadata: Metadata,
    /// 是否进入全文/向量检索。`false` 的 chunk 仍保存在真源，供管理 API 按主键读取。
    #[serde(default = "default_searchable")]
    pub searchable: bool,
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

    /// 校验透传元数据的资源边界。FastSearch 不解释字段，只限制大小与结构复杂度。
    pub fn validate_metadata(&self) -> Result<()> {
        let encoded = serde_json::to_vec(&self.metadata)
            .map_err(|e| CoreError::InvalidRequest(format!("metadata is not valid JSON: {e}")))?;
        if encoded.len() > MAX_METADATA_BYTES {
            return Err(CoreError::InvalidRequest(format!(
                "metadata must be <= {MAX_METADATA_BYTES} bytes"
            )));
        }

        let (keys, depth) = metadata_shape(&Value::Object(self.metadata.clone()), 0);
        if keys > MAX_METADATA_KEYS {
            return Err(CoreError::InvalidRequest(format!(
                "metadata must contain <= {MAX_METADATA_KEYS} keys"
            )));
        }
        if depth > MAX_METADATA_DEPTH {
            return Err(CoreError::InvalidRequest(format!(
                "metadata depth must be <= {MAX_METADATA_DEPTH}"
            )));
        }
        Ok(())
    }
}

fn default_searchable() -> bool {
    true
}

fn metadata_shape(value: &Value, depth: usize) -> (usize, usize) {
    match value {
        Value::Object(map) => map.iter().fold((map.len(), depth + 1), |acc, (_, value)| {
            let child = metadata_shape(value, depth + 1);
            (acc.0 + child.0, acc.1.max(child.1))
        }),
        Value::Array(values) => values.iter().fold((0, depth + 1), |acc, value| {
            let child = metadata_shape(value, depth + 1);
            (acc.0 + child.0, acc.1.max(child.1))
        }),
        _ => (0, depth),
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
    /// 音视频深链区间（无则 None）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<TimeSpan>,
    /// 渲染/取字节所需媒资引用（答案层据此内联展示；无则 None）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media: Option<MediaRef>,
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
    fn modality_derivation_and_serde() {
        assert_eq!(ChunkKind::Paragraph.modality(), Modality::Text);
        assert_eq!(ChunkKind::Table.modality(), Modality::Text);
        assert_eq!(ChunkKind::Image.modality(), Modality::Image);
        assert_eq!(ChunkKind::Audio.modality(), Modality::Audio);
        assert_eq!(ChunkKind::Video.modality(), Modality::Video);
        // serde snake_case，可作 Filter 字段值
        assert_eq!(
            serde_json::to_string(&Modality::Image).unwrap(),
            "\"image\""
        );
        assert_eq!(Modality::Audio.as_str(), "audio");
        // 新增 ChunkKind 变体 serde
        assert_eq!(
            serde_json::to_string(&ChunkKind::Video).unwrap(),
            "\"video\""
        );
        let k: ChunkKind = serde_json::from_str("\"audio\"").unwrap();
        assert_eq!(k, ChunkKind::Audio);
    }

    #[test]
    fn media_ref_serde_roundtrip() {
        // 各 AssetPointer 变体 + 时间区间 + region
        let m = MediaRef {
            asset: AssetPointer::Object {
                uri: "s3://bucket/clip.mp4".into(),
            },
            media_type: Some("video/mp4".into()),
            time: Some(TimeSpan {
                start_ms: 1000,
                end_ms: 4500,
            }),
            region: None,
            caption_source: Some("asr".into()),
            thumbnail: Some(AssetPointer::Inline),
        };
        let j = serde_json::to_string(&m).unwrap();
        assert_eq!(serde_json::from_str::<MediaRef>(&j).unwrap(), m);
        // tag 形式：AssetPointer 用 internally tagged "kind"
        let inline: AssetPointer = serde_json::from_str(r#"{"kind":"inline"}"#).unwrap();
        assert_eq!(inline, AssetPointer::Inline);
        let region: AssetPointer = serde_json::from_str(
            r#"{"kind":"doc_region","page":3,"bbox":{"x0":0.0,"y0":0.0,"x1":1.0,"y1":1.0}}"#,
        )
        .unwrap();
        assert!(matches!(region, AssetPointer::DocRegion { page: 3, .. }));
    }

    #[test]
    fn citation_time_media_default_and_roundtrip() {
        // 旧 Citation JSON（无 time/media）应能解析（serde default）
        let old = r#"{"collection":"kb","doc_id":"d","chunk_id":1,"page":1,
            "bbox":{"x0":0.0,"y0":0.0,"x1":1.0,"y1":1.0}}"#;
        let c: Citation = serde_json::from_str(old).unwrap();
        assert!(c.time.is_none() && c.media.is_none());
        // 带 time 的回环
        let c2 = Citation {
            time: Some(TimeSpan {
                start_ms: 5,
                end_ms: 9,
            }),
            ..c.clone()
        };
        let j = serde_json::to_string(&c2).unwrap();
        assert_eq!(serde_json::from_str::<Citation>(&j).unwrap(), c2);
    }

    #[test]
    fn chunk_media_defaults_none() {
        // 旧 Chunk JSON（无 media）解析 → media None（additive 向后兼容）
        let json = r#"{"doc_id":"a","chunk_id":1,"kind":"audio","text":"转录文本",
            "page":1,"bbox":{"x0":0.0,"y0":0.0,"x1":1.0,"y1":1.0},"char_len":4}"#;
        let c: Chunk = serde_json::from_str(json).unwrap();
        assert_eq!(c.kind, ChunkKind::Audio);
        assert!(c.media.is_none());
    }

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
            media: None,
            media_bytes: None,
            image_vector_status: None,
            tenant: None,
            acl: default_acl(),
            metadata: Metadata::new(),
            searchable: true,
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

    #[test]
    fn chunk_metadata_and_searchable_have_compatible_defaults() {
        let json = r#"{"doc_id":"a.pdf","chunk_id":1,"kind":"paragraph","text":"hi",
            "page":1,"bbox":{"x0":0.0,"y0":0.0,"x1":1.0,"y1":1.0},"char_len":2}"#;
        let c: Chunk = serde_json::from_str(json).unwrap();
        assert!(c.metadata.is_empty());
        assert!(c.searchable);
        assert!(c.validate_metadata().is_ok());
    }

    #[test]
    fn chunk_metadata_limits_are_enforced() {
        let mut c: Chunk = serde_json::from_str(
            r#"{"doc_id":"a.pdf","chunk_id":1,"kind":"paragraph","text":"hi",
            "page":1,"bbox":{"x0":0.0,"y0":0.0,"x1":1.0,"y1":1.0},"char_len":2}"#,
        )
        .unwrap();
        c.metadata.insert(
            "large".into(),
            Value::String("x".repeat(MAX_METADATA_BYTES)),
        );
        assert!(c.validate_metadata().is_err());

        c.metadata.clear();
        let mut nested = Value::Null;
        for _ in 0..=MAX_METADATA_DEPTH {
            nested = Value::Array(vec![nested]);
        }
        c.metadata.insert("nested".into(), nested);
        assert!(c.validate_metadata().is_err());
    }
}
