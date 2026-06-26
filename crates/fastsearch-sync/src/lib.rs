//! # fastsearch-sync
//!
//! CDC 应用编排：把 Postgres（真源）的变更**幂等、按 LSN 水位、可续传地**应用到
//! 引擎侧派生索引（经 [`IndexSink`]）。这是 CDC 的"正确性核心"，与具体的复制
//! 连接/pgoutput 解码解耦——后者作为 env-gated 集成层在后续迭代接入。
//!
//! 详见 [spec](../../docs/specs/13-sync.md)。设计要点：
//! - **幂等/续传**：`lsn <= applied_lsn` 的事件被跳过（重启从持久化 LSN 续传，
//!   重复消息无副作用）→ 达到 exactly-once 效果。
//! - **按序**：批量假定 LSN 升序；低于水位者跳过。
//! - **替换语义**：`DeleteDoc` 后跟同 doc `Upsert` 序列 = doc_id 级替换。
//! - **不静默吞错**：sink 错误向上传播，applied_lsn 仅在成功后推进。

pub mod pgoutput;
pub mod replication;

use fastsearch_core::{Chunk, GlobalId};

/// 复制日志序号。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Lsn(pub u64);

/// 一次变更。
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    /// 新增或更新一个 chunk（按 global_id 覆盖）。
    Upsert {
        collection: String,
        chunk: Box<Chunk>,
    },
    /// 删除一个 chunk。
    Delete { gid: GlobalId },
    /// 删除某 doc 的全部 chunk（doc_id 级替换的第一步）。
    DeleteDoc { collection: String, doc_id: String },
}

/// 带 LSN 的变更事件。
#[derive(Debug, Clone, PartialEq)]
pub struct ChangeEvent {
    pub change: Change,
    pub lsn: Lsn,
}

/// 派生索引落地端（由 fastsearch-text / fastsearch-vector 实现）。
pub trait IndexSink {
    fn apply_upsert(&mut self, collection: &str, chunk: &Chunk) -> anyhow::Result<()>;
    fn apply_delete(&mut self, gid: &GlobalId) -> anyhow::Result<()>;
    fn apply_delete_doc(&mut self, collection: &str, doc_id: &str) -> anyhow::Result<()>;
    fn commit(&mut self) -> anyhow::Result<()>;
}

/// 幂等、LSN 水位驱动的应用器。
pub struct Applier {
    applied_lsn: Lsn,
}

impl Applier {
    /// 从某起点 LSN 开始（重启时传入持久化的 applied_lsn）。
    pub fn new(start_lsn: Lsn) -> Self {
        Applier {
            applied_lsn: start_lsn,
        }
    }

    pub fn applied_lsn(&self) -> Lsn {
        self.applied_lsn
    }

    /// 应用单个事件。`lsn <= applied_lsn` 视为已应用、跳过并返回 `Ok(false)`；
    /// 否则应用到 sink、成功后推进 applied_lsn、返回 `Ok(true)`。
    pub fn apply(&mut self, sink: &mut dyn IndexSink, ev: &ChangeEvent) -> anyhow::Result<bool> {
        if ev.lsn <= self.applied_lsn {
            return Ok(false);
        }
        match &ev.change {
            Change::Upsert { collection, chunk } => sink.apply_upsert(collection, chunk)?,
            Change::Delete { gid } => sink.apply_delete(gid)?,
            Change::DeleteDoc { collection, doc_id } => {
                sink.apply_delete_doc(collection, doc_id)?
            }
        }
        // 仅在成功后推进水位（sink 出错则水位不动，可重试）。
        self.applied_lsn = ev.lsn;
        Ok(true)
    }

