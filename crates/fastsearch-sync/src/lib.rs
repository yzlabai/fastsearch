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
    /// 清空真源表对应的全部派生索引（PG `TRUNCATE`）。
    Clear,
    /// 同一条 WAL 消息产生的有序复合单元（如 PK UPDATE 的 Delete(old) + Upsert(new)）。
    /// 共用一个 LSN，必须作为单个 `ChangeEvent` 应用，不能伪造递增 LSN。
    Batch(Vec<Change>),
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
    fn apply_clear(&mut self) -> anyhow::Result<()>;
    /// 准备并应用一组有序变更。实现可覆写此方法，在改变可见派生状态前完成批量嵌入等
    /// 易失败工作；默认实现保持既有逐项行为。
    fn apply_changes(&mut self, changes: &[Change]) -> anyhow::Result<()> {
        for change in changes {
            match change {
                Change::Upsert { collection, chunk } => self.apply_upsert(collection, chunk)?,
                Change::Delete { gid } => self.apply_delete(gid)?,
                Change::DeleteDoc { collection, doc_id } => {
                    self.apply_delete_doc(collection, doc_id)?
                }
                Change::Clear => self.apply_clear()?,
                Change::Batch(nested) => self.apply_changes(nested)?,
            }
        }
        Ok(())
    }
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
        sink.apply_changes(std::slice::from_ref(&ev.change))?;
        // 仅在整个 compound change 成功后推进水位（sink 出错则水位不动，可重试）。
        self.applied_lsn = ev.lsn;
        Ok(true)
    }

    /// 批量应用（输入按 LSN 升序），末尾 `commit`。返回实际应用条数。
    pub fn apply_batch(
        &mut self,
        sink: &mut dyn IndexSink,
        evs: &[ChangeEvent],
    ) -> anyhow::Result<usize> {
        let starting_lsn = self.applied_lsn;
        let pending: Vec<Change> = evs
            .iter()
            .filter(|ev| ev.lsn > starting_lsn)
            .map(|ev| ev.change.clone())
            .collect();
        let applied = pending.len();
        let final_lsn = evs
            .iter()
            .filter(|ev| ev.lsn > starting_lsn)
            .map(|ev| ev.lsn)
            .max()
            .unwrap_or(starting_lsn);
        sink.apply_changes(&pending)?;
        if let Err(err) = sink.commit() {
            self.applied_lsn = starting_lsn;
            return Err(err);
        }
        self.applied_lsn = final_lsn;
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
        Clear,
        Commit,
    }

    #[derive(Default)]
    struct MockSink {
        ops: Vec<Op>,
        fail: bool,
        fail_commit: bool,
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
        fn apply_clear(&mut self) -> anyhow::Result<()> {
            self.ops.push(Op::Clear);
            Ok(())
        }
        fn commit(&mut self) -> anyhow::Result<()> {
            if self.fail_commit {
                anyhow::bail!("commit failure");
            }
            self.ops.push(Op::Commit);
            Ok(())
        }
    }

    #[derive(Default)]
    struct BatchOnlySink {
        batches: Vec<Vec<Change>>,
    }

    impl IndexSink for BatchOnlySink {
        fn apply_upsert(&mut self, _collection: &str, _chunk: &Chunk) -> anyhow::Result<()> {
            anyhow::bail!("single-item dispatcher must not be used")
        }

        fn apply_delete(&mut self, _gid: &GlobalId) -> anyhow::Result<()> {
            anyhow::bail!("single-item dispatcher must not be used")
        }

        fn apply_delete_doc(&mut self, _collection: &str, _doc_id: &str) -> anyhow::Result<()> {
            anyhow::bail!("single-item dispatcher must not be used")
        }

        fn apply_clear(&mut self) -> anyhow::Result<()> {
            anyhow::bail!("single-item dispatcher must not be used")
        }

        fn apply_changes(&mut self, changes: &[Change]) -> anyhow::Result<()> {
            self.batches.push(changes.to_vec());
            Ok(())
        }

        fn commit(&mut self) -> anyhow::Result<()> {
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
            media_bytes: None,
            image_vector_status: None,
            tenant: None,
            acl: vec!["public".into()],
            metadata: Default::default(),
            searchable: true,
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
    fn single_event_uses_the_same_batch_dispatch_seam() {
        let mut sink = BatchOnlySink::default();
        let mut applier = Applier::new(Lsn(0));
        let event = ev(Change::Clear, 5);

        assert!(applier.apply(&mut sink, &event).unwrap());
        assert_eq!(sink.batches, vec![vec![Change::Clear]]);
        assert_eq!(applier.applied_lsn(), Lsn(5));
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
    fn compound_pk_update_preserves_delete_then_upsert_at_one_lsn() {
        let mut sink = MockSink::default();
        let mut ap = Applier::new(Lsn(0));
        let event = ev(
            Change::Batch(vec![
                Change::Delete { gid: gid("old", 1) },
                Change::Upsert {
                    collection: "kb".into(),
                    chunk: chunk("new", 2),
                },
            ]),
            7,
        );
        assert!(ap.apply(&mut sink, &event).unwrap());
        assert_eq!(
            sink.ops,
            vec![Op::Delete(gid("old", 1)), Op::Upsert(gid("new", 2))]
        );
        assert_eq!(ap.applied_lsn(), Lsn(7));
    }

    #[test]
    fn truncate_clears_the_sink() {
        let mut sink = MockSink::default();
        let mut ap = Applier::new(Lsn(0));
        assert!(ap.apply(&mut sink, &ev(Change::Clear, 9)).unwrap());
        assert_eq!(sink.ops, vec![Op::Clear]);
        assert_eq!(ap.applied_lsn(), Lsn(9));
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

    #[test]
    fn commit_error_does_not_advance_watermark() {
        let mut sink = MockSink {
            fail_commit: true,
            ..Default::default()
        };
        let mut ap = Applier::new(Lsn(0));
        let err = ap
            .apply_batch(
                &mut sink,
                &[ev(
                    Change::Upsert {
                        collection: "kb".into(),
                        chunk: chunk("a", 1),
                    },
                    8,
                )],
            )
            .unwrap_err();
        assert!(err.to_string().contains("commit failure"));
        assert_eq!(ap.applied_lsn(), Lsn(0));
    }
}
