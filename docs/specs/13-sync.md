# spec · fastsearch-sync

> 模块 #4，依赖：fastsearch-core、fastsearch-pg、（sink）fastsearch-text/vector。阶段 P1。
> 上游：[产品设计 §2.4/§3.13](../plans/2026-06-24-产品设计文档.md)、需求 F30/F51–F53。
> 状态：**开发中**。

## 1. 目的与范围

CDC 同步：把 Postgres（真源）的变更增量、可靠地应用到引擎侧派生索引。

- **变更模型**：`Change`（Insert/Update/Delete，携带 collection+chunk 或删除键 + LSN）。
- **Sink 抽象**：`IndexSink` trait（upsert chunk / delete by gid / delete by doc），由 text/vector 索引实现。
- **Applier**：幂等地把 `Change` 应用到 sink；维护 `applied_lsn` 检查点；保证按序、删除/替换正确、重复消息无副作用（exactly-once 效果）。
- **快照 + 增量切换**：初始全量快照建索引（用 pg.fetch 全表）→ 记 snapshot_lsn → 从该 LSN 起增量。
- **复制连接 + pgoutput 解码**：连接 PG 复制 slot、解码 pgoutput → `Change`。**env-gated 集成**（无 PG 时不跑）；wire 解码逻辑尽量纯函数可单测。

**不做**：检索、PG 写入（pg 模块）、具体索引实现（text/vector）。

## 2. 数据结构

```rust
pub struct Lsn(pub u64);                          // 复制日志序号
pub enum Change {
    Upsert { collection: String, chunk: Chunk },
    Delete { gid: GlobalId },
    DeleteDoc { collection: String, doc_id: String },
}
pub struct ChangeEvent { pub change: Change, pub lsn: Lsn }

pub trait IndexSink {
    fn apply_upsert(&mut self, collection: &str, chunk: &Chunk) -> anyhow::Result<()>;
    fn apply_delete(&mut self, gid: &GlobalId) -> anyhow::Result<()>;
    fn apply_delete_doc(&mut self, collection: &str, doc_id: &str) -> anyhow::Result<()>;
    fn commit(&mut self) -> anyhow::Result<()>;
}

pub struct Applier { applied_lsn: Lsn }
impl Applier {
    pub fn new(start_lsn: Lsn) -> Self;
    pub fn applied_lsn(&self) -> Lsn;
    /// 幂等应用：lsn <= applied_lsn 的事件忽略（已应用）；否则应用并推进 applied_lsn。
    pub fn apply(&mut self, sink: &mut dyn IndexSink, ev: &ChangeEvent) -> anyhow::Result<bool>;
    pub fn apply_batch(&mut self, sink: &mut dyn IndexSink, evs: &[ChangeEvent]) -> anyhow::Result<usize>;
}
```

## 3. 行为规约

- **幂等/续传**：`apply` 跳过 `lsn <= applied_lsn`（重启后从持久化的 applied_lsn 续传，重复消息无副作用）。返回是否实际应用。
- **按序**：apply_batch 假定输入按 LSN 升序；乱序中低于水位的被跳过。
- **替换语义**：`DeleteDoc` 后跟同 doc 的 `Upsert` 序列 = doc_id 级替换（与 pg.upsert_doc 对应）。
- **提交边界**：apply_batch 末尾调用 `sink.commit()`（成功后才认为 applied_lsn 持久化点推进——持久化由调用方负责）。
- **健壮**：sink 错误向上传播，不静默吞；applied_lsn 仅在 apply 成功后推进。

## 4. 快照 + 增量（集成层）

- `initial_snapshot(pg, sink, collections)`：从 pg 全量读 → sink.apply_upsert → commit；返回 snapshot 起点 LSN（实际 LSN 由复制协议在创建 slot 时给出；MVP 集成测试用 pg_current_wal_lsn 近似）。
- `stream(pg_repl_conn, applier, sink)`：从 slot 解码 pgoutput → ChangeEvent → applier.apply。**env-gated**。

## 5. 依赖

`fastsearch-core`、`fastsearch-pg`、`anyhow`、`tokio-postgres`（复制连接，集成）、（dev）mock sink。

## 6. 测试用例

**单元（必跑，纯逻辑）**：
1. Applier 幂等：apply 同一 ev 两次，第二次返回 false、sink 只收到一次。
2. 水位续传：从 start_lsn=100 起，lsn<=100 的事件被跳过、>100 的应用。
3. apply_batch：混合 Upsert/Delete/DeleteDoc 按序应用，applied_lsn 推进到最大；返回实际应用数。
4. 替换语义：DeleteDoc + 两个 Upsert → sink 记录 delete_doc 后 2 次 upsert。
5. sink 错误传播：sink 返回 Err 时 apply 返回 Err 且 applied_lsn 不推进。

**集成（env-gated）**：
6. pg 写入 → 近似 LSN → fetch 快照 → 内存 sink 得到对应 chunk。（完整 pgoutput 流式解码列为后续迭代。）

## 7. 验收标准与状态

- 单元测试全绿、clippy 净、fmt 净。
- 状态：
  - [x] v1 完成：Change 模型 + IndexSink trait + Applier（幂等/LSN 水位/批量/替换语义/错误不推进水位）+ 5 单测绿。clippy 净、fmt 净。

**已知限制 / 下一迭代：**
- **pgoutput 流式解码 + 复制连接尚未实现**（CDC 的"线缆层"）——v1 只做了"正确性核心"（apply 编排），这是 CDC 最容易出微妙 bug 的部分，已用 mock sink 测透。线缆层（tokio-postgres 复制模式 + pgoutput 二进制解码 + slot 生命周期 + 心跳）列入下一迭代，env-gated 集成测试。
- `initial_snapshot`/`stream` 集成函数待线缆层落地后补。
- IndexSink 由 fastsearch-engine 的适配器桥接 TextIndex（避免 text 反向依赖 sync）。
