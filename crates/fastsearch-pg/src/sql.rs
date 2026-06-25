//! 纯 SQL 生成 + Chunk↔行映射（无 PG 依赖，可单测）。

use crate::error::{PgError, Result};
use fastsearch_core::{BBox, Chunk, ChunkKind, ImageMeta};

/// 向量列类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VectorType {
    Vector,
    HalfVec,
}

impl VectorType {
    fn sql(self) -> &'static str {
        match self {
            VectorType::Vector => "vector",
            VectorType::HalfVec => "halfvec",
        }
    }
}

/// 逻辑复制 publication 名（固定）。
pub const PUBLICATION: &str = "fastsearch_pub";

/// 幂等 DDL：扩展 + 表 + 索引 + publication。仅依赖 pgvector + 逻辑复制
/// （不需任何 `shared_preload_libraries` 原生扩展，保证托管 PG 可移植）。
pub fn ddl(table: &str, vector_type: VectorType, vector_dim: usize) -> Vec<String> {
    vec![
        "CREATE EXTENSION IF NOT EXISTS vector;".to_string(),
        format!(
            "CREATE TABLE IF NOT EXISTS {table} (\n\
             collection text NOT NULL,\n\
             doc_id text NOT NULL,\n\
             chunk_id bigint NOT NULL,\n\
             kind text NOT NULL,\n\
             text text NOT NULL,\n\
             page integer NOT NULL,\n\
             bbox jsonb NOT NULL,\n\
             heading_path text[] NOT NULL DEFAULT '{{}}',\n\
             section_id bigint NOT NULL DEFAULT 0,\n\
             char_len integer NOT NULL,\n\
             image_meta jsonb,\n\
             tenant text,\n\
             acl text[] NOT NULL DEFAULT '{{public}}',\n\
             embedding {vectype}({dim}),\n\
             embed_model text,\n\
             updated_at timestamptz NOT NULL DEFAULT now(),\n\
             PRIMARY KEY (collection, doc_id, chunk_id)\n\
             );",
            vectype = vector_type.sql(),
            dim = vector_dim
        ),
        format!("CREATE INDEX IF NOT EXISTS {table}_doc ON {table} (collection, doc_id);"),
        format!(
            "DO $$ BEGIN\n\
             IF NOT EXISTS (SELECT 1 FROM pg_publication WHERE pubname = '{PUBLICATION}') THEN\n\
             CREATE PUBLICATION {PUBLICATION} FOR TABLE {table};\n\
             END IF;\n\
             END $$;"
        ),
    ]
}

/// 列顺序（写入 + 读取共用）。
pub const COLUMNS: &[&str] = &[
    "collection",
    "doc_id",
    "chunk_id",
    "kind",
    "text",
    "page",
    "bbox",
    "heading_path",
    "section_id",
    "char_len",
    "image_meta",
    "tenant",
    "acl",
];

/// 参数化 INSERT；jsonb 列以文本传参 + `::text::jsonb` 转换（免依赖 serde_json 的
/// tokio-postgres ToSql 特性）。**必须先 `::text` 再 `::jsonb`**：否则 PG 会把参数类型
/// 推断为 jsonb，tokio-postgres 拒收 String（WrongType）；`$7::text` 强制参数推断为 text，
/// 运行时再 text→jsonb。
pub fn insert_sql(table: &str) -> String {
    format!(
        "INSERT INTO {table} \
         (collection, doc_id, chunk_id, kind, text, page, bbox, heading_path, section_id, char_len, image_meta, tenant, acl) \
         VALUES ($1, $2, $3, $4, $5, $6, $7::text::jsonb, $8, $9, $10, $11::text::jsonb, $12, $13)"
    )
}

/// doc_id 级删除（替换的第一步）。
pub fn delete_doc_sql(table: &str) -> String {
    format!("DELETE FROM {table} WHERE collection = $1 AND doc_id = $2")
}

/// 读取某 doc 全部 chunk（jsonb 列读成文本）。
pub fn fetch_doc_sql(table: &str) -> String {
    format!(
        "SELECT collection, doc_id, chunk_id, kind, text, page, bbox::text, heading_path, \
         section_id, char_len, image_meta::text, tenant, acl \
         FROM {table} WHERE collection = $1 AND doc_id = $2 ORDER BY chunk_id"
    )
}

/// 全表读取（初始快照 bootstrap 用），按 (collection, doc_id, chunk_id) 升序、确定性。
pub fn fetch_all_sql(table: &str) -> String {
    format!(
        "SELECT collection, doc_id, chunk_id, kind, text, page, bbox::text, heading_path, \
         section_id, char_len, image_meta::text, tenant, acl \
         FROM {table} ORDER BY collection, doc_id, chunk_id"
    )
}

fn kind_to_str(k: ChunkKind) -> String {
    // 复用 core 的 serde（snake_case）：序列化成裸字符串。
    match serde_json::to_value(k) {
        Ok(serde_json::Value::String(s)) => s,
        _ => "paragraph".to_string(),
    }
}

fn kind_from_str(s: &str) -> Result<ChunkKind> {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .map_err(|e| PgError::Mapping(format!("bad kind '{s}': {e}")))
}

