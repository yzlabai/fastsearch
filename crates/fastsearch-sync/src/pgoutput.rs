//! pgoutput 逻辑复制消息解码（CDC 线缆层的"二进制解析"部分）。
//!
//! Postgres 逻辑复制以 **pgoutput** 插件输出二进制消息（大端），这是 CDC 最易出微妙
//! bug 的部分，故先把**纯解码**做透并单测（无需活 PG）。复制连接（流式读取 + slot
//! 生命周期 + 心跳）作为 env-gated 集成层在后续迭代接入——见 [spec §下一迭代](../../../docs/specs/13-sync.md)。
//!
//! 参考：PostgreSQL 协议「Logical Replication Message Formats」。字符串为 C 串（NUL 结尾），
//! 整数大端。本模块解析协议 v1 的常见消息：Begin/Commit/Origin/Relation/Type/Insert/
//! Update/Delete/Truncate，以及 TupleData。

use anyhow::{bail, Result};

/// 关系（表）的一列定义（来自 Relation 消息）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    /// 列标志位（bit0 = 属于复制标识键的一部分）。
    pub flags: u8,
    pub name: String,
    pub type_oid: u32,
    pub type_modifier: i32,
}

/// Relation 消息：描述某表的 OID 与列布局（后续 Insert/Update/Delete 按此解读元组）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relation {
    pub oid: u32,
    pub namespace: String,
    pub name: String,
    pub replica_identity: u8,
    pub columns: Vec<Column>,
}

impl Relation {
    /// 把一行元组与列名配对，便于按列名取值。长度不匹配 → 截断到较短者。
    pub fn pair<'a>(&'a self, tuple: &'a TupleData) -> Vec<(&'a str, &'a TupleValue)> {
        self.columns
            .iter()
            .map(|c| c.name.as_str())
            .zip(tuple.values.iter())
            .collect()
    }
}

/// 元组中单列的取值形态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TupleValue {
    /// SQL NULL。
    Null,
    /// 未变更的 TOAST 值（Update 时旧值未变，pgoutput 不重发）。
    UnchangedToast,
    /// 文本/二进制值的原始字节（'t' 文本 或 'b' 二进制）。
    Bytes(Vec<u8>),
}

impl TupleValue {
    /// 以 UTF-8 文本视图取值（Null/UnchangedToast/非 UTF-8 → None）。
    pub fn as_str(&self) -> Option<&str> {
        match self {
            TupleValue::Bytes(b) => std::str::from_utf8(b).ok(),
            _ => None,
        }
    }
}

/// 一行元组数据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TupleData {
    pub values: Vec<TupleValue>,
}

/// 一条 pgoutput 消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PgMessage {
    Begin {
        final_lsn: u64,
        commit_ts: i64,
        xid: u32,
    },
    Commit {
        flags: u8,
        commit_lsn: u64,
        end_lsn: u64,
        commit_ts: i64,
    },
    Origin {
        commit_lsn: u64,
        name: String,
    },
    Relation(Relation),
    Type {
        oid: u32,
        namespace: String,
        name: String,
    },
    Insert {
        rel_oid: u32,
        tuple: TupleData,
    },
    Update {
        rel_oid: u32,
        /// 复制标识键旧元组（replica identity = USING INDEX 时，'K'）。
        key: Option<TupleData>,
        /// 完整旧元组（replica identity = FULL 时，'O'）。
        old: Option<TupleData>,
        new: TupleData,
    },
    Delete {
        rel_oid: u32,
        key: Option<TupleData>,
        old: Option<TupleData>,
    },
    Truncate {
        options: u8,
        rel_oids: Vec<u32>,
    },
}

