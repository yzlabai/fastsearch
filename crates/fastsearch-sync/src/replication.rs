//! CDC 线缆层：从 Postgres 逻辑复制 slot 拉取 **pgoutput** 二进制变更 → 解码
//! （[`crate::pgoutput`]）→ 映射成 [`ChangeEvent`] → 交 [`Applier`](crate::Applier)
//! 应用到引擎索引。
//!
//! **传输**：用逻辑解码的 SQL 函数 `pg_logical_slot_get_binary_changes`（普通连接即可，
//! 无需复制协议连接），这是一种合法的轮询式 CDC 消费方式（消费即推进 slot，崩溃后
//! 从 slot 续传）。低延迟流式（`START_REPLICATION` COPY）为后续可选优化。
//!
//! **需活 PG**（逻辑复制 + pgvector + `wal_level=logical`），集成测试 env-gated（`DATABASE_URL`）。
//! 解码（pgoutput）与映射（行→Chunk，复用 [`fastsearch_pg::ChunkRow::to_chunk`]）是纯逻辑、
//! 与传输解耦——故即便无 PG，解码/数组解析也有单测覆盖。

use crate::pgoutput::{self, PgMessage, Relation, TupleData};
use crate::{Change, ChangeEvent, Lsn};
use anyhow::{Context, Result};
use fastsearch_core::{Chunk, GlobalId};
use fastsearch_pg::ChunkRow;
use std::collections::HashMap;
use tokio_postgres::{Client, NoTls};

/// 逻辑复制消费配置。
#[derive(Debug, Clone)]
pub struct ReplicationConfig {
    pub url: String,
    /// 逻辑复制 slot 名（pgoutput 插件）。
    pub slot: String,
    /// publication 名（应与 pg DDL 的 `fastsearch_pub` 一致）。
    pub publication: String,
}

/// 连接一个普通（非复制协议）客户端，后台驱动连接 future。
async fn connect(url: &str) -> Result<Client> {
    let (client, connection) = tokio_postgres::connect(url, NoTls)
        .await
        .context("connect")?;
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("fastsearch-sync replication connection error: {e}");
        }
    });
    Ok(client)
}

/// 幂等创建 pgoutput 逻辑复制 slot。**新建**时返回 `Some(consistent_point)`（一致点 LSN，
/// 初始快照与增量的衔接点）；**已存在**返回 `None`。WHERE 假时函数不求值（不建 slot）。
pub async fn ensure_slot(cfg: &ReplicationConfig) -> Result<Option<Lsn>> {
    let client = connect(&cfg.url).await?;
    let rows = client
        .query(
            "SELECT (pg_create_logical_replication_slot($1, 'pgoutput')).lsn::text AS lsn \
             WHERE NOT EXISTS (SELECT 1 FROM pg_replication_slots WHERE slot_name = $1)",
            &[&cfg.slot],
        )
        .await
        .context("create logical replication slot")?;
    match rows.first() {
        Some(r) => {
            let lsn_text: String = r.get(0);
            Ok(Some(Lsn(parse_pg_lsn(&lsn_text)?)))
        }
        None => Ok(None),
    }
}

/// 删除 slot（测试清理用；不存在则忽略）。
pub async fn drop_slot(cfg: &ReplicationConfig) -> Result<()> {
    let client = connect(&cfg.url).await?;
    client
        .execute(
            "SELECT pg_drop_replication_slot(slot_name) FROM pg_replication_slots \
             WHERE slot_name = $1",
            &[&cfg.slot],
        )
        .await
        .context("drop replication slot")?;
    Ok(())
}

/// 从 slot 拉取并**消费**（`get`，立即推进 slot）全部待处理变更。
///
/// ⚠️ **仅供"无持久化"的闭环演示/测试**：get 拉取即推进 slot，若"拉取后、派生索引落盘前"
/// 崩溃会丢这批变更。生产用 [`peek_changes`] + 落盘后 [`advance_slot`]（见
/// [持久化与崩溃安全计划](../../../docs/plans/2026-06-25-派生索引持久化与崩溃安全.md)）。
pub async fn pull_changes(cfg: &ReplicationConfig) -> Result<Vec<ChangeEvent>> {
    Ok(fetch_changes(cfg, "pg_logical_slot_get_binary_changes")
        .await?
        .0)
}

/// 从 slot **查看**（`peek`，**不推进** slot）全部待处理变更。落盘后须显式
/// [`advance_slot`] 才推进——这是崩溃安全的关键（不丢/不重）。
pub async fn peek_changes(cfg: &ReplicationConfig) -> Result<Vec<ChangeEvent>> {
    Ok(fetch_changes(cfg, "pg_logical_slot_peek_binary_changes")
        .await?
        .0)
}

