# 初始快照 bootstrap（接入已有 PG 表）

> 状态：**计划（待 review → 实施）**｜日期：2026-06-25｜跨模块：`pg` / `sync` / `engine` / `server`。
> 上游：[13-sync §4 快照+增量](../specs/13-sync.md)、[持久化与崩溃安全计划](2026-06-25-派生索引持久化与崩溃安全.md)（共用检查点/幂等）。
> 不变量：PG 真源、引擎索引派生可重建；幂等 + LSN 水位。

## 1. 目的与范围

**问题**：当前 CDC 只捕获 **slot 创建之后** 的变更。接入一张**已有数据**的 PG 表时，存量行永远不会进派生索引——"接入已有库"这条路是断的。

**要做**：首次启动时做一次**一致性初始快照**——把已有行全量导入派生索引，并与增量 CDC **无缝衔接**（不丢、不重）。

**不做（本轮）**：分页/流式快照大表（先全量 fetch，超大表分页列为后续）；导出快照给多连接共享读（用幂等 + 一致点近似，见 §3）；并发副本各自 bootstrap。

## 2. 当前缺口

- `ensure_slot` 仅返回 `Result<()>`，**丢弃了 `pg_create_logical_replication_slot` 给出的一致点 LSN**——而这正是快照与增量的衔接点。
- `PgStore` 只有 `fetch_doc(collection, doc_id)`，**无全表读**。
- 无 bootstrap 编排：首启时没有"先快照存量、再增量"的逻辑。

## 3. 正确性模型（一致点 + 幂等重叠）

```
1. 建 slot：pg_create_logical_replication_slot(slot,'pgoutput') 返回 consistent_point LSN。
   —— slot 创建后，所有 > consistent_point 的提交都会被 slot 捕获。
2. 快照读：SELECT * FROM table（在建 slot 之后执行）→ 逐行 apply_upsert（含嵌入）。
3. 落盘：persist(data, applied_lsn = consistent_point)。
4. 增量：从 consistent_point 起 CDC（peek/consume_once）。
```

**为何不丢**：consistent_point 之前的数据在快照里；之后的在 slot 里。两者并集 = 全部。
**为何不重（即便重叠）**：快照在建 slot 之后执行，可能含 `(consistent_point, 快照时刻]` 间的少量变更，这些也会被 slot 再投递一次——但 **apply 幂等**（按 `GlobalId` upsert/delete），重复 INSERT = 同结果，重复 DELETE = 已删则 no-op。故"至少一次 + 幂等 = exactly-once 效果"，与持久化计划同一保证。
> 注：不使用 PG 的 `EXPORT_SNAPSHOT`/`SET TRANSACTION SNAPSHOT`（需复制协议连接，tokio-postgres 0.7 不支持）；用"一致点 + 幂等重叠"达到等效正确性，代价是重叠窗口少量重放（无害）。

## 4. 何时 bootstrap（判定）

| 检查点 applied_lsn | slot | 动作 |
|---|---|---|
| 0（首启，引擎空） | 新建（ensure_slot 返回 Some(lsn)） | **快照** → checkpoint=consistent_point → 增量 |
| 0 | 已存在（返回 None） | 不能取干净一致点；记 `警告` 跳过快照、从 slot 现位增量（边界场景，文档标注） |
| >0（恢复） | 任意 | 不快照（已 bootstrap），直接从检查点续传 |

## 5. API 变更

- `pg`：`PgStore::fetch_all_chunks() -> Result<Vec<(String, Chunk)>>`（全表读，按 (collection,doc_id,chunk_id) 升序；复用 `row_to_chunk`）。
- `sync`：`ensure_slot(cfg) -> Result<Option<Lsn>>`——新建 slot 时返回 `Some(consistent_point)`，已存在返回 `None`。（现有调用 `ensure_slot(&rcfg).await?;` 仍兼容，丢弃 Option。）
- `engine`：`bootstrap_snapshot(rows: &[(String, Chunk)], data_dir, lsn) -> Result<usize>`——逐行 `apply_upsert`（经 embedder 嵌入）→ `persist(data_dir, lsn)`；返回导入条数。
- `server` main：首启 + CDC 开 + slot 新建 → `fetch_all_chunks` → `bootstrap_snapshot(consistent_point)` → `spawn_cdc(start_lsn=consistent_point)`。

## 6. 测试用例

**集成（Docker PG，env-gated）**：
1. **存量导入**：建表 + 先写 2 行 → 建 slot（拿 consistent_point）→ `fetch_all_chunks` 得 2 → `bootstrap_snapshot` → 检索命中（keyword + 若有嵌入则 vector）。
2. **无缝衔接**：bootstrap 后再写第 3 行 → `consume_once` 拉到 1 条 → 共 3 条可检索（不丢、不重）。
3. **幂等重叠**：bootstrap 用的 consistent_point 之后若 slot 再投递快照已含的行，`Applier`/upsert 不产生重复条目。
4. `ensure_slot` 返回值：新建→Some(lsn>0)；再调→None。

**活服务（手验）**：先写 PG 存量 → 启动 server(CDC) → 启动日志显示 bootstrap N 行 → 直接检索命中（无需任何新写入）。

## 7. 验收标准

- 集成测试在 Docker PG 全绿；离线收口（fmt/clippy/test）净。
- 活服务：对**已有数据**的库启动即可检索存量 + 后续增量无缝。
- 回写 13-sync（§4 落地）、12-pg（fetch_all）、14-engine（bootstrap_snapshot）、19-server（bootstrap 编排）、看板 B3 done。

## 8. 实施次序

1. `pg` `fetch_all_chunks` + 单测（SQL 形态）/集成。
2. `sync` `ensure_slot -> Option<Lsn>`（+ 更新调用点）。
3. `engine` `bootstrap_snapshot`。
4. `server` main bootstrap 编排。
5. Docker 集成测试（用例 1–4）+ 活服务手验；回写。

## 9. 状态

- [x] **已实施 + Docker PG 验证 done**（2026-06-25）。按 §8 全部落地：
  1. `pg::fetch_all_chunks`（全表读 (collection, Chunk)）；
  2. `sync::ensure_slot -> Option<Lsn>`（新建返回一致点 LSN）；
  3. `engine::bootstrap_snapshot`（逐行 apply_upsert 含嵌入 → persist）；
  4. `server` main：首启 + 新建 slot → fetch_all → bootstrap → 起 CDC 循环。
  5. 集成测试 `cdc_initial_snapshot_bootstrap`（Docker PG）：存量导入 + 无缝衔接增量 + ensure_slot 幂等；活服务验证：库里已有 2 行 → 启动日志 `bootstrap: imported 2` → 立即 keyword 检索命中（无新写入）。
- **实现中发现并修正的关键正确性问题**：`pg_logical_slot_peek` 的逐行 `lsn` 对一个事务的 Begin/Insert 报的是**事务起点**（首事务等于 slot 一致点）。原 `consume_once` 用 `Applier::new(consistent)` 做水位跳过会**误跳首批**。改为：**consume_once 不靠 LSN 水位跳过**，每拍 `Applier::new(Lsn(0))` 应用全部 peek 到的变更；正确性靠 ① slot 在 advance 前不重投 + ② 按 GlobalId upsert/delete **幂等**（崩溃重投同结果）。检查点改记 **slot 高水位**（含 Commit）。
- **未做（后续）**：超大表分页/流式快照（当前全量 fetch）；引擎并发去串行。