    /// 批量应用（输入按 LSN 升序），末尾 `commit`。返回实际应用条数。
    pub fn apply_batch(
        &mut self,
        sink: &mut dyn IndexSink,
        evs: &[ChangeEvent],
    ) -> anyhow::Result<usize> {
        let mut applied = 0;
        for ev in evs {
            if self.apply(sink, ev)? {
                applied += 1;
            }
        }
        sink.commit()?;
        Ok(applied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastsearch_core::{BBox, ChunkKind};

    #[derive(Debug, PartialEq)]
    enum Op {
        Upsert(GlobalId),
        Delete(GlobalId),
        DeleteDoc(String, String),
        Commit,
    }

    #[derive(Default)]
    struct MockSink {
        ops: Vec<Op>,
        fail: bool,
    }
    impl IndexSink for MockSink {
        fn apply_upsert(&mut self, collection: &str, chunk: &Chunk) -> anyhow::Result<()> {
            if self.fail {
                anyhow::bail!("sink failure");
            }
            self.ops.push(Op::Upsert(chunk.global_id(collection)));
            Ok(())
        }
        fn apply_delete(&mut self, gid: &GlobalId) -> anyhow::Result<()> {
            self.ops.push(Op::Delete(gid.clone()));
            Ok(())
        }
        fn apply_delete_doc(&mut self, collection: &str, doc_id: &str) -> anyhow::Result<()> {
            self.ops
                .push(Op::DeleteDoc(collection.into(), doc_id.into()));
            Ok(())
        }
        fn commit(&mut self) -> anyhow::Result<()> {
            self.ops.push(Op::Commit);
            Ok(())
        }
    }

    fn chunk(doc: &str, id: u64) -> Box<Chunk> {
        Box::new(Chunk {
            doc_id: doc.into(),
            chunk_id: id,
            kind: ChunkKind::Paragraph,
            text: "t".into(),
            page: 1,
            bbox: BBox {
                x0: 0.0,
                y0: 0.0,
                x1: 1.0,
                y1: 1.0,
            },
            heading_path: vec![],
            section_id: 0,
            char_len: 1,
            media: None,
            tenant: None,
            acl: vec!["public".into()],
        })
    }

    fn ev(change: Change, lsn: u64) -> ChangeEvent {
        ChangeEvent {
            change,
            lsn: Lsn(lsn),
        }
    }

    fn gid(doc: &str, id: u64) -> GlobalId {
        GlobalId {
            collection: "kb".into(),
            doc_id: doc.into(),
            chunk_id: id,
        }
    }

    #[test]
    fn idempotent_same_event() {
        let mut sink = MockSink::default();
        let mut ap = Applier::new(Lsn(0));
        let e = ev(
            Change::Upsert {
                collection: "kb".into(),
                chunk: chunk("a", 1),
            },
            5,
        );
        assert!(ap.apply(&mut sink, &e).unwrap()); // first applies
        assert!(!ap.apply(&mut sink, &e).unwrap()); // second skipped (lsn<=watermark)
        assert_eq!(sink.ops, vec![Op::Upsert(gid("a", 1))]);
        assert_eq!(ap.applied_lsn(), Lsn(5));
    }

    #[test]
    fn watermark_resume_skips_old() {
        let mut sink = MockSink::default();
        let mut ap = Applier::new(Lsn(100));
        // <=100 跳过，>100 应用
        assert!(!ap
            .apply(&mut sink, &ev(Change::Delete { gid: gid("a", 1) }, 50))
            .unwrap());
        assert!(!ap
            .apply(&mut sink, &ev(Change::Delete { gid: gid("a", 1) }, 100))
            .unwrap());
        assert!(ap
            .apply(&mut sink, &ev(Change::Delete { gid: gid("a", 1) }, 101))
            .unwrap());
        assert_eq!(sink.ops, vec![Op::Delete(gid("a", 1))]);
        assert_eq!(ap.applied_lsn(), Lsn(101));
    }

    #[test]
    fn batch_mixed_and_replace_semantics() {
        let mut sink = MockSink::default();
        let mut ap = Applier::new(Lsn(0));
        // doc_id 级替换：DeleteDoc 后两个 Upsert
        let evs = vec![
            ev(
                Change::DeleteDoc {
                    collection: "kb".into(),
                    doc_id: "a".into(),
                },
                1,
            ),
            ev(
                Change::Upsert {
                    collection: "kb".into(),
                    chunk: chunk("a", 1),
                },
                2,
            ),
            ev(
                Change::Upsert {
                    collection: "kb".into(),
                    chunk: chunk("a", 2),
                },
                3,
            ),
        ];
        let n = ap.apply_batch(&mut sink, &evs).unwrap();
        assert_eq!(n, 3);
        assert_eq!(
            sink.ops,
            vec![
                Op::DeleteDoc("kb".into(), "a".into()),
                Op::Upsert(gid("a", 1)),
                Op::Upsert(gid("a", 2)),
                Op::Commit,
            ]
        );
        assert_eq!(ap.applied_lsn(), Lsn(3));
    }

    #[test]
    fn batch_skips_below_watermark() {
        let mut sink = MockSink::default();
        let mut ap = Applier::new(Lsn(2));
        let evs = vec![
            ev(
                Change::Upsert {
                    collection: "kb".into(),
                    chunk: chunk("a", 1),
                },
                1,
            ), // skip
            ev(
                Change::Upsert {
                    collection: "kb".into(),
                    chunk: chunk("a", 2),
                },
                3,
            ), // apply
        ];
        let n = ap.apply_batch(&mut sink, &evs).unwrap();
        assert_eq!(n, 1);
        assert_eq!(sink.ops, vec![Op::Upsert(gid("a", 2)), Op::Commit]);
    }

    #[test]
    fn sink_error_does_not_advance_watermark() {
        let mut sink = MockSink {
            fail: true,
            ..Default::default()
        };
        let mut ap = Applier::new(Lsn(0));
        let e = ev(
            Change::Upsert {
                collection: "kb".into(),
                chunk: chunk("a", 1),
            },
            7,
        );
        assert!(ap.apply(&mut sink, &e).is_err());
        // 水位未推进，可重试
        assert_eq!(ap.applied_lsn(), Lsn(0));
    }
}
