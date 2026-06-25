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
  - [x] v1.1：**pgoutput 二进制解码**（`pgoutput` 模块）—— 大端游标解析 Begin/Commit/Origin/Relation/Type/Insert/Update/Delete/Truncate + TupleData（null/unchanged-toast/text）；越界/未知 tag/非法 utf8 均返回 Err 不 panic；`Relation::pair` 按列名配对取值。纯函数、+5 单测（对构造字节）。**这是线缆层里最易出微妙 bug 的部分，先做透**。
  - [x] v1.2（**CDC 闭环真 PG 验证 done**，2026-06-25）：`replication` 模块 —— `ensure_slot`/`drop_slot`/`pull_changes(cfg)`。
    - **传输选型**：tokio-postgres 0.7.18 **无** `START_REPLICATION`/`copy_both` API，故改用逻辑解码 SQL 函数 `pg_logical_slot_get_binary_changes`（普通连接拉取 pgoutput 二进制）——一种合法的轮询式 CDC 消费。低延迟 COPY 流式为后续可选。
    - ⚠️ **崩溃安全（当前为 v1 演示级，未达生产）**：`get_binary_changes` 是**消费即推进 slot**——"拉取后、派生索引落盘前崩溃"会丢这批变更（slot 已推进、内存索引未持久化）。生产正确姿势是 **peek + 先落盘后 `pg_replication_slot_advance`**（详见 [派生索引持久化与崩溃安全计划](../plans/2026-06-25-派生索引持久化与崩溃安全.md)）。当前 `pull_changes` 仅供"无持久化"的闭环演示/测试。
    - **映射**：Relation 缓存 + Insert/Update→`Upsert`、Delete→`Delete`（PK→GlobalId）；行→Chunk 复用 `fastsearch_pg::ChunkRow::to_chunk`；含 `pg_lsn` 文本解析、Postgres `text[]` 数组字面量解析（+3 单测）。
    - **端到端闭环**（`fastsearch-engine/tests/cdc_closed_loop.rs`，env-gated）：写 PgStore → slot 捕获 → `pull_changes` 解码 → `Applier` 应用到 `Engine` → 检索命中（引用正确）。Docker pgvector 上全绿、可幂等重跑。

**复测配方（Docker）：**
```bash
docker run -d --name fs-pg -e POSTGRES_PASSWORD=pw -e POSTGRES_USER=fs -e POSTGRES_DB=fsdb \
  -p 55432:5432 pgvector/pgvector:pg17 \
  -c wal_level=logical -c max_replication_slots=8 -c max_wal_senders=8
export DATABASE_URL="postgres://fs:pw@localhost:55432/fsdb"
cargo test -p fastsearch-pg integration_roundtrip          # 真源写/替换/读回
cargo test -p fastsearch-engine --test cdc_closed_loop      # CDC 闭环
```

**已知限制 / 下一迭代：**
- 低延迟**流式**消费（`START_REPLICATION` COPY + keepalive/standby 反馈）：当前用 SQL 轮询，足够正确性与中低频；流式待换支持复制协议的客户端（或自实现 wire）。
- slot 生命周期监控（`max_slot_wal_keep_size`、滞留告警）、初始快照 + 无缝切换（B3）待续。
- `initial_snapshot`/`stream` 集成函数待线缆层落地后补。
- IndexSink 由 fastsearch-engine 的适配器桥接 TextIndex（避免 text 反向依赖 sync）。
