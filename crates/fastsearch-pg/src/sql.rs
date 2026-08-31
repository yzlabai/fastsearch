//! 纯 SQL 生成 + Chunk↔行映射（无 PG 依赖，可单测）。

use crate::error::{PgError, Result};
use fastsearch_core::{
    AclFilter, BBox, Chunk, ChunkKind, FieldValue, Filter, GlobalId, Signal, SignalStatus,
    SignalType,
};
use std::str::FromStr;

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

    /// HNSW cosine opclass。**必须与列类型一致**：halfvec 列建 hnsw 用 `halfvec_cosine_ops`，
    /// 否则 `CREATE INDEX` 报错、直查退化为顺序扫描（M17）。
    fn cosine_opclass(self) -> &'static str {
        match self {
            VectorType::Vector => "vector_cosine_ops",
            VectorType::HalfVec => "halfvec_cosine_ops",
        }
    }
}

/// 逻辑复制 publication 名（固定）。该 publication 永远专属 chunks 主表；
/// `chunk_signal` 等任何旁表不得加入，如需 CDC 必须新建 publication。
pub const PUBLICATION: &str = "fastsearch_pub";

/// Serialize every writer that can establish or replace one global document coordinate.
///
/// `GlobalId` and the chunks primary key deliberately omit tenant, so all PG write paths must
/// acquire this exact lock before checking the existing owner. Keeping the SQL in one place makes
/// it difficult for a new write path to accidentally use a weaker tenant-scoped lock.
pub(crate) fn lock_document_coordinate_sql() -> &'static str {
    "SELECT pg_advisory_xact_lock(hashtextextended($1::text || E'\\x1f' || $2::text, 0))"
}

/// Additive DDL for the ingestion work ledger. It is intentionally a normal table: no
/// extension, trigger, queue extension, or logical-replication publication is involved.
pub(crate) fn job_ddl(table: &str) -> Vec<String> {
    let state_constraint = format!("{table}_state_check");
    let retry_constraint = format!("{table}_retry_check");
    vec![
        format!(
            "CREATE TABLE IF NOT EXISTS {table} (\n\
             job_id text PRIMARY KEY,\n\
             collection text NOT NULL,\n\
             doc_id text NOT NULL,\n\
             tenant text,\n\
             acl text[] NOT NULL,\n\
             source_uri text NOT NULL,\n\
             content_sha256 text NOT NULL,\n\
             content_bytes bigint NOT NULL,\n\
             media_type text,\n\
             filename text,\n\
             parse_profile jsonb NOT NULL DEFAULT '{{}}'::jsonb,\n\
             state text NOT NULL DEFAULT 'queued',\n\
             stage_detail jsonb NOT NULL DEFAULT '{{}}'::jsonb,\n\
             chunk_count integer NOT NULL DEFAULT 0,\n\
             lease_owner text,\n\
             lease_epoch bigint NOT NULL DEFAULT 0,\n\
             lease_until timestamptz,\n\
             heartbeat_at timestamptz,\n\
             retry_count integer NOT NULL DEFAULT 0,\n\
             max_retries integer NOT NULL DEFAULT 3,\n\
             next_attempt_at timestamptz NOT NULL DEFAULT now(),\n\
             error text,\n\
             error_stage text,\n\
             created_at timestamptz NOT NULL DEFAULT now(),\n\
             updated_at timestamptz NOT NULL DEFAULT now(),\n\
             started_at timestamptz,\n\
             finished_at timestamptz,\n\
             CONSTRAINT {state_constraint} CHECK (state IN ('queued','parsing','chunking','embedding','indexed','failed')),\n\
             CONSTRAINT {retry_constraint} CHECK (retry_count >= 0 AND max_retries > 0 AND content_bytes >= 0)\n\
             );"
        ),
        // Additive upgrades. Fields that were present in the initial design are included too so
        // deployments created from an early prototype converge without destructive rewrites.
        format!("ALTER TABLE {table} ADD COLUMN IF NOT EXISTS collection text NOT NULL DEFAULT '';"),
        format!("ALTER TABLE {table} ALTER COLUMN collection DROP DEFAULT;"),
        format!("ALTER TABLE {table} ADD COLUMN IF NOT EXISTS doc_id text NOT NULL DEFAULT '';"),
        format!("ALTER TABLE {table} ALTER COLUMN doc_id DROP DEFAULT;"),
        format!("ALTER TABLE {table} ADD COLUMN IF NOT EXISTS tenant text;"),
        format!("ALTER TABLE {table} ADD COLUMN IF NOT EXISTS acl text[] NOT NULL DEFAULT '{{}}';"),
        format!("ALTER TABLE {table} ALTER COLUMN acl DROP DEFAULT;"),
        format!("ALTER TABLE {table} ADD COLUMN IF NOT EXISTS source_uri text NOT NULL DEFAULT '';"),
        format!("ALTER TABLE {table} ALTER COLUMN source_uri DROP DEFAULT;"),
        format!("ALTER TABLE {table} ADD COLUMN IF NOT EXISTS content_sha256 text NOT NULL DEFAULT '';"),
        format!("ALTER TABLE {table} ALTER COLUMN content_sha256 DROP DEFAULT;"),
        format!("ALTER TABLE {table} ADD COLUMN IF NOT EXISTS content_bytes bigint NOT NULL DEFAULT 0;"),
        format!("ALTER TABLE {table} ADD COLUMN IF NOT EXISTS media_type text;"),
        format!("ALTER TABLE {table} ADD COLUMN IF NOT EXISTS filename text;"),
        format!("ALTER TABLE {table} ADD COLUMN IF NOT EXISTS parse_profile jsonb NOT NULL DEFAULT '{{}}'::jsonb;"),
        format!("ALTER TABLE {table} ADD COLUMN IF NOT EXISTS state text NOT NULL DEFAULT 'queued';"),
        format!("ALTER TABLE {table} ADD COLUMN IF NOT EXISTS stage_detail jsonb NOT NULL DEFAULT '{{}}'::jsonb;"),
        format!("ALTER TABLE {table} ADD COLUMN IF NOT EXISTS chunk_count integer NOT NULL DEFAULT 0;"),
        format!("ALTER TABLE {table} ADD COLUMN IF NOT EXISTS lease_owner text;"),
        format!("ALTER TABLE {table} ADD COLUMN IF NOT EXISTS lease_epoch bigint NOT NULL DEFAULT 0;"),
        format!("ALTER TABLE {table} ADD COLUMN IF NOT EXISTS lease_until timestamptz;"),
        format!("ALTER TABLE {table} ADD COLUMN IF NOT EXISTS heartbeat_at timestamptz;"),
        format!("ALTER TABLE {table} ADD COLUMN IF NOT EXISTS retry_count integer NOT NULL DEFAULT 0;"),
        format!("ALTER TABLE {table} ADD COLUMN IF NOT EXISTS max_retries integer NOT NULL DEFAULT 3;"),
        format!("ALTER TABLE {table} ADD COLUMN IF NOT EXISTS next_attempt_at timestamptz NOT NULL DEFAULT now();"),
        format!("ALTER TABLE {table} ADD COLUMN IF NOT EXISTS error text;"),
        format!("ALTER TABLE {table} ADD COLUMN IF NOT EXISTS error_stage text;"),
        format!("ALTER TABLE {table} ADD COLUMN IF NOT EXISTS created_at timestamptz NOT NULL DEFAULT now();"),
        format!("ALTER TABLE {table} ADD COLUMN IF NOT EXISTS updated_at timestamptz NOT NULL DEFAULT now();"),
        format!("ALTER TABLE {table} ADD COLUMN IF NOT EXISTS started_at timestamptz;"),
        format!("ALTER TABLE {table} ADD COLUMN IF NOT EXISTS finished_at timestamptz;"),
        format!(
            "DO $$ BEGIN ALTER TABLE {table} ADD CONSTRAINT {state_constraint} \
             CHECK (state IN ('queued','parsing','chunking','embedding','indexed','failed')); \
             EXCEPTION WHEN duplicate_object THEN NULL; END $$;"
        ),
        format!(
            "DO $$ BEGIN ALTER TABLE {table} ADD CONSTRAINT {retry_constraint} \
             CHECK (retry_count >= 0 AND max_retries > 0 AND content_bytes >= 0); \
             EXCEPTION WHEN duplicate_object THEN NULL; END $$;"
        ),
        format!(
            "CREATE UNIQUE INDEX IF NOT EXISTS {table}_doc ON {table} \
             (COALESCE(tenant, ''), collection, doc_id);"
        ),
        // GlobalId/citation identity does not carry tenant. Keep the legacy tenant-scoped index
        // for additive compatibility, but make the actual document coordinate globally unique.
        // An upgrade with historical duplicates fails closed here and requires explicit cleanup.
        format!(
            "CREATE UNIQUE INDEX IF NOT EXISTS {table}_global_doc ON {table} \
             (collection, doc_id);"
        ),
        format!(
            "CREATE INDEX IF NOT EXISTS {table}_claim ON {table} (next_attempt_at, created_at) \
             WHERE state <> 'indexed';"
        ),
        format!(
            "CREATE INDEX IF NOT EXISTS {table}_list ON {table} \
             (COALESCE(tenant, ''), collection, doc_id);"
        ),
        format!(
            "CREATE INDEX IF NOT EXISTS {table}_hash ON {table} \
             (COALESCE(tenant, ''), collection, content_sha256);"
        ),
    ]
}

