//! 纯 SQL 生成 + Chunk↔行映射（无 PG 依赖，可单测）。

use crate::error::{PgError, Result};
use fastsearch_core::{AclFilter, BBox, Chunk, ChunkKind, FieldValue, Filter};

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
             modality text NOT NULL DEFAULT 'text',\n\
             media jsonb,\n\
             media_bytes bytea,\n\
             time_start_ms bigint,\n\
             time_end_ms bigint,\n\
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

// ============================ B6: pgvector 直查档 SQL 生成（纯函数，可单测） ============================
//
// 把 `AclFilter`+`Filter` 翻译成**精确 SQL WHERE**（可翻译子句）或 `TRUE`（不可翻译→SUPERSET，
// 由调用方 Rust 侧 `Filter::eval`/`AclFilter::visible` 精确后过滤），守不变量 #5。详见
// [B6 设计](../../docs/plans/2026-06-26-B6-pgvector直查档设计.md)。

/// 绑定到 SQL 的参数（按出现顺序；调用方据类型 bind 进 tokio-postgres）。
#[derive(Debug, Clone, PartialEq)]
pub enum SqlParam {
    Text(String),
    Int(i64),
    /// `text[]`（ACL 标签集）。
    TextArray(Vec<String>),
}

/// 可翻译列 → (列名, 是否 text 类型)。其余字段不可翻译（→ TRUE 超集）。
/// `time_start_ms`/`time_end_ms` 为 bigint 列（MM2c）：从 `media.time` 派生落列，
/// 与 `PgVecRow` 后过滤同源（后过滤亦取自 `media.time`）→ 时间区间可精确 SUPERSET 下推、守不变量 #5。
fn col_kind(field: &str) -> Option<(&'static str, bool)> {
    match field {
        "collection" => Some(("collection", true)),
        "doc_id" => Some(("doc_id", true)),
        "kind" => Some(("kind", true)),
        "modality" => Some(("modality", true)),
        "tenant" => Some(("tenant", true)),
        "page" => Some(("page", false)),
        "section_id" => Some(("section_id", false)),
        "time_start_ms" => Some(("time_start_ms", false)),
        "time_end_ms" => Some(("time_end_ms", false)),
        _ => None,
    }
}

/// 该列是否是"**权威源在别处**的可空反规范化列"——其值由写路径从权威字段派生落列，
/// 但权威源（如 `media.time`）才是后过滤读的真相。这类列下推须加 `OR col IS NULL` 保超集：
/// 否则列 NULL（遗留/外部写入行）而权威源有值时，下推会排除掉后过滤会保留的行（违反 #5）。
/// 目前仅 `time_start_ms`/`time_end_ms`（派生自 `media.time`，MM2c-time）。
fn nullable_denorm(col: &str) -> bool {
    matches!(col, "time_start_ms" | "time_end_ms")
}

/// 值与列类型匹配则返回对应 `SqlParam`，否则 None（→ 该叶子不可翻译，TRUE 超集）。
fn match_param(is_text_col: bool, v: &FieldValue) -> Option<SqlParam> {
    match (is_text_col, v) {
        (true, FieldValue::Str(s)) => Some(SqlParam::Text(s.clone())),
        (false, FieldValue::Int(i)) => Some(SqlParam::Int(*i)),
        _ => None, // 类型不匹配（如对 int 列传字符串）→ 不翻译
    }
}

struct WhereBuilder {
    params: Vec<SqlParam>,
    base: usize, // 首个参数占位符编号（$1 留给查询向量 → base=2）
}

impl WhereBuilder {
    fn ph(&mut self, p: SqlParam) -> String {
        self.params.push(p);
        format!("${}", self.base + self.params.len() - 1)
    }