/// 大端字节游标读取器（带越界检查，不 panic）。
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.pos + n > self.buf.len() {
            bail!(
                "pgoutput: unexpected end of message (need {n} at {})",
                self.pos
            );
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn i16(&mut self) -> Result<i16> {
        let b = self.take(2)?;
        Ok(i16::from_be_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn i32(&mut self) -> Result<i32> {
        Ok(self.u32()? as i32)
    }
    fn u64(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        Ok(u64::from_be_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }
    fn i64(&mut self) -> Result<i64> {
        Ok(self.u64()? as i64)
    }
    /// C 串（读到 NUL）。
    fn cstr(&mut self) -> Result<String> {
        let start = self.pos;
        while self.pos < self.buf.len() && self.buf[self.pos] != 0 {
            self.pos += 1;
        }
        if self.pos >= self.buf.len() {
            bail!("pgoutput: unterminated C string");
        }
        let s = std::str::from_utf8(&self.buf[start..self.pos])
            .map_err(|e| anyhow::anyhow!("pgoutput: invalid utf8 in string: {e}"))?
            .to_string();
        self.pos += 1; // 跳过 NUL
        Ok(s)
    }
}

/// 解析一条 pgoutput 消息（不含上层 CopyData/XLogData 封装，仅消息体）。
pub fn parse_message(buf: &[u8]) -> Result<PgMessage> {
    let mut r = Reader::new(buf);
    let tag = r.u8()?;
    match tag {
        b'B' => Ok(PgMessage::Begin {
            final_lsn: r.u64()?,
            commit_ts: r.i64()?,
            xid: r.u32()?,
        }),
        b'C' => Ok(PgMessage::Commit {
            flags: r.u8()?,
            commit_lsn: r.u64()?,
            end_lsn: r.u64()?,
            commit_ts: r.i64()?,
        }),
        b'O' => Ok(PgMessage::Origin {
            commit_lsn: r.u64()?,
            name: r.cstr()?,
        }),
        b'R' => {
            let oid = r.u32()?;
            let namespace = r.cstr()?;
            let name = r.cstr()?;
            let replica_identity = r.u8()?;
            let ncols = r.i16()?;
            if ncols < 0 {
                bail!("pgoutput: negative column count");
            }
            let mut columns = Vec::with_capacity(ncols as usize);
            for _ in 0..ncols {
                columns.push(Column {
                    flags: r.u8()?,
                    name: r.cstr()?,
                    type_oid: r.u32()?,
                    type_modifier: r.i32()?,
                });
            }
            Ok(PgMessage::Relation(Relation {
                oid,
                namespace,
                name,
                replica_identity,
                columns,
            }))
        }
        b'Y' => Ok(PgMessage::Type {
            oid: r.u32()?,
            namespace: r.cstr()?,
            name: r.cstr()?,
        }),
        b'I' => {
            let rel_oid = r.u32()?;
            expect(&mut r, b'N')?;
            Ok(PgMessage::Insert {
                rel_oid,
                tuple: read_tuple(&mut r)?,
            })
        }
        b'U' => {
            let rel_oid = r.u32()?;
            let mut key = None;
            let mut old = None;
            // 可选旧元组：'K'（键）或 'O'（完整），随后必有 'N'（新元组）。
            let mut sub = r.u8()?;
            if sub == b'K' {
                key = Some(read_tuple(&mut r)?);
                sub = r.u8()?;
            } else if sub == b'O' {
                old = Some(read_tuple(&mut r)?);
                sub = r.u8()?;
            }
            if sub != b'N' {
                bail!("pgoutput: Update missing new tuple ('N'), got {sub:#x}");
            }
            Ok(PgMessage::Update {
                rel_oid,
                key,
                old,
                new: read_tuple(&mut r)?,
            })
        }
        b'D' => {
            let rel_oid = r.u32()?;
            let sub = r.u8()?;
            let (key, old) = match sub {
                b'K' => (Some(read_tuple(&mut r)?), None),
                b'O' => (None, Some(read_tuple(&mut r)?)),
                other => bail!("pgoutput: Delete expects 'K'|'O', got {other:#x}"),
            };
            Ok(PgMessage::Delete { rel_oid, key, old })
        }
        b'T' => {
            let nrels = r.u32()?;
            let options = r.u8()?;
            let mut rel_oids = Vec::with_capacity(nrels as usize);
            for _ in 0..nrels {
                rel_oids.push(r.u32()?);
            }
            Ok(PgMessage::Truncate { options, rel_oids })
        }
        other => bail!(
            "pgoutput: unknown message tag {:#x} ('{}')",
            other,
            other as char
        ),
    }
}

fn expect(r: &mut Reader, want: u8) -> Result<()> {
    let got = r.u8()?;
    if got != want {
        bail!("pgoutput: expected {want:#x}, got {got:#x}");
    }
    Ok(())
}

/// 读取 TupleData：Int16 列数 + 每列（'n' 空 / 'u' 未变 TOAST / 't'|'b' 长度+字节）。
fn read_tuple(r: &mut Reader) -> Result<TupleData> {
    let ncols = r.i16()?;
    if ncols < 0 {
        bail!("pgoutput: negative tuple column count");
    }
    let mut values = Vec::with_capacity(ncols as usize);
    for _ in 0..ncols {
        let kind = r.u8()?;
        let v = match kind {
            b'n' => TupleValue::Null,
            b'u' => TupleValue::UnchangedToast,
            b't' | b'b' => {
                let len = r.u32()? as usize;
                TupleValue::Bytes(r.take(len)?.to_vec())
            }
            other => bail!("pgoutput: unknown tuple column kind {other:#x}"),
        };
        values.push(v);
    }
    Ok(TupleData { values })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 小工具：拼接 pgoutput 字节。
    struct B(Vec<u8>);
    impl B {
        fn new(tag: u8) -> Self {
            B(vec![tag])
        }
        fn u8(mut self, v: u8) -> Self {
            self.0.push(v);
            self
        }
        fn u32(mut self, v: u32) -> Self {
            self.0.extend_from_slice(&v.to_be_bytes());
            self
        }
        fn i32(mut self, v: i32) -> Self {
            self.0.extend_from_slice(&v.to_be_bytes());
            self
        }
        fn i16(mut self, v: i16) -> Self {
            self.0.extend_from_slice(&v.to_be_bytes());
            self
        }
        fn u64(mut self, v: u64) -> Self {
            self.0.extend_from_slice(&v.to_be_bytes());
            self
        }
        fn i64(mut self, v: i64) -> Self {
            self.0.extend_from_slice(&v.to_be_bytes());
            self
        }
        fn cstr(mut self, s: &str) -> Self {
            self.0.extend_from_slice(s.as_bytes());
            self.0.push(0);
            self
        }
        /// 文本列：'t' + len + bytes。
        fn text(mut self, s: &str) -> Self {
            self.0.push(b't');
            self.0.extend_from_slice(&(s.len() as u32).to_be_bytes());
            self.0.extend_from_slice(s.as_bytes());
            self
        }
        fn done(self) -> Vec<u8> {
            self.0
        }
    }

    #[test]
    fn begin_commit() {
        let b = B::new(b'B').u64(0x1234).i64(700).u32(42).done();
        assert_eq!(
            parse_message(&b).unwrap(),
            PgMessage::Begin {
                final_lsn: 0x1234,
                commit_ts: 700,
                xid: 42
            }
        );
        let c = B::new(b'C').u8(0).u64(0x1234).u64(0x1240).i64(700).done();
        assert_eq!(
            parse_message(&c).unwrap(),
            PgMessage::Commit {
                flags: 0,
                commit_lsn: 0x1234,
                end_lsn: 0x1240,
                commit_ts: 700
            }
        );
    }

    #[test]
    fn relation_then_insert() {
        // 关系：public.fastsearch_chunks，两列 doc_id/page
        let rel = B::new(b'R')
            .u32(16385)
            .cstr("public")
            .cstr("fastsearch_chunks")
            .u8(b'd') // replica identity default
            .i16(2)
            .u8(1)
            .cstr("doc_id")
            .u32(25)
            .i32(-1)
            .u8(0)
            .cstr("page")
            .u32(23)
            .i32(-1)
            .done();
        let relation = match parse_message(&rel).unwrap() {
            PgMessage::Relation(r) => r,
            _ => panic!("expected relation"),
        };
        assert_eq!(relation.oid, 16385);
        assert_eq!(relation.name, "fastsearch_chunks");
        assert_eq!(relation.columns.len(), 2);
        assert_eq!(relation.columns[0].name, "doc_id");
        assert_eq!(relation.columns[1].type_oid, 23);

        // Insert：('r.pdf', '5')
        let ins = B::new(b'I')
            .u32(16385)
            .u8(b'N')
            .i16(2)
            .text("r.pdf")
            .text("5")
            .done();
        let tuple = match parse_message(&ins).unwrap() {
            PgMessage::Insert { rel_oid, tuple } => {
                assert_eq!(rel_oid, 16385);
                tuple
            }
            _ => panic!("expected insert"),
        };
        let paired = relation.pair(&tuple);
        assert_eq!(paired[0].0, "doc_id");
        assert_eq!(paired[0].1.as_str(), Some("r.pdf"));
        assert_eq!(paired[1].1.as_str(), Some("5"));
    }

    #[test]
    fn insert_with_null_and_unchanged() {
        // 3 列：'t' 文本 "x" / 'n' 空 / 'u' 未变 toast
        let mut ins = B::new(b'I').u32(1).u8(b'N').i16(3).text("x").done();
        ins.push(b'n');
        ins.push(b'u');
        match parse_message(&ins).unwrap() {
            PgMessage::Insert { tuple, .. } => {
                assert_eq!(tuple.values.len(), 3);
                assert_eq!(tuple.values[0], TupleValue::Bytes(b"x".to_vec()));
                assert_eq!(tuple.values[1], TupleValue::Null);
                assert_eq!(tuple.values[2], TupleValue::UnchangedToast);
            }
            _ => panic!("expected insert"),
        }
    }

    #[test]
    fn update_with_key_and_delete() {
        // Update：'K' 旧键元组 + 'N' 新元组
        let upd = {
            let mut v = vec![b'U'];
            v.extend_from_slice(&7u32.to_be_bytes());
            v.push(b'K');
            v.extend_from_slice(&1i16.to_be_bytes());
            v.push(b't');
            v.extend_from_slice(&3u32.to_be_bytes());
            v.extend_from_slice(b"old");
            v.push(b'N');
            v.extend_from_slice(&1i16.to_be_bytes());
            v.push(b't');
            v.extend_from_slice(&3u32.to_be_bytes());
            v.extend_from_slice(b"new");
            v
        };
        match parse_message(&upd).unwrap() {
            PgMessage::Update {
                rel_oid,
                key,
                old,
                new,
            } => {
                assert_eq!(rel_oid, 7);
                assert_eq!(key.unwrap().values[0].as_str(), Some("old"));
                assert!(old.is_none());
                assert_eq!(new.values[0].as_str(), Some("new"));
            }
            _ => panic!("expected update"),
        }
        // Delete：'K' 键元组
        let del = {
            let mut v = vec![b'D'];
            v.extend_from_slice(&7u32.to_be_bytes());
            v.push(b'K');
            v.extend_from_slice(&1i16.to_be_bytes());
            v.push(b't');
            v.extend_from_slice(&1u32.to_be_bytes());
            v.push(b'9');
            v
        };
        match parse_message(&del).unwrap() {
            PgMessage::Delete { rel_oid, key, old } => {
                assert_eq!(rel_oid, 7);
                assert_eq!(key.unwrap().values[0].as_str(), Some("9"));
                assert!(old.is_none());
            }
            _ => panic!("expected delete"),
        }
    }

    #[test]
    fn truncate_and_errors() {
        let t = B::new(b'T').u32(2).u8(0).u32(10).u32(11).done();
        assert_eq!(
            parse_message(&t).unwrap(),
            PgMessage::Truncate {
                options: 0,
                rel_oids: vec![10, 11]
            }
        );
        // 截断的消息 → Err，不 panic
        assert!(parse_message(&[b'B', 0, 0]).is_err());
        // 未知 tag → Err
        assert!(parse_message(b"Z").is_err());
        // 空 buffer → Err
        assert!(parse_message(&[]).is_err());
    }
}