/// 同 [`peek_changes`]，外加**所有行的最高 LSN**（含 Begin/Commit 等非数据消息）——
/// 这是 `advance_slot` 应推进到的位置（仅推到最后一条数据变更会漏掉更高的 Commit LSN，
/// 导致下次 peek 重复返回该事务）。
pub async fn peek_with_lsn(cfg: &ReplicationConfig) -> Result<(Vec<ChangeEvent>, Lsn)> {
    fetch_changes(cfg, "pg_logical_slot_peek_binary_changes").await
}

/// 显式推进 slot 的 confirmed_flush 到 `lsn`（持久化派生索引**之后**才调用）。
pub async fn advance_slot(cfg: &ReplicationConfig, lsn: Lsn) -> Result<()> {
    let client = connect(&cfg.url).await?;
    let lsn_text = format_pg_lsn(lsn.0);
    // `$2::text::pg_lsn`：先 ::text 强制参数推断为 text，运行时再转 pg_lsn——否则 PG 把
    // 参数推断为 pg_lsn，tokio-postgres 拒收 String（同 pg 层 jsonb 的坑）。
    client
        .execute(
            "SELECT pg_replication_slot_advance($1, $2::text::pg_lsn)",
            &[&cfg.slot, &lsn_text],
        )
        .await
        .context("pg_replication_slot_advance")?;
    Ok(())
}

/// 用逻辑解码函数 `func`（get 或 peek）取变更并解码 + 映射成有序 `ChangeEvent`。
/// `func` 为受控字面量（非用户输入）；slot/publication 单引号转义。
async fn fetch_changes(cfg: &ReplicationConfig, func: &str) -> Result<(Vec<ChangeEvent>, Lsn)> {
    let client = connect(&cfg.url).await?;
    let sql = format!(
        "SELECT lsn::text, data FROM {func}(\
         '{slot}', NULL, NULL, 'proto_version', '1', 'publication_names', '{pubn}')",
        slot = esc(&cfg.slot),
        pubn = esc(&cfg.publication),
    );
    let rows = client.query(&sql, &[]).await.context(func.to_string())?;

    let mut relations: HashMap<u32, Relation> = HashMap::new();
    let mut out = Vec::new();
    let mut max_lsn = Lsn(0);
    for row in &rows {
        let lsn_text: String = row.get(0);
        let data: Vec<u8> = row.get(1);
        let lsn = Lsn(parse_pg_lsn(&lsn_text)?);
        max_lsn = max_lsn.max(lsn); // 含 Begin/Commit 等非数据行：slot 推进位置
        let pg = pgoutput::parse_message(&data)?;
        if let Some(change) = map(&mut relations, pg)? {
            out.push(ChangeEvent { change, lsn });
        }
    }
    Ok((out, max_lsn))
}

/// pgoutput 消息 → 可选 Change。Relation 入缓存；Insert/Update→Upsert，Delete→Delete；
/// Begin/Commit/Origin/Type/Truncate → None（消化）。
fn map(relations: &mut HashMap<u32, Relation>, pg: PgMessage) -> Result<Option<Change>> {
    match pg {
        PgMessage::Relation(r) => {
            relations.insert(r.oid, r);
            Ok(None)
        }
        PgMessage::Insert { rel_oid, tuple }
        | PgMessage::Update {
            rel_oid,
            new: tuple,
            ..
        } => {
            let rel = relation(relations, rel_oid)?;
            let (collection, chunk) = row_to_chunk(rel, &tuple)?;
            Ok(Some(Change::Upsert {
                collection,
                chunk: Box::new(chunk),
            }))
        }
        PgMessage::Delete { rel_oid, key, old } => {
            let rel = relation(relations, rel_oid)?;
            let tuple = key
                .or(old)
                .context("Delete without key/old tuple (need REPLICA IDENTITY)")?;
            Ok(Some(Change::Delete {
                gid: row_to_gid(rel, &tuple)?,
            }))
        }
        _ => Ok(None),
    }
}

fn relation(relations: &HashMap<u32, Relation>, oid: u32) -> Result<&Relation> {
    relations
        .get(&oid)
        .with_context(|| format!("no Relation for oid {oid} (missing 'R' message)"))
}

/// 列名 → 文本值（Null/UnchangedToast → None）。
fn cols<'a>(rel: &'a Relation, tuple: &'a TupleData) -> HashMap<&'a str, Option<&'a str>> {
    rel.pair(tuple)
        .into_iter()
        .map(|(name, v)| (name, v.as_str()))
        .collect()
}

fn get<'a>(m: &HashMap<&'a str, Option<&'a str>>, k: &str) -> Result<&'a str> {
    m.get(k)
        .copied()
        .flatten()
        .with_context(|| format!("column '{k}' missing/null"))
}