    /// 叶子比较 `col OP $n`；不可翻译（列未知/类型不符/文本比较/否定）→ "TRUE"（超集）。
    fn cmp(&mut self, field: &str, op: &str, v: &FieldValue) -> String {
        let Some((col, is_text)) = col_kind(field) else {
            return "TRUE".into();
        };
        // 大小比较仅对数值列（文本字典序受 collation 影响，交给 Rust 后过滤）。
        if matches!(op, "<" | "<=" | ">" | ">=") && is_text {
            return "TRUE".into();
        }
        match match_param(is_text, v) {
            Some(p) => {
                let ph = self.ph(p);
                if nullable_denorm(col) {
                    // 超集：列 NULL 行也放行，交后过滤（读权威 media.time）精确判定，守 #5。
                    format!("({col} {op} {ph} OR {col} IS NULL)")
                } else {
                    format!("{col} {op} {ph}")
                }
            }
            None => "TRUE".into(),
        }
    }

    fn build(&mut self, f: &Filter) -> String {
        match f {
            Filter::And(fs) => self.join(fs, "AND", "TRUE"),
            Filter::Or(fs) => self.join(fs, "OR", "FALSE"),
            // 否定的精确 SQL 在可空列上有 NULL 补集坑 → 一律 TRUE 超集，Rust 后过滤兜精确。
            Filter::Not(_) | Filter::Ne(_, _) => "TRUE".into(),
            Filter::Eq(k, v) => self.cmp(k, "=", v),
            Filter::Gt(k, v) => self.cmp(k, ">", v),
            Filter::Gte(k, v) => self.cmp(k, ">=", v),
            Filter::Lt(k, v) => self.cmp(k, "<", v),
            Filter::Lte(k, v) => self.cmp(k, "<=", v),
            Filter::In(k, vs) => self.in_clause(k, vs),
            Filter::Exists(k) => match col_kind(k) {
                Some(("tenant", _)) => "tenant IS NOT NULL".into(),
                Some(_) => "TRUE".into(), // 其余列 NOT NULL → 恒存在
                None => "TRUE".into(),
            },
            // heading_path 前缀：数组前缀匹配不便精确下推 → TRUE 超集，Rust 后过滤兜。
            Filter::HeadingPrefix(_) => "TRUE".into(),
        }
    }

    fn join(&mut self, fs: &[Filter], op: &str, empty: &str) -> String {
        if fs.is_empty() {
            return empty.into();
        }
        let parts: Vec<String> = fs.iter().map(|f| self.build(f)).collect();
        format!("({})", parts.join(&format!(" {op} ")))
    }

    fn in_clause(&mut self, field: &str, vs: &[FieldValue]) -> String {
        let Some((col, is_text)) = col_kind(field) else {
            return "TRUE".into();
        };
        // 全部值类型匹配才翻译；否则 TRUE 超集。
        let params: Option<Vec<SqlParam>> = vs.iter().map(|v| match_param(is_text, v)).collect();
        match params {
            Some(ps) if !ps.is_empty() => {
                let phs: Vec<String> = ps.into_iter().map(|p| self.ph(p)).collect();
                let inner = format!("{col} IN ({})", phs.join(", "));
                if nullable_denorm(col) {
                    format!("({inner} OR {col} IS NULL)")
                } else {
                    inner
                }
            }
            _ => "TRUE".into(), // 空 In 或类型不符
        }
    }
}

/// ACL → 精确 SQL（tenant 严格隔离 + public/标签相交）。无 tenant 限制（管理员）→ 仅标签维度。
fn acl_clause(acl: &AclFilter, b: &mut WhereBuilder) -> String {
    let mut clauses = Vec::new();
    if let Some(t) = &acl.tenant {
        let ph = b.ph(SqlParam::Text(t.clone()));
        clauses.push(format!("tenant = {ph}")); // 行 tenant 必须等于调用者（NULL→排除，严格）
    }
    // public 公开 或 acl 与授权标签相交。
    let tags = b.ph(SqlParam::TextArray(acl.allowed_tags.clone()));
    clauses.push(format!("('public' = ANY(acl) OR acl && {tags}::text[])"));
    format!("({})", clauses.join(" AND "))
}