/// 列值的拥有式视图：写入时按列借引用作参数，读取时从此构造 [`Chunk`]。
/// jsonb 列以文本承载（`bbox`/`image_meta`）。
#[derive(Debug, Clone, PartialEq)]
pub struct ChunkRow {
    pub collection: String,
    pub doc_id: String,
    pub chunk_id: i64,
    pub kind: String,
    pub text: String,
    pub page: i32,
    pub bbox: String,
    pub heading_path: Vec<String>,
    pub section_id: i64,
    pub char_len: i32,
    pub image_meta: Option<String>,
    pub tenant: Option<String>,
    pub acl: Vec<String>,
}

impl ChunkRow {
    pub fn from_chunk(collection: &str, c: &Chunk) -> Result<Self> {
        Ok(ChunkRow {
            collection: collection.to_string(),
            doc_id: c.doc_id.clone(),
            chunk_id: c.chunk_id as i64,
            kind: kind_to_str(c.kind),
            text: c.text.clone(),
            page: c.page as i32,
            bbox: serde_json::to_string(&c.bbox)?,
            heading_path: c.heading_path.clone(),
            section_id: c.section_id as i64,
            char_len: c.char_len as i32,
            image_meta: c
                .image_meta
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
            tenant: c.tenant.clone(),
            acl: c.acl.clone(),
        })
    }

    pub fn to_chunk(&self) -> Result<Chunk> {
        let bbox: BBox = serde_json::from_str(&self.bbox)?;
        let image_meta: Option<ImageMeta> = match &self.image_meta {
            Some(j) => Some(serde_json::from_str(j)?),
            None => None,
        };
        Ok(Chunk {
            doc_id: self.doc_id.clone(),
            chunk_id: self.chunk_id as u64,
            kind: kind_from_str(&self.kind)?,
            text: self.text.clone(),
            page: self.page as u32,
            bbox,
            heading_path: self.heading_path.clone(),
            section_id: self.section_id as u64,
            char_len: self.char_len as u32,
            image_meta,
            tenant: self.tenant.clone(),
            acl: self.acl.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastsearch_core::BBox;

    fn sample() -> Chunk {
        Chunk {
            doc_id: "dir:sub:report.pdf".into(),
            chunk_id: 152,
            kind: ChunkKind::Table,
            text: "本季度毛利率下降".into(),
            page: 23,
            bbox: BBox {
                x0: 1.0,
                y0: 2.0,
                x1: 3.0,
                y1: 4.0,
            },
            heading_path: vec!["第3章".into(), "财务".into()],
            section_id: 17,
            char_len: 8,
            image_meta: None,
            tenant: Some("acme".into()),
            acl: vec!["team-a".into(), "public".into()],
        }
    }

    #[test]
    fn ddl_has_extension_table_publication() {
        let stmts = ddl("fastsearch_chunks", VectorType::HalfVec, 384);
        let joined = stmts.join("\n");
        assert!(joined.contains("CREATE EXTENSION IF NOT EXISTS vector"));
        assert!(joined.contains("CREATE TABLE IF NOT EXISTS fastsearch_chunks"));
        assert!(joined.contains("PRIMARY KEY (collection, doc_id, chunk_id)"));
        assert!(joined.contains("halfvec(384)"));
        assert!(joined.contains("acl text[]"));
        assert!(joined.contains("CREATE PUBLICATION fastsearch_pub FOR TABLE fastsearch_chunks"));
    }

    #[test]
    fn insert_and_delete_sql_shape() {
        let ins = insert_sql("t");
        assert!(ins.contains("$13"));
        assert!(ins.contains("$7::text::jsonb")); // bbox（先 ::text 再 ::jsonb，见 insert_sql 注释）
        assert!(ins.contains("$11::text::jsonb")); // image_meta
        assert!(!ins.contains("$14")); // exactly 13 params
        let del = delete_doc_sql("t");
        assert_eq!(del, "DELETE FROM t WHERE collection = $1 AND doc_id = $2");
    }

    #[test]
    fn chunkrow_roundtrip() {
        let c = sample();
        let row = ChunkRow::from_chunk("kb", &c).unwrap();
        assert_eq!(row.collection, "kb");
        assert_eq!(row.chunk_id, 152);
        assert_eq!(row.kind, "table");
        assert_eq!(row.heading_path, vec!["第3章", "财务"]);
        let back = row.to_chunk().unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn chunkrow_handles_image_meta_and_empty_heading() {
        let mut c = sample();
        c.heading_path = vec![];
        c.image_meta = Some(ImageMeta {
            caption: Some("图1".into()),
            ..Default::default()
        });
        let row = ChunkRow::from_chunk("kb", &c).unwrap();
        assert!(row.image_meta.as_ref().unwrap().contains("图1"));
        assert_eq!(row.to_chunk().unwrap(), c);
    }

    #[test]
    fn bad_kind_errors() {
        assert!(kind_from_str("nonsense").is_err());
        assert_eq!(kind_to_str(ChunkKind::ListItem), "list_item");
    }
}
