# `/v1/index` 的 pgvector 写穿：与 `/v1/chunks` 对齐

> 日期：2026-08-24
> 状态：**已实施 + 活服务验证 done**（2026-08-24，Docker pgvector:pg16 真机）
> 来源：[FastGPT 知识库与多模态检索参考建议](2026-08-24-fastgpt知识库与多模态检索参考建议.md) §4.7 / P0-4
> 相关 spec：[19-server](../specs/19-server.md)、[12-pg](../specs/12-pg.md)、[14-engine](../specs/14-engine.md)

## 1. 要做什么

pgvector 直查档（B6）下，`POST /v1/index` 必须在 PG 真源写入成功后，把该次计算/携带的向量
**写回 PG `embedding` 列**（`set_embedding`），与 `POST /v1/chunks` 的既有行为一致。

## 2. 为什么（当前的真实故障）

服务端有两条写路径，pgvector 写穿只有一条做了：

| 写路径 | PG 真源 | PG `embedding` 写穿 | 引擎侧派生向量 |
|---|---|---|---|
| `/v1/chunks`（`batch_upsert_chunks`） | `upsert_chunks` | ✅ [lib.rs:2696-2713](../../crates/fastsearch-server/src/lib.rs#L2696-L2713) | ✅ |
| `/v1/index`（`index`） | `upsert_doc` | ❌ 无 | ✅ [lib.rs:2485-2497](../../crates/fastsearch-server/src/lib.rs#L2485-L2497) |
| CDC `apply_upsert` | —（回放） | ✅ [engine/src/lib.rs:1918-1929](../../crates/fastsearch-engine/src/lib.rs#L1918-L1929) | 不写（直查不读） |

后果（直查档读的是 PG `embedding`，不是引擎侧向量）：

1. `PgStore::upsert_doc` 是 **delete + insert 的 doc 级替换**
   （[pg/src/lib.rs:170-202](../../crates/fastsearch-pg/src/lib.rs#L170-L202)），
   新行的 `embedding` 必然是 `NULL`。
2. `/v1/index` 之后没有任何一步把向量写回 PG，`Engine::ingest_vector` 只写引擎侧本地向量库
   （[engine/src/lib.rs:1370-1388](../../crates/fastsearch-engine/src/lib.rs#L1370-L1388)），
   而直查档的读路径根本不看它。
3. ⇒ **刚 index 完的文档在向量检索里查不到**，直到 CDC 消费到这批变更并 `apply_upsert` 重新嵌入。
   CDC 没开 / 还没追上 ⇒ 一直查不到。对同一 doc 重复 index，更是把已可检索的向量清成 `NULL`。

（注：`upsert_chunk_sql` 的 `ON CONFLICT ... embedding = NULL` 是**有意**的正文-向量防错配设计，
不是 bug；`/v1/index` 走的 delete+insert 也同理。问题在于**写穿这一步缺席**。）

## 3. 怎么做

在 `index()` 中 `pg_arc.upsert_doc(...)` 成功之后、写引擎派生索引之前，照搬 `batch_upsert_chunks`
的写穿块：`has_pg_vector()` 为真时，对每个有向量的 chunk 调 `pg.set_embedding(...)`，
`embed_model` 标记沿用同一约定（携带预计算向量 → `api-precomputed`，服务端现算 → `api-embedder`）。

- **复用既有模式，不新造抽象**：两条路径的写穿语义必须一字不差地一致，这正是本次要消除的分歧。
- 引擎侧 `ingest_vector` **保持不变**（仍写本地派生向量）：与 `/v1/chunks` 一致；
  text 索引本来就要写，向量顺带写入在直查档下只是冗余、不影响正确性。

## 4. 不做什么（明确排除）

- **不做**"真源 + embedding 单事务原子"：`set_embedding` 是 `upsert_doc` 提交后的独立 UPDATE，
  中间崩溃会留下"真源已提交、embedding 未就绪"的窗口。这与 `/v1/chunks` 现状**相同**，
  本次只消除路径间分歧、不改一致性等级。原子化 = 新增 `upsert_doc_with_embeddings` 事务 API，
  单独立项（见 §7）。
- **不做**响应体的 `source_committed` / `embedding_ready` / `derived_index_visible` 三态区分
  （参考建议 §4.7 的更大提案）——那是契约变更，需要单独 spec。
- 不动 CDC 路径、不动 DDL、不动 `/v1/chunks`。

## 5. 用户使用例子

```bash
FASTSEARCH_VECTOR_BACKEND=pgvector DATABASE_URL=postgres://... \
  cargo run -p fastsearch-server --bin fastsearch-server
# index 后**立即**向量检索：修复前 0 命中（等 CDC），修复后立刻命中
fastsearch index --collection kb --doc-id r.pdf chunks.json
fastsearch search --collection kb --query "毛利率" --json
```

## 6. 测试用例（验收标准）

`crates/fastsearch-server/src/lib.rs` 测试模块，沿用既有 env-gated PG 集成测试形态
（`DATABASE_URL` 未设则跳过、独立表名、`multi_thread` runtime——直查档要求）：

1. `index_writes_embedding_through_to_pg_in_pgvector_mode`：
   - `PgStore` 建独立表、`with_vector_dim(3)`；`engine.set_source_store` + `set_pg_vector`。
   - `POST /v1/index` 灌入带图/带文的 chunk（`PairEmbedder`，dim=3）。
   - 断言 A：`POST /v1/search`（vector 模式）**立即**命中——不经任何 CDC。
   - 断言 B：直接查 PG，该行 `embedding IS NOT NULL` 且 `embed_model` 是约定标记。
   - 断言 C：对同一 doc 再 index 一次，仍命中（覆盖 delete+insert 后的重新写穿）。
2. 回归保持：`searchable_false_is_stored_in_pg_but_not_searchable`、
   `chunk_management_routes_enforce_acl_tenant_and_idempotency` 等既有 PG 测试仍绿。
3. 收口：`cargo fmt --all --check` + `cargo clippy --workspace --all-targets -- -D warnings`
   + `cargo test --workspace`；PG 部分需 `DATABASE_URL` 实跑，跑不到就在状态里标 `待运行验证`。

## 6.1 实际验收结果（2026-08-24）

- 集成测试 `index_writes_embedding_through_to_pg_in_pgvector_mode` 对**真实 pgvector:pg16** 实跑绿；
  **去掉写穿即红**（直查 0 命中），证伪力已验证。
- **§6 的"断言 B"（直查 PG 确认 `embedding IS NOT NULL` 且 `embed_model` 为约定标记）
  未进自动化测试**——`PgStore` 没有暴露读 embedding 的公开 API，为此加一个仅测试用的读接口
  不值当。该断言改由下面的活服务验证以 `psql` 直查真源覆盖，**如实记账，不假装自动化已覆盖**。
- 收口：fmt 净、clippy `-D warnings` 0 告警、`cargo test --workspace` 343 passed / 0 failed。
- **活服务验证**（实跑 `fastsearch-server` 二进制，`FASTSEARCH_VECTOR_BACKEND=pgvector` +
  `DATABASE_URL`，**`FASTSEARCH_CDC` 未置**——即"CDC 不参与"，正是此前查不到的场景）：

  1. 启动日志确认档位：`vector backend: pgvector 直查（ANN 在 PG，需 embedding 已入 PG）`。
  2. `POST /v1/index` 两条 chunk → `psql` 直查真源：两行均 `embedding IS NOT NULL`、
     `embed_model = api-precomputed` ✅（覆盖断言 B）。
  3. 立即 `POST /v1/search`（纯向量）→ 命中 `[3, 1]` ✅ 不经任何 CDC。
  4. **对同一 doc 重复 `/v1/index`**（此前会把 embedding 清成 NULL）→ 两行仍 `NOT NULL`、
     检索仍命中 ✅ 覆盖 delete+insert 后的重新写穿。

## 7. 下一迭代

- `upsert_doc_with_embeddings`：真源行与 embedding 同一事务提交，消除"真源已提交、向量未就绪"窗口。
- 写入响应显式三态（source / embedding / derived），让调用方能等到"可检索"而不是"已收下"。