/// 构造 pgvector 直查 SELECT：`$1` 为查询向量（调用方 bind），filter/acl 参数从 `$2` 起。
/// 返回 (SQL, params)。SUPERSET WHERE + 调用方 over-fetch + Rust 精确后过滤（守 #5）。
pub fn pgvector_search_sql(
    table: &str,
    limit: usize,
    acl: Option<&AclFilter>,
    filter: Option<&Filter>,
) -> (String, Vec<SqlParam>) {
    let mut b = WhereBuilder {
        params: Vec::new(),
        base: 2,
    };
    let mut wheres = vec!["embedding IS NOT NULL".to_string()];
    if let Some(a) = acl {
        wheres.push(acl_clause(a, &mut b));
    }
    if let Some(f) = filter {
        wheres.push(b.build(f));
    }
    // heading_path/media 供不可翻译子句（HeadingPrefix）+ 时间后过滤（media.time 权威）；bbox/media 供组装 Citation。
    let sql = format!(
        "SELECT collection, doc_id, chunk_id, kind, modality, page, section_id, tenant, acl, \
         heading_path, bbox::text, media::text, 1 - (embedding <=> $1::text::vector) AS score \
         FROM {table} WHERE {} \
         ORDER BY embedding <=> $1::text::vector LIMIT {limit}",
        wheres.join(" AND ")
    );
    (sql, b.params)
}

/// embedding 上的 HNSW ANN 索引（cosine）——直查档需要；幂等。
pub fn ann_index_sql(table: &str) -> String {
    format!(
        "CREATE INDEX IF NOT EXISTS {table}_emb_hnsw ON {table} \
         USING hnsw (embedding vector_cosine_ops)"
    )
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
    "modality",
    "media",
    "media_bytes",
    "time_start_ms",
    "time_end_ms",
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
         (collection, doc_id, chunk_id, kind, text, page, bbox, heading_path, section_id, char_len, modality, media, media_bytes, time_start_ms, time_end_ms, tenant, acl) \
         VALUES ($1, $2, $3, $4, $5, $6, $7::text::jsonb, $8, $9, $10, $11, $12::text::jsonb, $13, $14, $15, $16, $17)"
    )
}

/// 按主键取 inline 媒资字节（媒资网关 `/v1/asset` 的 Inline 路径，MM6-inline 用）。
/// 返回单列 `media_bytes`（可空 bytea）。字节是 PG 真源、引擎派生层不持有 → 按需直查。
pub fn fetch_media_bytes_sql(table: &str) -> String {
    format!(
        "SELECT media_bytes FROM {table} \
         WHERE collection = $1 AND doc_id = $2 AND chunk_id = $3"
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
         section_id, char_len, modality, media::text, media_bytes, time_start_ms, time_end_ms, tenant, acl \
         FROM {table} WHERE collection = $1 AND doc_id = $2 ORDER BY chunk_id"
    )
}