pub(crate) const JOB_RETURN_COLUMNS: &str = "job_id, collection, doc_id, tenant, acl, source_uri, \
content_sha256, content_bytes, media_type, filename, parse_profile::text AS parse_profile, state, \
stage_detail::text AS stage_detail, chunk_count, lease_owner, lease_epoch, \
(extract(epoch FROM lease_until) * 1000)::bigint AS lease_until_ms, \
(extract(epoch FROM heartbeat_at) * 1000)::bigint AS heartbeat_at_ms, retry_count, max_retries, \
(extract(epoch FROM next_attempt_at) * 1000)::bigint AS next_attempt_at_ms, error, error_stage, \
(extract(epoch FROM created_at) * 1000)::bigint AS created_at_ms, \
(extract(epoch FROM updated_at) * 1000)::bigint AS updated_at_ms, \
(extract(epoch FROM started_at) * 1000)::bigint AS started_at_ms, \
(extract(epoch FROM finished_at) * 1000)::bigint AS finished_at_ms";

pub(crate) fn enqueue_job_sql(table: &str) -> String {
    format!(
        "INSERT INTO {table} (job_id, collection, doc_id, tenant, acl, source_uri, \
         content_sha256, content_bytes, media_type, filename, parse_profile, max_retries) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11::text::jsonb,$12) \
         RETURNING {JOB_RETURN_COLUMNS}"
    )
}

pub(crate) fn get_job_sql(table: &str) -> String {
    format!("SELECT {JOB_RETURN_COLUMNS} FROM {table} WHERE job_id = $1")
}

pub(crate) fn lock_job_document_sql(table: &str) -> String {
    format!(
        "SELECT {JOB_RETURN_COLUMNS}, COALESCE(lease_until >= clock_timestamp(), false) AS lease_active FROM {table} \
         WHERE collection = $1 AND doc_id = $2 FOR UPDATE"
    )
}

pub(crate) fn reset_upload_job_sql(table: &str) -> String {
    format!(
        "UPDATE {table} SET acl = $2, source_uri = $3, content_sha256 = $4, \
         content_bytes = $5, media_type = $6, filename = $7, parse_profile = $8::text::jsonb, \
         max_retries = $9, state = 'queued', stage_detail = '{{}}'::jsonb, chunk_count = 0, \
         lease_owner = NULL, lease_until = NULL, heartbeat_at = NULL, retry_count = 0, \
         next_attempt_at = clock_timestamp(), error = NULL, error_stage = NULL, \
         updated_at = clock_timestamp(), started_at = NULL, finished_at = NULL \
         WHERE job_id = $1 RETURNING {JOB_RETURN_COLUMNS}"
    )
}

pub(crate) fn claim_jobs_sql(table: &str) -> String {
    format!(
        "WITH cand AS (\
           SELECT job_id FROM {table} \
            WHERE state <> 'indexed' \
              AND retry_count < max_retries \
              AND next_attempt_at <= clock_timestamp() \
              AND (state IN ('queued','failed') OR lease_until IS NULL \
                   OR lease_until < clock_timestamp()) \
            ORDER BY next_attempt_at, created_at, job_id \
            FOR UPDATE SKIP LOCKED LIMIT $1::bigint\
         ), claimed AS (\
         UPDATE {table} AS j \
            SET state = 'parsing', lease_owner = $2, lease_epoch = j.lease_epoch + 1, \
                lease_until = clock_timestamp() + make_interval(secs => $3::bigint::double precision / 1000.0), \
                heartbeat_at = clock_timestamp(), started_at = COALESCE(j.started_at, clock_timestamp()), \
                updated_at = clock_timestamp() \
           FROM cand WHERE j.job_id = cand.job_id RETURNING j.*\
         ) SELECT {JOB_RETURN_COLUMNS} FROM claimed"
    )
}

pub(crate) fn heartbeat_job_sql(table: &str) -> String {
    format!(
        "UPDATE {table} SET heartbeat_at = clock_timestamp(), \
         lease_until = clock_timestamp() + make_interval(secs => $4::bigint::double precision / 1000.0), \
         updated_at = clock_timestamp() \
         WHERE job_id = $1 AND lease_owner = $2 AND lease_epoch = $3 \
           AND state <> 'indexed' AND lease_until >= clock_timestamp() \
         RETURNING job_id"
    )
}

pub(crate) fn advance_job_sql(table: &str) -> String {
    format!(
        "UPDATE {table} SET state = $5, stage_detail = $6::text::jsonb, \
         heartbeat_at = clock_timestamp(), \
         lease_until = clock_timestamp() + make_interval(secs => $7::bigint::double precision / 1000.0), \
         updated_at = clock_timestamp() \
         WHERE job_id = $1 AND lease_owner = $2 AND lease_epoch = $3 AND state = $4 \
           AND lease_until >= clock_timestamp() RETURNING job_id"
    )
}

