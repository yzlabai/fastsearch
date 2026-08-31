# spec · fastsearch-sync

> 模块 #4，依赖：fastsearch-core、fastsearch-pg、（sink）fastsearch-text/vector。阶段 P1。
> 上游：[产品设计 §2.4/§3.13](../plans/2026-06-24-产品设计文档.md)、需求 F30/F51–F53。
> 状态：**已落地核心**（pgoutput 解码 + CDC 消费 + 初始快照 bootstrap + LSN 检查点续传 + 幂等替换，Docker pgvector 端到端验证）。流式 pgoutput 线缆层（替代 SQL 轮询）= 下一迭代。

## 1. 目的与范围

CDC 同步：把 Postgres（真源）的变更增量、可靠地应用到引擎侧派生索引。

- **变更模型**：`Change`（Upsert/Delete/DeleteDoc/Clear/Batch，携带 collection+chunk 或删除键 + LSN）。
- **Sink 抽象**：`IndexSink` trait（upsert chunk / delete by gid / delete by doc / clear），由 engine 桥接 text/vector 索引。
- **Applier**：供无 slot 的独立调用方/测试按事件 LSN 去重；生产 slot 消费不使用其水位，因为 pgoutput 行消息 LSN 可能是事务起点，生产检查点只采用本批最高 commit LSN。
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
    Clear,
    Batch(Vec<Change>),
}
pub struct ChangeEvent { pub change: Change, pub lsn: Lsn }

