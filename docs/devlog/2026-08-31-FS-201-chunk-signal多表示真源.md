# FS-201 `chunk_signal` 多表示真源开发日志

> 日期：2026-08-31  
> 状态：已完成  
> 规格：[`chunk_signal` 多表示设计](../plans/2026-08-25-chunk-signal多表示设计.md)

## 本轮交付

- 在 `fastsearch-core` 定义 `SignalType` / `SignalStatus` / `Signal`，五种 signal 的正文与工件绑定关系使用穷尽 `match`。
- `fastsearch-pg` 从 chunks 表名派生 `{table}_signal`，校验表名及 `_doc`/`_worklist` 索引名，additive 创建无外键、无 ACL 副本、无 ANN 的 `real[]` 真源表。
- 实现 `upsert_signal` / `fetch_signals` / `set_signal_embedding` / `orphan_signal_count`；`body_hash` / `artifact_hash` / `embedding_dim` 由 SQL 从 chunks 权威列派生，不信任调用方传值。
- 在原有 chunk 写删事务中挂接精确 stale、doc 收敛和删除回收；作废保留 model/version/text/hash/error 审计证据，只清向量与维度。
- `fastsearch_pub` 继续为 chunks-only；FS-101 表名/列形状白名单作为误配时的第二道防线。

## 审查中的改进

- 首次全量门禁发现 publication 契约测试与其他 PG 用例共库并行时可产生关系锁死锁；该用例改在独立临时数据库内重置 publication，完整门禁重跑后稳定通过。
- 双轴 review 发现派生表名可合法但再加索引后超过 PG 63 字节；现已对全部派生 identifier 做连锁校验并补临界负例。
- 补强 T3/T4：重复未变 chunk 时所有 signal `updated_at` 不变；工件变更时 artifact-bound 向量必须清空，`user_text` 的向量与时间戳不变。
- 将 chunk 删除后的信号回收提取为事务 helper，降低后续删除入口遗漏配对清理的风险。

## 验收结果

- `cargo fmt --all -- --check`：通过。
- `cargo clippy --workspace --all-targets -- -D warnings`：通过。
- `scripts/ci/run_environment_gates.sh --require-pg`：完整 workspace 测试通过；PG/CDC `executed=32 skipped=0 missing=0`。
- 真实模型未配置；与 FS-201 无关的模型门禁显式记账 `executed=0 skipped=3 missing=0`。
- Standards / Spec 双轴初审问题均已修正，最终复审通过。

## 边界与下一步

- FS-201 只交付真源、审计和重建工作面，不改 Engine 检索排序，不增 REST/MCP 入口。
- FS-202 再将多信号向量接入实际 N 路召回与 `SearchHit.sources` model/version provenance。