pub(crate) fn finish_job_sql(table: &str) -> String {
    format!(
        "UPDATE {table} SET state = 'indexed', chunk_count = $4, error = NULL, \
         error_stage = NULL, lease_owner = NULL, lease_until = NULL, finished_at = clock_timestamp(), \
         updated_at = clock_timestamp() \
         WHERE job_id = $1 AND lease_owner = $2 AND lease_epoch = $3 \
           AND state = 'embedding' AND lease_until >= clock_timestamp() RETURNING job_id"
    )
}

pub(crate) fn fail_job_sql(table: &str) -> String {
    format!(
        "UPDATE {table} SET state = 'failed', error = $4, error_stage = $5, \
         retry_count = retry_count + 1, next_attempt_at = to_timestamp($6::bigint::double precision / 1000.0), \
         lease_owner = NULL, lease_until = NULL, updated_at = clock_timestamp() \
         WHERE job_id = $1 AND lease_owner = $2 AND lease_epoch = $3 \
           AND state <> 'indexed' AND lease_until >= clock_timestamp() \
         RETURNING retry_count, max_retries"
    )
}

/// 幂等 DDL：扩展 + 表 + 索引 + publication。仅依赖 pgvector + 逻辑复制
/// （不需任何 `shared_preload_libraries` 原生扩展，保证托管 PG 可移植）。
pub fn ddl(table: &str, vector_type: VectorType, vector_dim: usize) -> Vec<String> {
    let mut statements = vec![
        "CREATE EXTENSION IF NOT EXISTS vector;".to_string(),
        format!(
            "CREATE TABLE IF NOT EXISTS {table} (\n\
             collection text NOT NULL,\n\
             doc_id text NOT NULL,\n\
             chunk_id bigint NOT NULL,\n\
             kind text NOT NULL,\n\
             text text NOT NULL,\n\
             metadata jsonb NOT NULL DEFAULT '{{}}'::jsonb,\n\
             searchable boolean NOT NULL DEFAULT true,\n\
             page integer NOT NULL,\n\
             bbox jsonb NOT NULL,\n\
             heading_path text[] NOT NULL DEFAULT '{{}}',\n\
             section_id bigint NOT NULL DEFAULT 0,\n\
             char_len integer NOT NULL,\n\
             modality text NOT NULL DEFAULT 'text',\n\
             media jsonb,\n\
             media_bytes bytea,\n\
             image_vector_status text,\n\
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
        format!(
            "ALTER TABLE {table} ADD COLUMN IF NOT EXISTS metadata jsonb NOT NULL DEFAULT '{{}}'::jsonb;"
        ),
        format!(
            "ALTER TABLE {table} ADD COLUMN IF NOT EXISTS searchable boolean NOT NULL DEFAULT true;"
        ),
        format!("CREATE INDEX IF NOT EXISTS {table}_doc ON {table} (collection, doc_id);"),
        // embedding HNSW ANN 索引（直查档，opclass 随列类型）——否则直查全程顺序扫描（M17）。
        // embedding 列全 NULL（非直查部署）时索引为空、几乎零代价；直查填充后即生效。
        format!("{};", ann_index_sql(table, vector_type)),
        // Publication 只发布**源真源列**（`COLUMNS`），刻意**排除引擎派生/记账列**
        // `embedding`/`embed_model`/`updated_at`——这样 B6 写穿（`set_embedding` UPDATE 这三列）的
        // 复制流里**不含派生列的值**，重解码不会看到它们、不据此重嵌。
        // **（实测更正，H3/R4）**：列清单只过滤"列的值"、**不抑制"Update 事件本身"**——只改被排除列的
        // UPDATE 仍会产生 Begin/Relation/Update/Commit（其中已发布列取当前值、或大列为 'u'）。故 CDC
        // 写穿反馈环**不是靠"不产生事件"断开**，而是靠 `set_embedding` 的 `IS DISTINCT FROM` 幂等守卫
        // 阻尼：值未变→0 行更新→第二轮无事件，环在一轮内收敛（不至无限）。该写穿事件对大 text 行正是
        // 'u'(UnchangedToast) 的载体 → 依赖 H3 的"遇 'u' 从真源重取整行"才不卡死。
        // 列清单发布需 **PG 15+**（核心特性、非扩展，不破"托管 PG 可移植"不变量 #1）。
        //
        // 幂等 + **自愈但不抢占**：① 无 publication → CREATE（带列清单）；② 本表已在该 publication
        // → ALTER 收敛到当前列清单（使 additive 源列能进入既有部署的 CDC）。
        // publication 属于别的表 → **不动**（避免并发实例互抢同名 publication）。
        // `CREATE` 包 `EXCEPTION` 防并发首建 TOCTOU（两连接同时见"无"→ 都建）：并发竞态在 PG 表现为
        // `unique_violation`(23505, pg_publication 唯一索引) 或 `duplicate_object`(42710)，两者都忽略。
        format!(
            "DO $$ BEGIN\n\
             IF NOT EXISTS (SELECT 1 FROM pg_publication WHERE pubname = '{PUBLICATION}') THEN\n\
             BEGIN\n\
             CREATE PUBLICATION {PUBLICATION} FOR TABLE {table} ({collist});\n\
             EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL;\n\
             END;\n\
             ELSIF EXISTS (\n\
             SELECT 1 FROM pg_publication_rel pr\n\
             JOIN pg_publication p ON p.oid = pr.prpubid\n\
             JOIN pg_class c ON c.oid = pr.prrelid\n\
             WHERE p.pubname = '{PUBLICATION}' AND c.relname = '{table}'\n\
             ) THEN\n\
             ALTER PUBLICATION {PUBLICATION} SET TABLE {table} ({collist});\n\
             END IF;\n\
             END $$;",
            collist = COLUMNS.join(", ")
        ),
    ];
    statements.extend(signal_ddl(&format!("{table}_signal")));
    statements
}

/// Additive source-of-truth table for named chunk representations. It deliberately has no
/// foreign key and no publication statement: document replacement temporarily deletes the
/// parent rows, while reconciliation in the same transaction preserves valid artifact signals.
pub fn signal_ddl(signal_table: &str) -> Vec<String> {
    vec![
        format!(
            "CREATE TABLE IF NOT EXISTS {signal_table} (\n\
             collection text NOT NULL,\n\
             doc_id text NOT NULL,\n\
             chunk_id bigint NOT NULL,\n\
             signal_type text NOT NULL,\n\
             status text NOT NULL DEFAULT 'pending',\n\
             model text,\n\
             model_version text,\n\
             artifact_hash text,\n\
             body_hash text,\n\
             signal_text text,\n\
             embedding real[],\n\
             embedding_dim integer,\n\
             error text,\n\
             updated_at timestamptz NOT NULL DEFAULT now(),\n\
             PRIMARY KEY (collection, doc_id, chunk_id, signal_type)\n\
             );"
        ),
        format!(
            "CREATE INDEX IF NOT EXISTS {signal_table}_doc ON {signal_table} (collection, doc_id);"
        ),
        format!(
            "CREATE INDEX IF NOT EXISTS {signal_table}_worklist ON {signal_table} (signal_type, status);"
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
                // Numeric filter values are represented as i64. Cast the placeholder explicitly
                // so PostgreSQL does not infer int4 from columns such as `page` and then reject the
                // tokio-postgres i64 binder. int4/int8 columns compare exactly against bigint.
                let ph = if is_text { ph } else { format!("{ph}::bigint") };
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
                let phs: Vec<String> = ps
                    .into_iter()
                    .map(|p| {
                        let ph = self.ph(p);
                        if is_text {
                            ph
                        } else {
                            format!("{ph}::bigint")
                        }
                    })
                    .collect();
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
    vector_type: VectorType,
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
    // 查询向量 cast 必须与 embedding 列类型一致（halfvec 列用 `::halfvec`，否则 `<=>` 类型不符）（M17）。
    let vtype = vector_type.sql();
    // heading_path/media 供不可翻译子句（HeadingPrefix）+ 时间后过滤（media.time 权威）；bbox/media 供组装 Citation。
    let sql = format!(
        "SELECT collection, doc_id, chunk_id, kind, modality, page, section_id, tenant, acl, \
         heading_path, bbox::text, media::text, 1 - (embedding <=> $1::text::{vtype}) AS score \
         FROM {table} WHERE {} \
         ORDER BY embedding <=> $1::text::{vtype} LIMIT {limit}",
        wheres.join(" AND ")
    );
    (sql, b.params)
}

/// Build an exact cosine-search query over ready `chunk_signal.embedding` rows. `$1` is the
/// query-vector text, `$2` its dimension and `$3` the allowed signal-type names. Each signal type
/// receives its own candidate window so one populous route cannot crowd the others out before
/// fusion. The `real[]` source-of-truth column intentionally has no ANN index in FS-202.
pub fn signal_vector_search_sql(
    signal_table: &str,
    chunks_table: &str,
    limit_per_signal: usize,
    acl: Option<&AclFilter>,
    filter: Option<&Filter>,
) -> (String, Vec<SqlParam>) {
    let mut b = WhereBuilder {
        params: Vec::new(),
        base: 4,
    };
    let mut wheres = Vec::new();
    if let Some(a) = acl {
        wheres.push(acl_clause(a, &mut b));
    }
    if let Some(f) = filter {
        wheres.push(b.build(f));
    }
    if wheres.is_empty() {
        wheres.push("TRUE".into());
    }
    let distance = "embedding::vector <=> $1::text::vector";
    let sql = format!(
        "WITH filtered AS (\
           SELECT c.collection, c.doc_id, c.chunk_id, c.kind, c.modality, c.page, \
                  c.section_id, c.time_start_ms, c.time_end_ms, c.tenant, c.acl, \
                  c.heading_path, c.bbox, c.media, \
                  s.signal_type, s.model, s.model_version, s.embedding \
           FROM {signal_table} AS s \
           JOIN {chunks_table} AS c \
             USING (collection, doc_id, chunk_id) \
           WHERE s.status = 'ready' AND s.embedding IS NOT NULL \
             AND s.embedding_dim = $2 AND s.signal_type = ANY($3::text[]) \
             AND c.searchable = TRUE\
         ), ranked AS (\
           SELECT collection, doc_id, chunk_id, kind, modality, page, section_id, tenant, acl, \
                  heading_path, bbox, media, signal_type, model, model_version, \
                  1 - ({distance}) AS score, \
                  row_number() OVER (PARTITION BY signal_type \
                    ORDER BY {distance}, collection, doc_id, chunk_id) AS signal_rank \
           FROM filtered WHERE {}\
         ) \
         SELECT collection, doc_id, chunk_id, kind, modality, page, section_id, tenant, acl, \
                heading_path, bbox::text, media::text, signal_type, model, model_version, score \
         FROM ranked WHERE signal_rank <= {limit_per_signal} \
         ORDER BY signal_type, score DESC, collection, doc_id, chunk_id",
        wheres.join(" AND ")
    );
    (sql, b.params)
}

/// embedding 上的 HNSW ANN 索引（cosine）——直查档需要；幂等。opclass 随列类型（halfvec 用
/// `halfvec_cosine_ops`）。由 `ddl` 建（否则直查全程顺序扫描）。
pub fn ann_index_sql(table: &str, vector_type: VectorType) -> String {
    format!(
        "CREATE INDEX IF NOT EXISTS {table}_emb_hnsw ON {table} \
         USING hnsw (embedding {opclass})",
        opclass = vector_type.cosine_opclass()
    )
}

/// 列顺序（写入 + 读取共用）。
/// Publication 列清单（发布到逻辑复制流的源列）。**刻意不含 `media_bytes`**（M16）：inline 媒资
/// 字节（可达 20MB/张）是 PG 真源、网关按需 `fetch_media_bytes` 直查，复制流只搬指针（`media` JSON）。
/// 若把 bytea 放进列清单，字节会整个走 WAL 逻辑解码进内存（积压时 OOM、带宽白放大），与"CDC 不搬
/// inline 字节"（MM2c-bytes §4.2）矛盾。派生索引本就不持字节（sync `row_to_chunk` 置 `media_bytes: None`）。
pub const COLUMNS: &[&str] = &[
    "collection",
    "doc_id",
    "chunk_id",
    "kind",
    "text",
    "metadata",
    "searchable",
    "page",
    "bbox",
    "heading_path",
    "section_id",
    "char_len",
    "modality",
    "media",
    "image_vector_status",
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
         (collection, doc_id, chunk_id, kind, text, metadata, searchable, page, bbox, heading_path, section_id, char_len, modality, media, media_bytes, image_vector_status, time_start_ms, time_end_ms, tenant, acl) \
         VALUES ($1, $2, $3, $4, $5, $6::text::jsonb, $7, $8, $9::text::jsonb, $10, $11, $12, $13, $14::text::jsonb, $15, $16, $17, $18, $19, $20)"
    )
}

/// Chunk 级幂等 upsert。冲突行只允许同 tenant 覆盖；更新任一真源字段时清空旧 embedding，
/// 后续由 CDC/embedder 重建，避免正文已变而向量仍旧。
pub fn upsert_chunk_sql(table: &str) -> String {
    format!(
        "INSERT INTO {table} AS target \
         (collection, doc_id, chunk_id, kind, text, metadata, searchable, page, bbox, heading_path, section_id, char_len, modality, media, media_bytes, image_vector_status, time_start_ms, time_end_ms, tenant, acl) \
         VALUES ($1, $2, $3, $4, $5, $6::text::jsonb, $7, $8, $9::text::jsonb, $10, $11, $12, $13, $14::text::jsonb, $15, $16, $17, $18, $19, $20) \
         ON CONFLICT (collection, doc_id, chunk_id) DO UPDATE SET \
         kind = EXCLUDED.kind, text = EXCLUDED.text, metadata = EXCLUDED.metadata, \
         searchable = EXCLUDED.searchable, page = EXCLUDED.page, bbox = EXCLUDED.bbox, \
         heading_path = EXCLUDED.heading_path, section_id = EXCLUDED.section_id, \
         char_len = EXCLUDED.char_len, modality = EXCLUDED.modality, media = EXCLUDED.media, \
         media_bytes = EXCLUDED.media_bytes, image_vector_status = EXCLUDED.image_vector_status, \
         time_start_ms = EXCLUDED.time_start_ms, time_end_ms = EXCLUDED.time_end_ms, \
         tenant = EXCLUDED.tenant, acl = EXCLUDED.acl, embedding = NULL, embed_model = NULL, \
         updated_at = now() \
         WHERE target.tenant IS NOT DISTINCT FROM EXCLUDED.tenant \
         RETURNING 1"
    )
}

fn artifact_hash_sql(chunk_alias: &str) -> String {
    format!("COALESCE(md5({chunk_alias}.media_bytes), md5(({chunk_alias}.media -> 'asset')::text))")
}

/// Insert or replace one named signal. Hashes and dimensions are always derived from the chunk
/// row inside Postgres, so direct library callers cannot introduce a second invalidation rule.
pub fn upsert_signal_sql(signal_table: &str, chunks_table: &str) -> String {
    let artifact_hash = artifact_hash_sql("c");
    format!(
        "INSERT INTO {signal_table} AS target \
         (collection, doc_id, chunk_id, signal_type, status, model, model_version, \
          artifact_hash, body_hash, signal_text, embedding, embedding_dim, error, updated_at) \
         SELECT $1, $2, $3, $4, $5, $6, $7, {artifact_hash}, md5(c.text), $8, \
                $9::real[], cardinality($9::real[]), $10, now() \
         FROM {chunks_table} AS c \
         WHERE c.collection = $1 AND c.doc_id = $2 AND c.chunk_id = $3 \
         ON CONFLICT (collection, doc_id, chunk_id, signal_type) DO UPDATE SET \
         status = EXCLUDED.status, model = EXCLUDED.model, model_version = EXCLUDED.model_version, \
         artifact_hash = EXCLUDED.artifact_hash, body_hash = EXCLUDED.body_hash, \
         signal_text = EXCLUDED.signal_text, embedding = EXCLUDED.embedding, \
         embedding_dim = EXCLUDED.embedding_dim, error = EXCLUDED.error, updated_at = now() \
         WHERE target.status IS DISTINCT FROM EXCLUDED.status \
            OR target.model IS DISTINCT FROM EXCLUDED.model \
            OR target.model_version IS DISTINCT FROM EXCLUDED.model_version \
            OR target.artifact_hash IS DISTINCT FROM EXCLUDED.artifact_hash \
            OR target.body_hash IS DISTINCT FROM EXCLUDED.body_hash \
            OR target.signal_text IS DISTINCT FROM EXCLUDED.signal_text \
            OR target.embedding IS DISTINCT FROM EXCLUDED.embedding \
            OR target.embedding_dim IS DISTINCT FROM EXCLUDED.embedding_dim \
            OR target.error IS DISTINCT FROM EXCLUDED.error \
         RETURNING 1"
    )
}

/// Write a signal embedding and provenance only when at least one durable value differs.
pub fn set_signal_embedding_sql(signal_table: &str) -> String {
    format!(
        "UPDATE {signal_table} SET embedding = $5::real[], embedding_dim = cardinality($5::real[]), \
         status = 'ready', model = $6, model_version = $7, error = NULL, updated_at = now() \
         WHERE collection = $1 AND doc_id = $2 AND chunk_id = $3 AND signal_type = $4 \
         AND (embedding IS DISTINCT FROM $5::real[] OR status IS DISTINCT FROM 'ready' \
              OR model IS DISTINCT FROM $6 OR model_version IS DISTINCT FROM $7 OR error IS NOT NULL)"
    )
}

/// Mark body-bound signals stale after a chunk write. Signal type values are supplied by the
/// exhaustive Rust enum rules rather than duplicated as literals in SQL.
pub fn stale_body_bound_signals_sql(signal_table: &str, chunks_table: &str) -> String {
    format!(
        "UPDATE {signal_table} AS s SET status = 'stale', embedding = NULL, \
         embedding_dim = NULL, updated_at = now() FROM {chunks_table} AS c \
         WHERE c.collection = s.collection AND c.doc_id = s.doc_id AND c.chunk_id = s.chunk_id \
         AND s.collection = $1 AND s.doc_id = $2 AND s.signal_type = ANY($3::text[]) \
         AND s.body_hash IS DISTINCT FROM md5(c.text) \
         AND (s.status <> 'stale' OR s.embedding IS NOT NULL)"
    )
}

/// Mark artifact-bound signals stale after a chunk media write.
pub fn stale_artifact_bound_signals_sql(signal_table: &str, chunks_table: &str) -> String {
    let artifact_hash = artifact_hash_sql("c");
    format!(
        "UPDATE {signal_table} AS s SET status = 'stale', embedding = NULL, \
         embedding_dim = NULL, updated_at = now() FROM {chunks_table} AS c \
         WHERE c.collection = s.collection AND c.doc_id = s.doc_id AND c.chunk_id = s.chunk_id \
         AND s.collection = $1 AND s.doc_id = $2 AND s.signal_type = ANY($3::text[]) \
         AND s.artifact_hash IS DISTINCT FROM {artifact_hash} \
         AND (s.status <> 'stale' OR s.embedding IS NOT NULL)"
    )
}

pub fn fetch_signals_sql(signal_table: &str) -> String {
    format!(
        "SELECT collection, doc_id, chunk_id, signal_type, status, model, model_version, \
         artifact_hash, body_hash, signal_text, embedding, embedding_dim, error \
         FROM {signal_table} WHERE collection = $1 AND doc_id = $2 AND chunk_id = $3 \
         ORDER BY signal_type"
    )
}

pub fn reconcile_doc_signals_sql(signal_table: &str) -> String {
    format!(
        "DELETE FROM {signal_table} WHERE collection = $1 AND doc_id = $2 \
         AND chunk_id <> ALL($3::bigint[])"
    )
}

pub fn delete_doc_signals_sql(signal_table: &str) -> String {
    format!("DELETE FROM {signal_table} WHERE collection = $1 AND doc_id = $2")
}

pub fn delete_chunk_signals_sql(signal_table: &str) -> String {
    format!("DELETE FROM {signal_table} WHERE collection = $1 AND doc_id = $2 AND chunk_id = $3")
}

pub fn orphan_signals_sql(signal_table: &str, chunks_table: &str) -> String {
    format!(
        "SELECT s.collection, s.doc_id, s.chunk_id, s.signal_type FROM {signal_table} AS s \
         LEFT JOIN {chunks_table} AS c USING (collection, doc_id, chunk_id) \
         WHERE c.collection IS NULL ORDER BY s.collection, s.doc_id, s.chunk_id, s.signal_type"
    )
}

/// 批量主键读取；LEFT JOIN + ordinality 保持请求顺序并显式保留缺失项。
pub fn batch_get_sql(table: &str) -> String {
    format!(
        "WITH requested(collection, doc_id, chunk_id, ordinality) AS ( \
           SELECT * FROM unnest($1::text[], $2::text[], $3::bigint[]) WITH ORDINALITY \
         ) \
         SELECT requested.ordinality, c.collection, c.doc_id, c.chunk_id, c.kind, c.text, \
         c.metadata::text AS metadata, c.searchable, c.page, c.bbox::text AS bbox, \
         c.heading_path, c.section_id, c.char_len, c.modality, c.media::text AS media, \
         c.media_bytes, c.image_vector_status, c.time_start_ms, c.time_end_ms, c.tenant, c.acl \
         FROM requested LEFT JOIN {table} AS c \
         ON c.collection = requested.collection AND c.doc_id = requested.doc_id \
         AND c.chunk_id = requested.chunk_id ORDER BY requested.ordinality"
    )
}

/// 单 chunk 删除，ACL 在 SQL 内原子检查。未授权与不存在都不返回行。
pub fn delete_chunk_visible_sql(table: &str, tenant_scoped: bool) -> String {
    let acl = if tenant_scoped {
        "tenant = $4 AND ('public' = ANY(acl) OR acl && $5)"
    } else {
        "('public' = ANY(acl) OR acl && $4)"
    };
    format!(
        "DELETE FROM {table} WHERE collection = $1 AND doc_id = $2 AND chunk_id = $3 \
         AND {acl} RETURNING 1"
    )
}

/// 文档内按 chunk_id 游标分页；ACL 在 SQL 内过滤，游标只跨越调用方可见行。
pub fn list_doc_chunks_sql(table: &str, tenant_scoped: bool) -> String {
    let (acl, limit) = if tenant_scoped {
        ("tenant = $4 AND ('public' = ANY(acl) OR acl && $5)", "$6")
    } else {
        ("('public' = ANY(acl) OR acl && $4)", "$5")
    };
    format!(
        "SELECT collection, doc_id, chunk_id, kind, text, metadata::text, searchable, page, \
         bbox::text, heading_path, section_id, char_len, modality, media::text, media_bytes, \
         image_vector_status, time_start_ms, time_end_ms, tenant, acl FROM {table} \
         WHERE collection = $1 AND doc_id = $2 AND chunk_id > $3 AND {acl} \
         ORDER BY chunk_id LIMIT {limit}"
    )
}

/// 按 collection owner 删除真源行并返回派生索引/对象清理所需信息。
pub fn delete_collection_sql(table: &str, tenant_scoped: bool) -> String {
    let owner = if tenant_scoped {
        " AND tenant = $2"
    } else {
        ""
    };
    format!(
        "DELETE FROM {table} WHERE collection = $1{owner} \
         RETURNING collection, doc_id, chunk_id, media::text AS media"
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

pub(crate) fn lock_doc_ownership_sql(table: &str) -> String {
    format!("SELECT tenant FROM {table} WHERE collection = $1 AND doc_id = $2 FOR UPDATE")
}

/// 读取某 doc 全部 chunk（jsonb 列读成文本）。
pub fn fetch_doc_sql(table: &str) -> String {
    format!(
        "SELECT collection, doc_id, chunk_id, kind, text, metadata::text, searchable, page, bbox::text, heading_path, \
         section_id, char_len, modality, media::text, media_bytes, image_vector_status, time_start_ms, time_end_ms, tenant, acl \
         FROM {table} WHERE collection = $1 AND doc_id = $2 ORDER BY chunk_id"
    )
}

/// 按主键读取单个 chunk（CDC 遇 UnchangedToast 不完整 WAL 时重取真源用，见 fastsearch-sync）。
pub fn fetch_chunk_sql(table: &str) -> String {
    format!(
        "SELECT collection, doc_id, chunk_id, kind, text, metadata::text, searchable, page, bbox::text, heading_path, \
         section_id, char_len, modality, media::text, media_bytes, image_vector_status, time_start_ms, time_end_ms, tenant, acl \
         FROM {table} WHERE collection = $1 AND doc_id = $2 AND chunk_id = $3"
    )
}

/// 全表读取（初始快照 bootstrap 用），按 (collection, doc_id, chunk_id) 升序、确定性。
pub fn fetch_all_sql(table: &str) -> String {
    format!(
        "SELECT collection, doc_id, chunk_id, kind, text, metadata::text, searchable, page, bbox::text, heading_path, \
         section_id, char_len, modality, media::text, media_bytes, image_vector_status, time_start_ms, time_end_ms, tenant, acl \
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

/// Owned database representation of one chunk signal.
#[derive(Debug, Clone, PartialEq)]
pub struct SignalRow {
    pub collection: String,
    pub doc_id: String,
    pub chunk_id: i64,
    pub signal_type: String,
    pub status: String,
    pub model: Option<String>,
    pub model_version: Option<String>,
    pub artifact_hash: Option<String>,
    pub body_hash: Option<String>,
    pub signal_text: Option<String>,
    pub embedding: Option<Vec<f32>>,
    pub embedding_dim: Option<i32>,
    pub error: Option<String>,
}

impl SignalRow {
    pub fn from_signal(signal: &Signal) -> Self {
        Self {
            collection: signal.gid.collection.clone(),
            doc_id: signal.gid.doc_id.clone(),
            chunk_id: signal.gid.chunk_id as i64,
            signal_type: signal.signal_type.as_str().to_string(),
            status: signal.status.as_str().to_string(),
            model: signal.model.clone(),
            model_version: signal.model_version.clone(),
            artifact_hash: signal.artifact_hash.clone(),
            body_hash: signal.body_hash.clone(),
            signal_text: signal.signal_text.clone(),
            embedding: signal.embedding.clone(),
            embedding_dim: signal.embedding_dim.map(|dim| dim as i32),
            error: signal.error.clone(),
        }
    }

    pub fn to_signal(&self) -> Result<Signal> {
        Ok(Signal {
            gid: GlobalId {
                collection: self.collection.clone(),
                doc_id: self.doc_id.clone(),
                chunk_id: self.chunk_id as u64,
            },
            signal_type: SignalType::from_str(&self.signal_type)?,
            status: SignalStatus::from_str(&self.status)?,
            model: self.model.clone(),
            model_version: self.model_version.clone(),
            artifact_hash: self.artifact_hash.clone(),
            body_hash: self.body_hash.clone(),
            signal_text: self.signal_text.clone(),
            embedding: self.embedding.clone(),
            embedding_dim: self.embedding_dim.map(|dim| dim as u32),
            error: self.error.clone(),
        })
    }
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
    /// 调用方透传 metadata JSON 对象。
    pub metadata: String,
    /// 是否进入全文/向量派生索引。
    pub searchable: bool,
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
    /// 图片视觉向量状态（PG 真源）。
    pub image_vector_status: Option<String>,
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
            metadata: serde_json::to_string(&c.metadata)?,
            searchable: c.searchable,
            page: c.page as i32,
            bbox: serde_json::to_string(&c.bbox)?,
            heading_path: c.heading_path.clone(),
            section_id: c.section_id as i64,
            char_len: c.char_len as i32,
            modality: c.kind.modality().as_str().to_string(),
            media: c.media.as_ref().map(serde_json::to_string).transpose()?,
            media_bytes: c.media_bytes.clone(),
            image_vector_status: c.image_vector_status.map(|s| s.as_str().to_string()),
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
        let metadata = serde_json::from_str(&self.metadata)?;
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
            image_vector_status: self.image_vector_status(),
            tenant: self.tenant.clone(),
            acl: self.acl.clone(),
            metadata,
            searchable: self.searchable,
        })
    }

    fn image_vector_status(&self) -> Option<fastsearch_core::ImageVectorStatus> {
        self.image_vector_status.as_deref().and_then(|s| {
            serde_json::from_value::<fastsearch_core::ImageVectorStatus>(serde_json::Value::String(
                s.to_string(),
            ))
            .ok()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastsearch_core::{BBox, GlobalId, Signal, SignalStatus, SignalType};

    #[test]
    fn job_schema_is_one_portable_non_publication_table() {
        let statements = job_ddl("fastsearch_ingest_jobs");
        let ddl = statements.join("\n");
        assert_eq!(ddl.matches("CREATE TABLE").count(), 1);
        for required in [
            "job_id text PRIMARY KEY",
            "tenant text",
            "acl text[] NOT NULL",
            "lease_owner text",
            "lease_epoch bigint NOT NULL DEFAULT 0",
            "retry_count integer NOT NULL DEFAULT 0",
            "CHECK (state IN ('queued','parsing','chunking','embedding','indexed','failed'))",
            "fastsearch_ingest_jobs_doc",
            "fastsearch_ingest_jobs_global_doc",
            "fastsearch_ingest_jobs_claim",
            "fastsearch_ingest_jobs_list",
            "fastsearch_ingest_jobs_hash",
        ] {
            assert!(ddl.contains(required), "missing {required:?} in {ddl}");
        }
        let upper = ddl.to_ascii_uppercase();
        assert!(!upper.contains("PUBLICATION"));
        assert!(!upper.contains("CREATE EXTENSION"));
        for forbidden_column in [
            "owner",
            "role",
            "member",
            "group",
            "parent",
            "inherit",
            "permission",
            "dataset",
            "version",
            "revision",
        ] {
            assert!(
                !ddl.lines().any(|line| {
                    line.trim_start()
                        .starts_with(&format!("{forbidden_column} "))
                }),
                "forbidden identity/product column {forbidden_column:?} in {ddl}"
            );
        }
    }

    #[test]
    fn document_coordinate_lock_is_global_and_shared_by_all_writers() {
        let sql = lock_document_coordinate_sql();
        assert!(sql.contains("pg_advisory_xact_lock"));
        assert!(sql.contains("hashtextextended"));
        assert!(sql.contains("$1::text"));
        assert!(sql.contains("$2::text"));
        assert!(!sql.contains("tenant"));
    }

    #[test]
    fn job_claim_and_mutations_are_fenced() {
        let claim = claim_jobs_sql("jobs");
        assert!(claim.contains("FOR UPDATE SKIP LOCKED"));
        assert!(claim.contains("retry_count < max_retries"));
        assert!(claim.contains("lease_epoch = j.lease_epoch + 1"));
        assert!(claim.contains("lease_until < clock_timestamp()"));

        for sql in [
            heartbeat_job_sql("jobs"),
            advance_job_sql("jobs"),
            finish_job_sql("jobs"),
            fail_job_sql("jobs"),
        ] {
            assert!(
                sql.contains("lease_owner = $2"),
                "missing owner fence: {sql}"
            );
            assert!(
                sql.contains("lease_epoch = $3"),
                "missing epoch fence: {sql}"
            );
        }
        let advance = advance_job_sql("jobs");
        assert!(advance.contains("state = $4"));
        assert!(advance.contains("state = $5"));
    }

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
            image_vector_status: None,
            tenant: Some("acme".into()),
            acl: vec!["team-a".into(), "public".into()],
            metadata: serde_json::from_value(serde_json::json!({
                "source": "fixture",
                "ordinal": 3
            }))
            .unwrap(),
            searchable: true,
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
        assert!(joined.contains("metadata jsonb NOT NULL DEFAULT '{}'::jsonb"));
        assert!(joined.contains("searchable boolean NOT NULL DEFAULT true"));
        assert!(joined.contains("ALTER TABLE fastsearch_chunks ADD COLUMN IF NOT EXISTS metadata"));
        assert!(joined.contains("time_start_ms bigint"));
        assert!(joined.contains("time_end_ms bigint"));
        // Publication 用列清单（PG15+）、发布源列、**排除派生列**（断 CDC 写穿反馈环）。
        assert!(joined.contains(
            "CREATE PUBLICATION fastsearch_pub FOR TABLE fastsearch_chunks (collection, doc_id,"
        ));
        assert!(joined.contains("ALTER PUBLICATION fastsearch_pub SET TABLE fastsearch_chunks ("));
        // 派生/记账列不得出现在 publication 列清单里。
        let pub_line = joined
            .lines()
            .find(|l| l.contains("CREATE PUBLICATION"))
            .unwrap();
        assert!(!pub_line.contains("embedding"), "embedding 不应被发布");
        assert!(!pub_line.contains("embed_model"), "embed_model 不应被发布");
        assert!(!pub_line.contains("updated_at"), "updated_at 不应被发布");
        // 但源列要在。
        assert!(pub_line.contains("text") && pub_line.contains("acl"));
    }

    #[test]
    fn signal_schema_and_mutations_preserve_single_table_publication() {
        let joined = signal_ddl("fastsearch_chunks_signal").join("\n");
        assert!(joined.contains("CREATE TABLE IF NOT EXISTS fastsearch_chunks_signal"));
        assert!(joined.contains("PRIMARY KEY (collection, doc_id, chunk_id, signal_type)"));
        assert!(joined.contains("embedding real[]"));
        assert!(joined.contains("fastsearch_chunks_signal_doc"));
        assert!(joined.contains("fastsearch_chunks_signal_worklist"));
        assert!(!joined.contains("CREATE EXTENSION"));
        assert!(!joined.contains("FOREIGN KEY"));
        assert!(!joined.contains("ALTER PUBLICATION"));

        let all = ddl("fastsearch_chunks", VectorType::HalfVec, 384).join("\n");
        assert!(all.contains("CREATE TABLE IF NOT EXISTS fastsearch_chunks_signal"));
        let publication = all
            .lines()
            .filter(|line| line.contains("PUBLICATION"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!publication.contains("fastsearch_chunks_signal"));

        let stale_body = stale_body_bound_signals_sql("sig", "chunks");
        assert!(stale_body.contains("signal_type = ANY($3::text[])"));
        assert!(stale_body.contains("body_hash IS DISTINCT FROM md5(c.text)"));
        assert!(stale_body.contains("s.status <> 'stale' OR s.embedding IS NOT NULL"));

        let stale_artifact = stale_artifact_bound_signals_sql("sig", "chunks");
        assert!(stale_artifact.contains("COALESCE(md5(c.media_bytes)"));
        assert!(stale_artifact.contains("c.media -> 'asset'"));
        assert!(stale_artifact.contains("artifact_hash IS DISTINCT FROM"));

        let set_embedding = set_signal_embedding_sql("sig");
        assert!(set_embedding.contains("embedding IS DISTINCT FROM $5"));
        assert!(set_embedding.contains("status IS DISTINCT FROM 'ready'"));

        let upsert = upsert_signal_sql("sig", "chunks");
        assert!(upsert.contains("ON CONFLICT (collection, doc_id, chunk_id, signal_type)"));
        assert!(upsert.contains("IS DISTINCT FROM EXCLUDED"));
    }

    #[test]
    fn signal_row_roundtrips_optional_vector_and_error() {
        let signal = Signal {
            gid: GlobalId {
                collection: "kb".into(),
                doc_id: "dir:sub:report.pdf".into(),
                chunk_id: 42,
            },
            signal_type: SignalType::Ocr,
            status: SignalStatus::Failed,
            model: Some("ovis-ocr2".into()),
            model_version: Some("v2".into()),
            artifact_hash: Some("artifact-hash".into()),
            body_hash: Some("body-hash".into()),
            signal_text: Some("中文 OCR".into()),
            embedding: None,
            embedding_dim: None,
            error: Some("decode failed".into()),
        };
        let row = SignalRow::from_signal(&signal);
        assert_eq!(row.to_signal().unwrap(), signal);
        assert!(SignalRow {
            signal_type: "unknown".into(),
            ..row
        }
        .to_signal()
        .is_err());
    }

    #[test]
    fn insert_and_delete_sql_shape() {
        let ins = insert_sql("t");
        assert!(ins.contains("$19"));
        assert!(ins.contains("$6::text::jsonb")); // metadata
        assert!(ins.contains("$9::text::jsonb")); // bbox（先 ::text 再 ::jsonb，见 insert_sql 注释）
        assert!(ins.contains("$14::text::jsonb")); // media
        assert!(ins.contains(
            "modality, media, media_bytes, image_vector_status, time_start_ms, time_end_ms"
        )); // 新列（MM2c + image vector status）
        assert!(!ins.contains("image_meta")); // 遗留列已移除
        assert!(ins.contains("$20"));
        assert!(!ins.contains("$21")); // exactly 20 params
        let del = delete_doc_sql("t");
        assert_eq!(del, "DELETE FROM t WHERE collection = $1 AND doc_id = $2");
    }

    #[test]
    fn management_sql_preserves_identity_acl_and_pagination() {
        let upsert = upsert_chunk_sql("t");
        assert!(upsert.contains("ON CONFLICT (collection, doc_id, chunk_id)"));
        assert!(upsert.contains("target.tenant IS NOT DISTINCT FROM EXCLUDED.tenant"));
        assert!(upsert.contains("embedding = NULL"));

        let get = batch_get_sql("t");
        assert!(get.contains("WITH ORDINALITY"));
        assert!(get.contains("LEFT JOIN t AS c"));
        assert!(get.contains("ORDER BY requested.ordinality"));

        let delete = delete_chunk_visible_sql("t", true);
        assert!(delete.contains("tenant = $4"));
        assert!(delete.contains("acl && $5"));
        let delete_admin = delete_chunk_visible_sql("t", false);
        assert!(!delete_admin.contains("tenant ="));
        assert!(delete_admin.contains("acl && $4"));

        let list = list_doc_chunks_sql("t", true);
        assert!(list.contains("chunk_id > $3"));
        assert!(list.contains("ORDER BY chunk_id LIMIT $6"));

        let collection = delete_collection_sql("t", true);
        assert!(collection.contains("collection = $1 AND tenant = $2"));
        assert!(collection.contains("RETURNING collection, doc_id, chunk_id"));
    }

    #[test]
    fn chunkrow_roundtrip() {
        let c = sample();
        let row = ChunkRow::from_chunk("kb", &c).unwrap();
        assert_eq!(row.collection, "kb");
        assert_eq!(row.chunk_id, 152);
        assert_eq!(row.kind, "table");
        assert_eq!(row.heading_path, vec!["第3章", "财务"]);
        assert!(row.metadata.contains("\"source\":\"fixture\""));
        assert!(row.searchable);
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
        let (sql, params) =
            pgvector_search_sql("t", VectorType::Vector, 80, Some(&acl), Some(&filter));
        // 查询向量是 $1；ACL 先入参（$2 tenant, $3 tags），filter 后（$4 modality, $5 page）。
        assert!(sql.contains("embedding <=> $1::text::vector"));
        assert!(sql.contains("tenant = $2"));
        assert!(sql.contains("'public' = ANY(acl) OR acl && $3::text[]"));
        assert!(sql.contains("modality = $4"));
        assert!(sql.contains("page >= $5::bigint"));
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
        let (sql, params) = pgvector_search_sql("t", VectorType::Vector, 10, None, Some(&f));
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
        let (sql, params) = pgvector_search_sql("t", VectorType::Vector, 5, None, Some(&f));
        assert!(sql.contains("kind IN ($2, $3)"));
        assert_eq!(params.len(), 2);
        // 无过滤 + 无 ACL：仅 embedding 守门。
        let (sql2, p2) = pgvector_search_sql("t", VectorType::Vector, 5, None, None);
        assert!(sql2.contains("WHERE embedding IS NOT NULL ORDER BY"));
        assert!(p2.is_empty());
        assert!(ann_index_sql("t", VectorType::Vector)
            .contains("USING hnsw (embedding vector_cosine_ops)"));
    }

    #[test]
    fn halfvec_opclass_and_cast_and_ddl() {
        // M17：halfvec 列（默认档）的 ANN 索引 opclass 与查询 cast 必须是 halfvec 版本，否则建索引
        // 报错/直查退化为顺序扫描。且 ddl 应把 ANN 索引建出来（此前 ann_index_sql 从不被调用）。
        assert!(ann_index_sql("t", VectorType::HalfVec).contains("halfvec_cosine_ops"));
        let (sql, _) = pgvector_search_sql("t", VectorType::HalfVec, 5, None, None);
        assert!(
            sql.contains("$1::text::halfvec"),
            "halfvec 列查询 cast 应为 ::halfvec: {sql}"
        );
        let ddl_stmts = ddl("c", VectorType::HalfVec, 384).join("\n");
        assert!(
            ddl_stmts.contains("halfvec_cosine_ops"),
            "默认 ddl 应建 halfvec opclass 的 ANN 索引"
        );
    }

    #[test]
    fn pgvector_sql_time_range_pushdown() {
        // 时间区间过滤（音视频）→ 精确 SUPERSET 下推到 SQL（MM2c）：与 modality/page 同构。
        let f = Filter::And(vec![
            Filter::Gte("time_start_ms".into(), FieldValue::Int(1000)),
            Filter::Lte("time_end_ms".into(), FieldValue::Int(9000)),
        ]);
        let (sql, params) = pgvector_search_sql("t", VectorType::Vector, 20, None, Some(&f));
        // 超集守 #5：时间列下推须 `OR col IS NULL`（列 NULL 而 media.time 有值的遗留行不被排除，
        // 交后过滤读权威 media.time 精确判定）。
        assert!(sql.contains("(time_start_ms >= $2::bigint OR time_start_ms IS NULL)"));
        assert!(sql.contains("(time_end_ms <= $3::bigint OR time_end_ms IS NULL)"));
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