pub trait IndexSink {
    fn apply_upsert(&mut self, collection: &str, chunk: &Chunk) -> anyhow::Result<()>;
    fn apply_delete(&mut self, gid: &GlobalId) -> anyhow::Result<()>;
    fn apply_delete_doc(&mut self, collection: &str, doc_id: &str) -> anyhow::Result<()>;
    fn apply_clear(&mut self) -> anyhow::Result<()>;
    fn apply_changes(&mut self, changes: &[Change]) -> anyhow::Result<()>;
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

- **独立 Applier 幂等**：`apply` 跳过 `lsn <= applied_lsn`；该契约不用于生产 slot 消费。
- **生产续传**：Engine 对 peek 批次全部幂等应用，以最高 commit LSN 持久化并 advance；事件 LSN 不参与首批跳过判断。
- **按序**：apply_batch 假定输入按 LSN 升序；乱序中低于水位的被跳过。
- **替换语义**：`DeleteDoc` 后跟同 doc 的 `Upsert` 序列 = doc_id 级替换（与 pg.upsert_doc 对应）。
- **主键迁移**：Update 旧 key 与新 tuple 的 GlobalId 不同时，映射成同一 LSN 下有序的 `Batch[Delete(old), Upsert(new)]`；避免旧 citation 成为幽灵行，也不伪造递增 LSN。
- **TRUNCATE**：白名单真源表的 Truncate 映射为 `Clear`，由 Engine 清 text/vector 后在批末统一 commit。
- **relation 白名单**：`ReplicationConfig.source_table` 以 `schema.table` 精确匹配 Relation，同时保留 chunks 必需列检查；同形旁表也不会产生 chunk 变更。
- **批次接缝**：`apply_batch` 将水位以上的有序 Change 一次交给 `sink.apply_changes`；默认实现逐项应用，Engine 覆写为全批 prepare→publish。
- **提交边界**：apply_batch 末尾调用 `sink.commit()`；只有 apply 与 commit 都成功才把 `applied_lsn` 从批前水位推进到本批最大值。
- **健壮**：sink 错误向上传播，不静默吞；applied_lsn 仅在 apply 成功后推进。

## 4. 快照 + 增量（集成层）—— ✅ 已实现（2026-06-25，Docker 验证）

- **初始快照 bootstrap**：`ensure_slot -> Option<Lsn>`（新建返回一致点）；`pg::fetch_all_chunks` 全表读；`engine::bootstrap_snapshot(rows, data, consistent)` 对全部行做一次批量 prepare + persist；server 首启 + 新建 slot 时自动 bootstrap 存量、再起增量。**正确性=一致点 + 幂等重叠**（不用 EXPORT_SNAPSHOT），详见 [计划](../plans/2026-06-25-初始快照-bootstrap.md)。
- **增量消费**：`engine::consume_once`（peek→应用全部→persist→advance）。**关键修正**：peek 逐行 lsn 对首事务等于一致点，故 consume_once **不靠 LSN 水位跳过**，靠 slot-advance 不重投 + GlobalId 幂等。
- 低延迟流式 `START_REPLICATION`（替代 SQL 轮询）仍为后续。

## 5. 依赖

`fastsearch-core`、`fastsearch-pg`、`anyhow`、`tokio-postgres`（复制连接，集成）、（dev）mock sink。

## 6. 测试用例

**单元（必跑，纯逻辑）**：
1. Applier 幂等：apply 同一 ev 两次，第二次返回 false、sink 只收到一次。
2. 水位续传：从 start_lsn=100 起，lsn<=100 的事件被跳过、>100 的应用。
3. apply_batch：混合 Upsert/Delete/DeleteDoc 按序应用，applied_lsn 推进到最大；返回实际应用数。
4. 替换语义：DeleteDoc + 两个 Upsert → sink 记录 delete_doc 后 2 次 upsert。
5. sink 错误传播：sink 返回 Err 时 apply 返回 Err 且 applied_lsn 不推进。
6. 复合 PK Update：同一事件严格先 Delete(old) 再 Upsert(new)，水位只推进一次。
7. TRUNCATE：Clear 传到 sink；同形不同名 Relation 被白名单拒绝。
8. commit 失败不推进批次水位；embedding timeout/维度错误不留下可被后续 commit 发布的半状态。

**集成（env-gated）**：
9. 真 PG 修改三列主键后旧 citation 消失、新 citation 命中；TRUNCATE 后 keyword/vector/hybrid 均无旧命中。
10. PG 写穿中途失败整批回滚并可重试；外部 embedding 等待不持 Engine 锁；发布期索引维度/后端上限失败不先执行 Delete/Clear；纯本地 apply 后崩溃不推进 slot，重启重放后两路收敛。

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
  - [x] FS-101（2026-08-31，Docker pgvector:pg17 真机）：`map` 升级为零到多变更；PK UPDATE 输出有序 Delete+Upsert 复合事件；TRUNCATE 输出 Clear 并清 text/vector；Relation 改为 `ReplicationConfig.source_table` 精确限定表名 + 列形状双守卫。环境门禁现为 PG/CDC 21/21 executed。
  - [x] FS-102（2026-08-31，Docker pgvector:pg17 真机）：`apply_changes` 建立整批接缝，Engine 全批准备后发布；一次 `embed_multi`、PG 事务写穿、嵌入锁外等待与故障重试闭环落地。PG 写穿至本地发布期间由 Engine 锁阻断搜索；跨 PG/本地文件的进程崩溃恢复边界明确转入 FS-103。PG/CDC 显式门禁增至 24 项。
  - [x] FS-103（2026-08-31，Docker pgvector:pg17 真机）：并发 `ensure_slot` 捕获 PG duplicate-object，8 副本首建全成功；生产路径移除 `Applier` 水位并统一为 commit LSN；PG 写穿/本地发布前原子落 `cdc-batch-intent.json`，persist 后更新阶段，slot advance 后清除。任何中途错误或崩溃均阻断 readiness，常规轮询按 GlobalId 幂等重放；不宣称跨 PG/本地文件 2PC。peek/apply/persist/advance 四边界恢复、恢复标记和状态指标均有实库回归。PG/CDC 显式门禁增至 28 项。

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

- `source_table` 当前是单个精确白名单项，因为 chunks publication 按不变量只含一个真源表；未来若确需同一消费者处理多张 chunk 真源表，再显式升级为集合，不以 shape guard 代替身份。
- 低延迟**流式**消费（`START_REPLICATION` COPY + keepalive/standby 反馈）：当前用 SQL 轮询，足够正确性与中低频；流式待换支持复制协议的客户端（或自实现 wire）。
- slot lag 已暴露；`max_slot_wal_keep_size` 自动治理、slot 失效后的自动重建仍待续。
- IndexSink 由 fastsearch-engine 的适配器桥接 TextIndex（避免 text 反向依赖 sync）。
