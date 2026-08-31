# FS-101：PK UPDATE 与 TRUNCATE 语义

> 日期：2026-08-31
> 状态：完成
> 对应计划：[FS-101](../plans/2026-08-30-迭代开发计划.md#fs-101-pk-update-与-truncate-语义--2026-08-31)

## 1. 本轮结果

FS-101 补齐逻辑复制在主键迁移和整表清空时的派生索引收敛语义：

- UPDATE 同时解析旧 key/tuple 与新 tuple；主键变化产生有序的 `Delete(old)`、`Upsert(new)`。
- 同一个 WAL 事件映射出的多条变更保留在同一 LSN 的 `Change::Batch` 中，全部成功后才推进水位。
- 真源表 TRUNCATE 映射为 `Change::Clear`，Engine 同时清空 text/vector 派生索引。
- relation 先按配置的精确 `schema.table` 白名单过滤，再做必要列形状校验，同形旁表不会误删或误写 chunk。

本项没有扩大为 FS-102：`Batch` 表达同 LSN 的确定顺序和重试边界，但当前 sink 的跨 text/vector 事务原子性仍由下一项完成。

## 2. 数据流与失败边界

```text
pgoutput UPDATE(old key, new tuple)
  -> map: [Delete(old), Upsert(new)]
  -> Change::Batch (one source LSN)
  -> sink applies in order
  -> sink commit
  -> persist/advance LSN

pgoutput TRUNCATE(source oid)
  -> Change::Clear
  -> clear text + vector
  -> sink commit
  -> persist/advance LSN
```

映射阶段会先完成新 tuple 的解析/必要回源，再返回 delete + upsert，确定性映射错误不会只留下 delete。应用阶段任一步失败都不推进水位，重试依靠 delete/upsert/clear 的幂等语义收敛；真正消除跨索引半状态属于 FS-102。

## 3. 配置与兼容性

`ReplicationConfig` 新增 `source_table`，server 通过 `FASTSEARCH_CDC_SOURCE_TABLE` 配置，默认值是 `public.fastsearch_chunks`。

这是刻意的精确白名单，而不是仅看 relation 列形状。当前 publication 只有一个 chunks 真源表；如未来支持多个真源表，应把配置显式扩展为表集合，不能回退为形状猜测。

`Change::Upsert` 和 `Change::Delete` 的既有调用方式保持不变；`Clear` 与 `Batch` 是加法扩展。普通未改变主键的 UPDATE 仍只产生一个 upsert。

## 4. TDD 与实库验收

RED 阶段先加入以下契约测试，因尚无 `Clear`、`Batch`、`apply_clear` 和零到多映射而出现 17 个预期编译错误：

- PK 更新必须保持 delete-old → upsert-new 顺序且共用一个 LSN。
- 普通 UPDATE 不得产生多余 delete。
- 真源 TRUNCATE 必须映射 clear。
- 同形但不同表名的 relation 必须被忽略。
- clear 必须清空 sink。

GREEN 阶段结果：

- `fastsearch-sync` 单测：23/23 通过。
- `cdc_closed_loop`：6/6 通过；其中新增实库用例验证 PK 三字段迁移及 TRUNCATE 后 keyword/vector/hybrid 全部为空。
- Docker PostgreSQL 17 + pgvector 使用 `wal_level=logical`，不是纯函数替代。
- 环境门禁：`pg executed=21 skipped=0 missing=0`；模型端点未配置，`model executed=0 skipped=3 missing=0` 如实记账。
- workspace 全量测试、`cargo fmt --all --check` 与 workspace clippy `-D warnings` 通过。

## 5. 后续项

FS-102 继续处理 `IndexSink` 跨 text/vector 的原子提交、批量嵌入与故障注入；FS-103 再统一 commit LSN、水位持久化、slot 并发首建和运行健康指标。本轮复合变更结构为这两项提供边界，但不替代它们的验收。