/// fastsearch_chunks 行（pgoutput 文本元组）→ `(collection, Chunk)`，复用
/// [`ChunkRow::to_chunk`]（bbox/image_meta JSON、kind 解析、类型转换）。
fn row_to_chunk(rel: &Relation, tuple: &TupleData) -> Result<(String, Chunk)> {
    let m = cols(rel, tuple);
    let row = ChunkRow {
        collection: get(&m, "collection")?.to_string(),
        doc_id: get(&m, "doc_id")?.to_string(),
        chunk_id: get(&m, "chunk_id")?.parse().context("chunk_id")?,
        kind: get(&m, "kind")?.to_string(),
        text: get(&m, "text")?.to_string(),
        page: get(&m, "page")?.parse().context("page")?,
        bbox: get(&m, "bbox")?.to_string(),
        heading_path: parse_pg_array(get(&m, "heading_path")?),
        section_id: get(&m, "section_id")?.parse().context("section_id")?,
        char_len: get(&m, "char_len")?.parse().context("char_len")?,
        image_meta: m.get("image_meta").copied().flatten().map(String::from),
        tenant: m.get("tenant").copied().flatten().map(String::from),
        acl: parse_pg_array(get(&m, "acl")?),
    };
    let collection = row.collection.clone();
    let chunk = row.to_chunk().context("ChunkRow::to_chunk")?;
    Ok((collection, chunk))
}

/// Delete 键元组 → GlobalId（仅需 PK 列）。
fn row_to_gid(rel: &Relation, tuple: &TupleData) -> Result<GlobalId> {
    let m = cols(rel, tuple);
    Ok(GlobalId {
        collection: get(&m, "collection")?.to_string(),
        doc_id: get(&m, "doc_id")?.to_string(),
        chunk_id: get(&m, "chunk_id")?.parse().context("chunk_id")?,
    })
}

/// u64 → `pg_lsn` 文本形式 `X/Y`（高32位/低32位，大写十六进制）。
fn format_pg_lsn(v: u64) -> String {
    format!("{:X}/{:X}", v >> 32, v & 0xFFFF_FFFF)
}

/// 解析 `pg_lsn` 文本形式 `X/Y`（高32位/低32位，十六进制）→ u64。
fn parse_pg_lsn(s: &str) -> Result<u64> {
    let (hi, lo) = s.split_once('/').context("bad pg_lsn (no '/')")?;
    let hi = u64::from_str_radix(hi.trim(), 16).context("pg_lsn high")?;
    let lo = u64::from_str_radix(lo.trim(), 16).context("pg_lsn low")?;
    Ok((hi << 32) | lo)
}

/// 解析 Postgres 文本数组字面量 `{a,b,"c,d"}` → `Vec<String>`（处理引号与反斜杠转义）。
fn parse_pg_array(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    if s.len() < 2 || !s.starts_with('{') || !s.ends_with('}') {
        return out;
    }
    let inner = &s[1..s.len() - 1];
    if inner.is_empty() {
        return out;
    }
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' if !in_quotes => in_quotes = true,
            '"' if in_quotes => in_quotes = false,
            '\\' => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            ',' if !in_quotes => out.push(std::mem::take(&mut cur)),
            other => cur.push(other),
        }
    }
    out.push(cur);
    out
}

/// 单引号转义（SQL 字面量）。
fn esc(s: &str) -> String {
    s.replace('\'', "''")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pg_array_parsing() {
        assert_eq!(parse_pg_array("{}"), Vec::<String>::new());
        assert_eq!(parse_pg_array("{public}"), vec!["public"]);
        assert_eq!(parse_pg_array("{第3章,财务}"), vec!["第3章", "财务"]);
        assert_eq!(parse_pg_array("{team-a,public}"), vec!["team-a", "public"]);
        assert_eq!(parse_pg_array(r#"{"a,b",c}"#), vec!["a,b", "c"]);
        assert_eq!(parse_pg_array("nope"), Vec::<String>::new());
    }

    #[test]
    fn pg_lsn_parsing() {
        assert_eq!(parse_pg_lsn("0/0").unwrap(), 0);
        assert_eq!(parse_pg_lsn("0/16B3748").unwrap(), 0x16B3748);
        assert_eq!(parse_pg_lsn("1/0").unwrap(), 1u64 << 32);
        assert!(parse_pg_lsn("bogus").is_err());
    }

    #[test]
    fn pg_lsn_format_roundtrip() {
        for v in [0u64, 0x16B3748, 1u64 << 32, (3u64 << 32) | 0xABCDEF] {
            assert_eq!(parse_pg_lsn(&format_pg_lsn(v)).unwrap(), v);
        }
    }

    #[test]
    fn esc_quotes() {
        assert_eq!(esc("a'b"), "a''b");
    }
}