/// 全表读取（初始快照 bootstrap 用），按 (collection, doc_id, chunk_id) 升序、确定性。
pub fn fetch_all_sql(table: &str) -> String {
    format!(
        "SELECT collection, doc_id, chunk_id, kind, text, page, bbox::text, heading_path, \
         section_id, char_len, modality, media::text, media_bytes, time_start_ms, time_end_ms, tenant, acl \
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
/// jsonb 列以文本承载（`bbox`/`media`）。
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
    /// 模态（由 kind 派生，落列供 SQL 侧过滤）。
    pub modality: String,
    /// 媒资引用 JSON（`MediaRef`，不含 inline 字节）。
    pub media: Option<String>,
    /// inline 媒资字节（`bytea`，`AssetPointer::Inline` 时有值；PG 真源，MM2c-bytes）。
    pub media_bytes: Option<Vec<u8>>,
    /// 时间区间（毫秒）：从 `media.time` 派生落列（MM2c），供 SQL 侧 SUPERSET 下推/排序。
    /// 读路径 `to_chunk` 的 `time` 仍由 `media` 恢复（这两列是写侧反规范化，非权威源）。
    pub time_start_ms: Option<i64>,
    pub time_end_ms: Option<i64>,
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
            modality: c.kind.modality().as_str().to_string(),
            media: c.media.as_ref().map(serde_json::to_string).transpose()?,
            media_bytes: c.media_bytes.clone(),
            // 时间区间从 media.time 派生落列（与后过滤同源 → 下推/后过滤一致）。
            time_start_ms: c
                .media
                .as_ref()
                .and_then(|m| m.time)
                .map(|t| t.start_ms as i64),
            time_end_ms: c
                .media
                .as_ref()
                .and_then(|m| m.time)
                .map(|t| t.end_ms as i64),
            tenant: c.tenant.clone(),
            acl: c.acl.clone(),
        })
    }

    pub fn to_chunk(&self) -> Result<Chunk> {
        let bbox: BBox = serde_json::from_str(&self.bbox)?;
        let media = match &self.media {
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
            media, // 媒资从 media jsonb 列恢复（modality 在 Chunk 侧由 kind 派生）
            media_bytes: self.media_bytes.clone(),
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
            media: None,
            media_bytes: None,
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
        assert!(joined.contains("modality text NOT NULL DEFAULT 'text'"));
        assert!(joined.contains("media jsonb"));
        assert!(joined.contains("media_bytes bytea"));
        assert!(joined.contains("time_start_ms bigint"));
        assert!(joined.contains("time_end_ms bigint"));
        assert!(joined.contains("CREATE PUBLICATION fastsearch_pub FOR TABLE fastsearch_chunks"));
    }

    #[test]
    fn insert_and_delete_sql_shape() {
        let ins = insert_sql("t");
        assert!(ins.contains("$17"));
        assert!(ins.contains("$7::text::jsonb")); // bbox（先 ::text 再 ::jsonb，见 insert_sql 注释）
        assert!(ins.contains("$12::text::jsonb")); // media
        assert!(ins.contains("modality, media, media_bytes, time_start_ms, time_end_ms")); // 新列（MM2c）
        assert!(!ins.contains("image_meta")); // 遗留列已移除
        assert!(!ins.contains("$18")); // exactly 17 params
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
        // modality 由 kind 派生落列（Table 属文本模态）
        assert_eq!(row.modality, "text");
        assert!(row.media.is_none());
    }

    #[test]
    fn chunkrow_media_roundtrip() {
        use fastsearch_core::{AssetPointer, MediaRef, TimeSpan};
        let mut c = sample();
        c.kind = ChunkKind::Audio;
        c.media_bytes = Some(vec![0xDE, 0xAD, 0xBE, 0xEF]); // inline 字节往返
        c.media = Some(MediaRef {
            asset: AssetPointer::Object {
                uri: "s3://b/clip.mp3".into(),
            },
            media_type: Some("audio/mpeg".into()),
            time: Some(TimeSpan {
                start_ms: 1000,
                end_ms: 5000,
            }),
            region: None,
            caption_source: Some("asr".into()),
            thumbnail: None,
        });
        let row = ChunkRow::from_chunk("kb", &c).unwrap();
        assert_eq!(row.modality, "audio"); // 由 kind 派生
        assert!(row.media.is_some());
        // 时间区间从 media.time 派生落列（MM2c），供 SQL 侧下推。
        assert_eq!(row.time_start_ms, Some(1000));
        assert_eq!(row.time_end_ms, Some(5000));
        assert_eq!(row.media_bytes, Some(vec![0xDE, 0xAD, 0xBE, 0xEF]));
        let back = row.to_chunk().unwrap();
        assert_eq!(back, c); // media 往返一致（time 由 media 恢复，与列同源）
    }

    #[test]
    fn chunkrow_handles_media_and_empty_heading() {
        use fastsearch_core::{AssetPointer, MediaRef};
        let mut c = sample();
        c.heading_path = vec![];
        c.kind = ChunkKind::Image;
        c.media = Some(MediaRef {
            asset: AssetPointer::DocRegion {
                page: 23,
                bbox: c.bbox,
            },
            media_type: Some("image/png".into()),
            time: None,
            region: Some(c.bbox),
            caption_source: Some("图1".into()),
            thumbnail: None,
        });
        let row = ChunkRow::from_chunk("kb", &c).unwrap();
        assert_eq!(row.modality, "image");
        assert!(row.media.as_ref().unwrap().contains("doc_region"));
        assert_eq!(row.to_chunk().unwrap(), c);
    }

    #[test]
    fn pgvector_sql_acl_and_filter_pushdown() {
        let acl = AclFilter {
            tenant: Some("acme".into()),
            allowed_tags: vec!["team-a".into()],
        };
        let filter = Filter::And(vec![
            Filter::Eq("modality".into(), FieldValue::Str("image".into())),
            Filter::Gte("page".into(), FieldValue::Int(5)),
        ]);
        let (sql, params) = pgvector_search_sql("t", 80, Some(&acl), Some(&filter));
        // 查询向量是 $1；ACL 先入参（$2 tenant, $3 tags），filter 后（$4 modality, $5 page）。
        assert!(sql.contains("embedding <=> $1::text::vector"));
        assert!(sql.contains("tenant = $2"));
        assert!(sql.contains("'public' = ANY(acl) OR acl && $3::text[]"));
        assert!(sql.contains("modality = $4"));
        assert!(sql.contains("page >= $5"));
        assert!(sql.contains("LIMIT 80"));
        assert_eq!(
            params,
            vec![
                SqlParam::Text("acme".into()),
                SqlParam::TextArray(vec!["team-a".into()]),
                SqlParam::Text("image".into()),
                SqlParam::Int(5),
            ]
        );
    }

    #[test]
    fn pgvector_sql_untranslatable_is_superset_true() {
        // 不可翻译：未知字段 / 否定 / 类型不符 / 文本大小比较 → TRUE（超集，Rust 后过滤兜）。
        let f = Filter::And(vec![
            Filter::Eq("weird_field".into(), FieldValue::Str("x".into())), // 未知列
            Filter::Ne("kind".into(), FieldValue::Str("image".into())),    // 否定
            Filter::Eq("page".into(), FieldValue::Str("oops".into())),     // 类型不符
            Filter::Gt("kind".into(), FieldValue::Str("a".into())),        // 文本大小比较
        ]);
        let (sql, params) = pgvector_search_sql("t", 10, None, Some(&f));
        assert!(params.is_empty(), "全不可翻译 → 无参数");
        // 子句全为 TRUE：AND(TRUE,TRUE,TRUE,TRUE)
        assert!(sql.contains("(TRUE AND TRUE AND TRUE AND TRUE)"));
        // 无 ACL/无可翻译过滤仍至少 embedding IS NOT NULL 守门。
        assert!(sql.contains("embedding IS NOT NULL"));
    }

    #[test]
    fn pgvector_sql_in_and_no_filter() {
        let f = Filter::In(
            "kind".into(),
            vec![
                FieldValue::Str("table".into()),
                FieldValue::Str("image".into()),
            ],
        );
        let (sql, params) = pgvector_search_sql("t", 5, None, Some(&f));
        assert!(sql.contains("kind IN ($2, $3)"));
        assert_eq!(params.len(), 2);
        // 无过滤 + 无 ACL：仅 embedding 守门。
        let (sql2, p2) = pgvector_search_sql("t", 5, None, None);
        assert!(sql2.contains("WHERE embedding IS NOT NULL ORDER BY"));
        assert!(p2.is_empty());
        assert!(ann_index_sql("t").contains("USING hnsw (embedding vector_cosine_ops)"));
    }

    #[test]
    fn pgvector_sql_time_range_pushdown() {
        // 时间区间过滤（音视频）→ 精确 SUPERSET 下推到 SQL（MM2c）：与 modality/page 同构。
        let f = Filter::And(vec![
            Filter::Gte("time_start_ms".into(), FieldValue::Int(1000)),
            Filter::Lte("time_end_ms".into(), FieldValue::Int(9000)),
        ]);
        let (sql, params) = pgvector_search_sql("t", 20, None, Some(&f));
        // 超集守 #5：时间列下推须 `OR col IS NULL`（列 NULL 而 media.time 有值的遗留行不被排除，
        // 交后过滤读权威 media.time 精确判定）。
        assert!(sql.contains("(time_start_ms >= $2 OR time_start_ms IS NULL)"));
        assert!(sql.contains("(time_end_ms <= $3 OR time_end_ms IS NULL)"));
        assert_eq!(
            params,
            vec![SqlParam::Int(1000), SqlParam::Int(9000)],
            "时间为 bigint 列 → Int 参数下推"
        );
    }

    #[test]
    fn bad_kind_errors() {
        assert!(kind_from_str("nonsense").is_err());
        assert_eq!(kind_to_str(ChunkKind::ListItem), "list_item");
    }
}
