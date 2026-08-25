# KB-2.1 `chunk_signal` 多表示子表设计

> 日期：2026-08-25 · 状态：**设计（未实施）**——本文只产出设计与验收，不改 `crates/` 下任何代码
> （[迭代计划 §11.3-3](2026-08-24-知识库引擎迭代计划.md)）。本机**无 `DATABASE_URL`**，
> 一切涉 PG 的结论一律标 `待运行验证`。
> 上游条目：[知识库引擎迭代计划 §4 KB-2.1](2026-08-24-知识库引擎迭代计划.md)（含两条"必答的既有约束"）
> 来源：[FastGPT 参考建议 §5.2-2 复核补充 / §P1-1](2026-08-24-fastgpt知识库与多模态检索参考建议.md)
> 相关 spec：[12-pg](../specs/12-pg.md)、[13-sync](../specs/13-sync.md)、[10-core](../specs/10-core.md)
> 通道：**C4（存储/作业）**——本文独占迭代计划 §10 待决策 **#4（正文变更作废哪些信号）**
> 与 **#5（`SET TABLE` 替换语义）** 的定夺权；KB-3.1（`ingest_job`）只能引用本文结论，不得自行定夺。

---

## 0. 本文的两条硬性结论（其他作业请直接引用这两段）

**结论 A（迭代计划 §10 待决策 #5）：`fastsearch_pub` **永远单表**，只发布 `fastsearch_chunks`。
DDL 保持现有 `SET TABLE` 不改。任何第二张表（`chunk_signal`、`ingest_job`……）
**都不进 `fastsearch_pub`**；将来若真需要给某张新表做 CDC，走**另建 publication**，
而不是把 `fastsearch_pub` 改成 `ADD/DROP` 收敛。** 完整论证见 §3。

**结论 B（迭代计划 §10 待决策 #4）：作废规则不按"正文变了"这句话写，按**每个信号所断言的关系**写，
逐 `signal_type` 定死为一张两列真值表（body-bound / artifact-bound），落成 Rust 侧穷尽 `match`。**
规则表见 §5.2。

---

## 1. 要做什么

新增一张 PG 子表 `fastsearch_chunk_signal`，让**一个 chunk 能诚实地持有多路表示/信号**，
每一路都知道自己**来自谁、用什么模型、什么版本、当前什么状态、输入工件是什么**。

- 键：`(collection, doc_id, chunk_id, signal_type)`——即 `GlobalId` + 信号类型。
- `signal_type` 首批覆盖：`user_text` / `ocr` / `asr` / `vlm_caption` / `image_bytes`。
- 列：`model` / `model_version` / `status` / `artifact_hash` / `body_hash` / `signal_text` /
  `embedding` / `embedding_dim` / `error` / `updated_at`。
- 配套：写路径（产信号、写向量、按规则作废）、删除路径的同事务回收、审计/重建工作队列查询。

**本迭代的边界：信号表只写不读（真源 + 审计 + 重建工作队列），检索读路径零改动。**
让信号真正参与召回是 KB-2.2（N 路具名融合）的事，本文只负责把"真源里存得下、审计得清、
作废得对、CDC 不炸"这四件事做对。

## 2. 为什么

1. **今天 PG 只有单 `embedding` + `embed_model` 列**（`crates/fastsearch-pg/src/sql.rs` 的 `ddl`
   与 `COLUMNS`）。"这一路向量到底是正文的、caption 的还是原图的"在真源里**无法区分**
   ⇒ 无法做 KB-2.2 的具名融合、无法审计、无法只重建其中一路。
2. **状态只覆盖了一路**：`fastsearch-core/src/model.rs` 的 `ImageVectorStatus`
   （`pending/embedded/text_fallback/missing_bytes/asset_missing/error`）落在 `chunks.image_vector_status`
   一列上，只描述"图片视觉向量"这一路。caption/OCR/ASR 各自的状态今天**没有地方放**。
   L2「表示完备」= 每种表示都知道来源、模型、版本、状态——这正是缺的那块。
3. **"降级"今天是一个状态值，而不是一个事实**：`ImageVectorStatus::TextFallback` 把
   "视觉这路没成、退回文本这路"压缩成单列的一个枚举值。多信号模型里它自然分解为
   "`image_bytes` 信号 `failed` + `user_text` 信号 `ready`"——**两个独立可观测的事实**，
   不需要一个混合状态。这是多表示模型顺带买到的东西。

## 3. 跨通道决策：`ALTER PUBLICATION … SET TABLE` 的替换语义（硬性交付物）

### 3.1 事实

`crates/fastsearch-pg/src/sql.rs` 的 `ddl` 里，publication 收敛分支是：

```
ALTER PUBLICATION fastsearch_pub SET TABLE {table} ({collist});
```

`SET TABLE` 是**替换**语义：它把 publication 的整张表清单换成 `{table}` 一张。
`PgStore::ensure_schema`（`crates/fastsearch-pg/src/lib.rs`）在**每次 server 启动**时都会跑这段幂等 DDL
（且整段包在事务 + `pg_advisory_xact_lock` 里）。
⇒ **任何在别处 `ALTER PUBLICATION … ADD TABLE` 进来的第二张表，会在下次 server 启动时被静默移除。**

### 3.2 但真正的风险不是"被移除"，而是"没被移除"——一条新发现的误删链路

比"表被悄悄踢出去"严重得多的是**表真的留在流里**。读
`crates/fastsearch-sync/src/replication.rs` 的 `map` / `row_to_chunk` / `row_to_gid` 后确认：

- `map` 只按 `rel_oid` 去 `relations` 缓存里取 `Relation`，**从不校验 `rel.name`**——
  publication 里有几张表，解码器就把几张表的行**一视同仁当 chunk 行**处理。
- `Insert`/`Update` → `row_to_chunk`，它 `get(&m, "kind")` / `get(&m, "text")` 取不到列就报错。
  该错误被 `decode_batch_lenient` 当**确定性毒丸**吞掉：计入死信、打一行 stderr、
  **slot 照常推进**。即：一张误入流的新表会让 CDC 持续刷死信日志，而不是显式失败。
- `Delete` → `row_to_gid`，它**只需要 `collection` / `doc_id` / `chunk_id` 三列**。
  而 `chunk_signal` 的主键**正好含这三列** ⇒ 一条 `chunk_signal` 行的 DELETE
  会被映射成 `Change::Delete { gid }`，经 `Applier` 打到 `IndexSink`，
  **把整个 chunk 从全文 + 向量派生索引里删掉**。

⇒ 结论：**"新表不进 publication"不是保守，是当前解码器的正确性前提。**
在 `map` 具备 relation 白名单之前，任何新表进流都会造成"静默毒丸 + 误删 chunk"。
这条也顺带说明：`SET TABLE` 的替换语义在今天**恰好是一道安全网**——它会把误加的表踢出去。

### 3.3 决策：选 A —— `fastsearch_pub` 永远单表

| 方案 | 内容 | 判断 |
|---|---|---|
| **A（采纳）** | `fastsearch_pub` 专属 `fastsearch_chunks`，DDL 保持 `SET TABLE`；第二张表要 CDC 就**另建 publication** | ✅ |
| B（否决） | 把 DDL 改成 `ADD TABLE` / `DROP TABLE` 精确收敛，允许多表共存于 `fastsearch_pub` | ❌ |

**选 A 的理由（按权重排序）**：

1. **B 会把 §3.2 的误删链路从"不可能"变成"一次配置失误就发生"。** A 保留了那道安全网。
2. **B 需求为零**：`chunk_signal`（本文）与 `ingest_job`（KB-3.1，红线 2）**都明确不需要进流**。
   为一个没有需求的能力，去改一段已经真机验证过、且带并发首建 `EXCEPTION` 守卫与
   自愈-不抢占逻辑的 DDL，收益为负。
3. **B 在 PG 语法上并不"更精确"，反而更脆**：`ALTER PUBLICATION` 一条语句只接受一个动作
   （`ADD` / `SET` / `DROP`）。要在**保留其他表**的前提下把 `fastsearch_chunks` 的**列清单**
   收敛到新的 `COLUMNS`（这正是现有 `SET TABLE` 分支存在的理由——让 additive 源列进入既有部署的 CDC），
   就必须 `DROP TABLE` + `ADD TABLE (collist)` 两条语句，并依赖它们在同一事务内原子生效。
   而"一张表短暂不在 publication 中对并发逻辑解码的影响"是 `[待验证]` 的（本机无 PG）。
   A 一条 `SET TABLE` 就完成收敛，无窗口、无待验证项。
4. **逃生门已经存在，且不需要动 DDL**：CDC 消费侧
   （`crates/fastsearch-sync/src/replication.rs` 的 `fetch_changes`）把
   `cfg.publication` 原样内插进 `'publication_names', '{pubn}'`，而 pgoutput 的
   `publication_names` 本就是**逗号分隔的多值参数**。`ReplicationConfig.publication`
   由 server 从 `FASTSEARCH_CDC_PUBLICATION` 读取（`crates/fastsearch-server/src/main.rs`）。
   ⇒ 将来真要给第二张表做 CDC：**另建一个 publication + 把 env 设成 `"fastsearch_pub,fastsearch_signal_pub"`**，
   `fastsearch_pub` 一行都不用改。
   `[待验证]` 该逗号多值形态仓内**无测试覆盖**，且 `esc()` 只转义单引号、逗号原样透传——
   真要用时必须先补一个 env-gated 集成测试。
5. **升级影响为零**（见 §3.5），而 B 要求所有既有部署重跑一遍变更过的 DDL 路径。

### 3.4 A 的配套约束（缺一不可，进验收清单）

1. **把隐性契约写成显式契约**：在 `crates/fastsearch-pg/src/sql.rs` 的 `PUBLICATION` 常量处
   写明"本 publication 专属 `fastsearch_chunks`，永不多表；第二张表另建 publication"。
2. **纯函数单测钉死单表**：断言 `ddl()` 生成的 publication 语句中出现的表名有且只有入参 `table`。
3. **集成测试把"静默移除"从 bug 升格为契约**（`待运行验证`）：手工
   `ALTER PUBLICATION fastsearch_pub ADD TABLE fastsearch_chunk_signal` →
   再跑一次 `ensure_schema` → 断言 `pg_publication_rel` 里只剩 `fastsearch_chunks`。
   **这是期望行为，不是缺陷。**
4. **给未来的第二张表留下唯一合法路径**：若哪天真要让 `chunk_signal` 进 CDC，
   **前置条件是先给 `fastsearch-sync::replication::map` 加 relation 白名单**
   （按 `rel.name` 判定，非 chunks 表的消息一律 `Ok(None)` 消化掉），
   否则 §3.2 的误删链路立即成立。这条写进 13-sync 的"下一迭代"。

### 3.5 对既有部署的升级影响

- **DDL 零变更 ⇒ 既有部署升级影响为零**：不需要重启 PG、不需要重建 slot、不需要重放、
  不需要 `DROP PUBLICATION`。新增的只有一张空表（`CREATE TABLE IF NOT EXISTS`）和两个索引。
- **唯一需要主动告知的人群**：曾**手工**往 `fastsearch_pub` 里 `ADD TABLE` 过自有表的部署方。
  他们的表在下次 server 启动时会被移除——**这是现状行为，不是本次引入的**，
  本次只是把它写进文档并加测试钉死。给他们的迁移指引就是 §3.3-4 的逃生门（另建 publication）。
- 回滚：`DROP TABLE fastsearch_chunk_signal;` 即可，`fastsearch_chunks` 与 publication 零改动（见 §9）。

### 3.6 给 KB-3.1（`ingest_job`）的结论引用

KB-3.1 红线 2 要求的"先解决 `SET TABLE` 替换语义"**已由本文解决**：
`ingest_job` **不进 `fastsearch_pub`**，理由与本表完全相同（§3.2 的误删链路对
`ingest_job` 同样成立——只要它有 `collection`/`doc_id` 列且被误加进流）。
KB-3.1 的 spec 直接引用本节即可，**不得重新定夺**（迭代计划 §11.1 的归属规定）。

---

## 4. 表结构

### 4.1 DDL（拟）

由 `crates/fastsearch-pg/src/sql.rs` 新增纯函数 `signal_ddl(sig_table, chunks_table, vector_type, vector_dim)`
生成，并入 `ddl()` 返回的语句序列（`ensure_schema` 已把整段包进事务 + advisory lock，无需额外并发处理）。

```sql
CREATE TABLE IF NOT EXISTS fastsearch_chunk_signal (
  collection      text   NOT NULL,
  doc_id          text   NOT NULL,
  chunk_id        bigint NOT NULL,
  signal_type     text   NOT NULL,          -- user_text/ocr/asr/vlm_caption/image_bytes
  status          text   NOT NULL DEFAULT 'pending',
  model           text,                     -- 产出该信号的模型标识（如 "bge-m3"）
  model_version   text,                     -- 模型/权重版本；与 model 分列，便于"只重建某版本的某一路"
  artifact_hash   text,                     -- 该信号**输入工件**的内容标识（见 §5.1）
  body_hash       text,                     -- 产出该信号时 chunks.text 的 md5（body-bound 失效判据）
  signal_text     text,                     -- 该路产出的文本表示（caption/OCR/转写）；无文本则 NULL
  embedding       real[],                   -- 该路向量（真源；可空。类型选择见 §4.3）
  embedding_dim   integer,                  -- 实际维度（冗余，供写入前后校验与"维度错配"审计）
  error           text,                     -- status='failed' 时的原因（L4 可观测）
  updated_at      timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (collection, doc_id, chunk_id, signal_type)
);
CREATE INDEX IF NOT EXISTS fastsearch_chunk_signal_doc
  ON fastsearch_chunk_signal (collection, doc_id);
CREATE INDEX IF NOT EXISTS fastsearch_chunk_signal_worklist
  ON fastsearch_chunk_signal (signal_type, status);
```

对齐既有风格的几处刻意选择：

- **表名跟随主表**：`{chunks_table}_signal`（默认 `fastsearch_chunks_signal`）或固定
  `fastsearch_chunk_signal`——`PgConfig.table` 可配（`crates/fastsearch-pg/src/lib.rs`），
  故信号表名必须**由主表名派生**，并同样过 `validate_identifier`。`[待决策 D1]`：派生规则取
  `format!("{table}_signal")`（简单、随主表隔离多部署）——推荐这条，本文按它写。
- **不加 `CHECK` 约束**：与 `chunks.kind`（`text` 列 + Rust 侧 `kind_from_str` 报错）保持一致的
  惯例——词表在 Rust 侧收口，加新 `signal_type` 不需要 schema 迁移。
- **不加外键**：见 §5.4，`upsert_doc` 的 doc 级 delete+insert 会让 `ON DELETE CASCADE`
  **每次重新索引都毁掉全部视觉/caption 信号**——正是本文要防的事。
- **不建 ANN 索引、不设 `REPLICA IDENTITY`**：本迭代信号表不参与检索、不进复制流。

### 4.2 `signal_type` 与 `status` 词表（落在 `fastsearch-core`）

放 `crates/fastsearch-core/src/model.rs`，与 `ChunkKind` / `Modality` / `ImageVectorStatus`
同款（serde `snake_case` + `as_str()`）：

```rust
pub enum SignalType { UserText, Ocr, Asr, VlmCaption, ImageBytes }
pub enum SignalStatus { Pending, Ready, Stale, Failed, Unsupported }
```

- `SignalType` 用**穷尽 `match`** 实现 §5.2 的规则表 ⇒ 将来加一个 `signal_type`，
  编译器强制作者回答"它随正文废还是随工件废"，**漏答不给编过**。这是本设计最重要的
  防腐措施，别用 `HashMap`/字符串表替代。
- `SignalStatus` 与 `ImageVectorStatus` 的对应：`Embedded→Ready`、`Pending→Pending`、
  `Error/MissingBytes/AssetMissing→Failed`（原因进 `error` 列）、
  `TextFallback→` **不需要**（分解为两条信号行，见 §2-3）。

### 4.3 `embedding` 的类型：为什么是 `real[]` 而不是 `halfvec(N)`

pgvector 的 `vector`/`halfvec` 列**必须有固定 typmod 才能建 ANN 索引**，而多信号天然多维
（文本 384/768、视觉 512/1024……）。若把信号表的 `embedding` 绑成 `halfvec(PgConfig::vector_dim)`，
第一个真实的视觉信号就**写不进真源**（维度不符被 pgvector 拒收）——
直接破坏不变量 #2（PG 是真源、派生可重建）。

⇒ 本迭代取 **`real[]`**（PG 核心数组类型，IEEE single 无损，任意维，零扩展依赖）：

- 保住"真源存得下任意一路向量、派生索引可从 PG 重建"；
- 明确**不承诺**在信号表上做 ANN——本迭代信号表不进读路径，不需要；
- `[待决策 D2]`：等 KB-2.2 真要在 PG 直查某一路时，再在**那时**选
  "按 dim 各开一个 typed 列 + 各自 HNSW" / "按 `signal_type` 分区表" / "只在引擎侧持多路向量"。
  两条约束先记下：① 只能用 pgvector + 普通表能力（不变量 #1）；
  ② 任何 typed 列都要有对应 opclass（`ann_index_sql` 的 `cosine_opclass` 已有先例：
  halfvec 列必须用 `halfvec_cosine_ops`）。

---

## 5. 作废规则（迭代计划 §10 待决策 #4 的定夺）

### 5.1 先把"作废"这件事的判据从动词换成名词

"正文变了作废旧向量"是现有 `upsert_chunk_sql`（`ON CONFLICT … embedding = NULL, embed_model = NULL`）
与 `upsert_doc`（事务内 `delete_doc_sql` + `insert_sql`，新行 `embedding` 必为 NULL）的设计意图，
**这条意图必须保留**——它防的是"正文已换、向量还是旧的"这种错配。

但读代码后必须先说清现状的一个事实：**现有实现作废的判据不是"正文变了"，而是"又写了一次"**。
`upsert_chunk_sql` 的 `DO UPDATE` 是**无条件**的（只有 tenant 守卫），
即使新旧 `text` 一字不差，`embedding` 也会被清成 NULL。
`crates/fastsearch-server/src/lib.rs` 的 `/v1/index` 处理里也留了这条注释的痕迹
（"重复 index 同一 doc 更会把已可检索的向量清成 NULL"，靠随后的 `set_embedding` 写穿兜住）。

对单路 384 维文本向量，"多算一次"是可以接受的代价；
对 **VLM caption / 视觉向量**（一次外部模型调用、有真金白银成本）就**不可接受**。
所以多信号之后，判据必须是**内容 hash 比较**，不是"写了没写"。

两个 hash（都用 PG 内建 `md5()`，**零 Rust 新依赖、零 PG 扩展**；
写侧与审计侧共用同一段 SQL 表达式，由纯函数生成，保证两端同源）：

| 判据 | 定义 | 说明 |
|---|---|---|
| `body_hash` | `md5(chunks.text)` | 该信号产出时正文的内容标识 |
| `artifact_hash` | `COALESCE(md5(c.media_bytes), md5((c.media -> 'asset')::text))` | inline 字节优先；否则取 `MediaRef.asset` 的规范化 jsonb 文本（Object URI / DocRegion 的 page+bbox） |

为什么用 `md5()` 而不是 Rust 侧 sha256：`md5(text)` 是 **PG 核心内建函数**（非 `pgcrypto` 扩展，
守不变量 #1），因此**外部直接写 PG 的调用方绕过我方代码改了正文时，审计查询依然能算出漂移**
（见 §5.5）。这里 hash 只用于变更检测，不是安全原语。
`[待验证]` FIPS 模式的 PG 构建可能禁用 md5；托管 PG（RDS/Supabase/Neon）默认不启用该模式。
`[待验证]` `(jsonb)::text` 的规范化输出（键序、空白）跨 PG 版本稳定——预期稳定，本机无 PG 无法确认。

### 5.2 规则表（**本文的第二条硬性结论**）

一个信号在**它所断言的关系被打破时**作废。逐 `signal_type` 定死：

| `signal_type` | 该信号断言什么 | 正文变（`body_hash` 漂移） | 工件变（`artifact_hash` 漂移） | `model`/`model_version` 变 |
|---|---|---|---|---|
| `user_text` | `embedding = f(chunks.text)` | ✅ **作废** | — （不绑工件） | ✅ 作废 |
| `ocr` | `chunks.text == OCR(工件)` 且 `embedding = f(该文本)` | ✅ **作废** | ✅ 作废 | ✅ 作废 |
| `asr` | `chunks.text == ASR(工件)` 且 `embedding = f(该文本)` | ✅ **作废** | ✅ 作废 | ✅ 作废 |
| `vlm_caption` | `signal_text == VLM(工件)` 且 `embedding = f(signal_text)` | ❌ **不作废** | ✅ 作废 | ✅ 作废 |
| `image_bytes` | `embedding = 视觉模型(工件字节)` | ❌ **不作废** | ✅ 作废 | ✅ 作废 |

落成两个集合（Rust 侧穷尽 `match`，见 §4.2）：

- **body-bound**（正文变即废）：`user_text`、`ocr`、`asr`
- **artifact-bound**（工件变即废）：`ocr`、`asr`、`vlm_caption`、`image_bytes`

对迭代计划里那句提示的精确化——**"OCR 信号随正文废、视觉信号未必"是对的，但理由不是"OCR 是文本"**：
`ocr`/`asr` 的产物**就是**正文（`Chunk.text` 的注释写明它是"可检索文本表示（正文 / caption / 转录）"），
所以别人改了正文，等于**证伪了这条信号的断言**（"这段正文来自对该工件的 OCR"）——它必须废。
而 `vlm_caption` 的产物落在**信号自己的 `signal_text`** 里、`image_bytes` 的产物是向量，
两者都不以正文为断言的一部分 ⇒ 正文换了它们照样成立。

**"自己写自己不算变"是自动成立的**，不需要"谁写的"这类 provenance：
重跑 OCR 的写路径在**同一事务**里既写 `chunks.text` 又写 `ocr` 信号行（`body_hash` 一并刷新），
比较发生在事务提交后的状态上，所以它自己写进去的那条不会被判漂移。
**作废是一次纯比较，不是一条规则引擎。**

### 5.3 作废动作：标 `stale` + 清 `embedding`，**不删行**

```
status = 'stale', embedding = NULL, embedding_dim = NULL, updated_at = now()
```

保留行（而不是 `DELETE`）的三个理由：
① 审计——"这一路曾经存在、因何作废"是 L2/L4 要的；
② `stale` 行天然就是**重建工作队列**（§6 的例子）；
③ 删行会让"这个 chunk 到底试没试过视觉这一路"和"从来没试过"混为一谈。

`signal_text` / `model` / `artifact_hash` **保留原值**（它们是"上一版是什么"的证据），
只有 `embedding` 与 `status` 变。

### 5.4 写路径与删除路径的挂接

新增纯 SQL 函数（`crates/fastsearch-pg/src/sql.rs`，与既有 `upsert_chunk_sql` 等同款可单测）
+ `PgStore` 方法（`crates/fastsearch-pg/src/lib.rs`）：

| 纯函数 | 挂在哪 | 作用 |
|---|---|---|
| `upsert_signal_sql` | `PgStore::upsert_signal` | 写/更新一条信号（`body_hash`/`artifact_hash` 由 SQL 侧 JOIN chunks 现算，写侧不传 hash） |
| `set_signal_embedding_sql` | `PgStore::set_signal_embedding` | 只写向量 + `status='ready'`，**带 `IS DISTINCT FROM` 幂等守卫**（见 §7） |
| `stale_body_bound_signals_sql` | `upsert_chunks` / `upsert_doc` **同事务内** | body-bound 且 `body_hash IS DISTINCT FROM md5(c.text)` → 标 stale |
| `stale_artifact_bound_signals_sql` | 同上 | artifact-bound 且 `artifact_hash` 漂移 → 标 stale |
| `reconcile_doc_signals_sql` | `upsert_doc` **同事务内** | `DELETE … WHERE collection=$1 AND doc_id=$2 AND chunk_id <> ALL($3::bigint[])`，回收"这次没再出现的 chunk_id"的信号 |
| `delete_doc_signals_sql` / `delete_collection_signals_sql` / `delete_chunk_signals_sql` | `delete_doc` / `delete_collection` / `delete_chunks_visible` **同事务内** | 跟随主表删除回收信号；`delete_chunks_visible` 按 `RETURNING` 实际删掉的 id 回收（ACL 判定只发生一次，不复制第二套） |
| `orphan_signals_sql` | 审计/测试 | `LEFT JOIN` chunks 找孤儿，正常应恒为 0 行 |

作废语句的形态（示意，`$3` = body-bound 类型数组）：

```sql
UPDATE {sig} AS s SET status = 'stale', embedding = NULL, embedding_dim = NULL, updated_at = now()
FROM {chunks} AS c
WHERE c.collection = s.collection AND c.doc_id = s.doc_id AND c.chunk_id = s.chunk_id
  AND s.collection = $1 AND s.doc_id = $2
  AND s.signal_type = ANY($3::text[])
  AND s.body_hash IS DISTINCT FROM md5(c.text)
  AND (s.status <> 'stale' OR s.embedding IS NOT NULL)   -- 幂等：重复执行 0 行
```

**关于 `upsert_doc` 的 doc 级 delete+insert**：它对 `fastsearch_chunks` 的语义**保持不变**
（CDC 侧继续看到 delete+insert，13-sync 的替换语义不受影响）。
信号表**不设外键**，所以主表行被删再插时信号行原地不动——
随后由 `stale_*` 与 `reconcile_doc_signals_sql` 在同一事务内做精确收敛。
**这是本设计避免"每次重新索引都毁掉全部视觉信号"的关键**：
若改用 `FOREIGN KEY … ON DELETE CASCADE`，每一次 `/v1/index` 重放都会连带删光
`vlm_caption`/`image_bytes`——正好是 Q1 要求保住的那一半。
代价是**引用完整性由代码保证**，故 `orphan_signals_sql` 必须进集成测试（§8）。

### 5.5 `chunks.embedding` 的既有作废行为要不要一起收紧

`[待决策 D4]`：把同样的 `body_hash` 守卫用到 `upsert_chunk_sql` 的
`embedding = NULL`（即正文真没变时**不清**主 embedding）。

- 赞成：省掉重复 index 同一 doc 的整轮重嵌；与信号表规则同源、一套心智。
- 反对：它改动的是**已真机验证过的热写路径**，且 `/v1/index` 与 `/v1/chunks` 两条路径
  都靠随后的 `set_embedding` 写穿兜住，收益不如信号表那边大。
- **本文建议：不在 KB-2.1 一起做**，单列一条小改动、单独收口。保守是为了让本项的
  失败面只落在新表上（§9 的回滚才干净）。

---

## 6. 用户使用例子

本迭代**没有新 REST 端点**（信号表只写不读，见 §1）。使用面是库 API + 运维 SQL：

```rust
// 摄取侧：同一个 image chunk 同时挂 caption 与视觉两路信号（KB-2.1 的验收场景）
pg.upsert_signal(&Signal {
    gid: gid.clone(), signal_type: SignalType::VlmCaption,
    status: SignalStatus::Ready, model: Some("ovis-ocr2".into()), model_version: Some("v2".into()),
    signal_text: Some("2024 年毛利率趋势折线图".into()), embedding: Some(caption_vec), ..Default::default()
}).await?;
pg.upsert_signal(&Signal {
    gid, signal_type: SignalType::ImageBytes,
    status: SignalStatus::Ready, model: Some("siglip".into()), model_version: Some("so400m".into()),
    signal_text: None, embedding: Some(visual_vec), ..Default::default()
}).await?;
// body_hash / artifact_hash 由 SQL 侧 JOIN chunks 现算，调用方不传（两端同源，避免第二套判据）
```

```sql
-- 运维：这个 chunk 现在有哪几路、各是什么状态、谁产的
SELECT signal_type, status, model, model_version, embedding_dim, left(signal_text, 40), updated_at
FROM fastsearch_chunks_signal
WHERE collection = 'kb' AND doc_id = 'r.pdf' AND chunk_id = 152;
--  vlm_caption | ready | ovis-ocr2 | v2       | 1024 | 2024 年毛利率趋势折线图 | …
--  image_bytes | ready | siglip    | so400m   |  768 |                        | …
--  user_text   | stale | bge-m3    | v1.5     | NULL |                        | …   ← 正文刚被改过

-- 重建工作队列：只重建 caption 这一路（"无法只重建其中一路"正是本表要解决的问题）
SELECT collection, doc_id, chunk_id FROM fastsearch_chunks_signal
WHERE signal_type = 'vlm_caption' AND status IN ('stale', 'pending', 'failed')
ORDER BY collection, doc_id, chunk_id LIMIT 500;

-- 审计：外部写入方绕过我方代码改了正文导致的漂移（md5 是 PG 内建，故这条查询自带）
SELECT s.collection, s.doc_id, s.chunk_id, s.signal_type FROM fastsearch_chunks_signal s
JOIN fastsearch_chunks c USING (collection, doc_id, chunk_id)
WHERE s.signal_type IN ('user_text','ocr','asr') AND s.body_hash IS DISTINCT FROM md5(c.text)
  AND s.status <> 'stale';

-- 孤儿检查（无外键 ⇒ 必须能查；正常恒为 0 行）
SELECT COUNT(*) FROM fastsearch_chunks_signal s
LEFT JOIN fastsearch_chunks c USING (collection, doc_id, chunk_id) WHERE c.collection IS NULL;
```

---

## 7. CDC 反馈环：新表怎么办

### 7.1 现状的阻尼在哪

`crates/fastsearch-pg/src/sql.rs` 的 `ddl` 注释已经把这件事讲清了（并经实测更正）：
publication 的**列清单只过滤"列的值"、不抑制"Update 事件本身"**——
只改被排除列的 UPDATE 仍会产生 Begin/Relation/Update/Commit。
真正断开"写穿 → 复制 → 再嵌入 → 再写穿"这个环的，是
`PgStore::set_embedding` 的 `AND (embedding IS DISTINCT FROM $1 OR embed_model IS DISTINCT FROM $5)`
**幂等守卫**：值没变 → 0 行更新 → 第二轮无事件，环在一轮内收敛。

### 7.2 本表的处理

1. **`chunk_signal` 不进 publication（结论 A）⇒ 信号写入零复制事件 ⇒ 本迭代不存在新的反馈环。**
   这一点是可测的（§8 集成 6）：写一批信号后 `peek_changes` 返回的事件数不增加。
2. **但阻尼不能只靠"不进流"**——那是部署态属性，一次误配就没了（而且 §3.2 说明误配的后果
   比反馈环严重）。所以**所有信号写入语句无条件带幂等守卫**，与 `set_embedding` 同款：
   - `set_signal_embedding_sql`：
     `AND (embedding IS DISTINCT FROM $n OR status IS DISTINCT FROM 'ready' OR model IS DISTINCT FROM $m)`
   - `stale_*_sql`：`AND (s.status <> 'stale' OR s.embedding IS NOT NULL)`（§5.4 已含）
   - `upsert_signal_sql` 的 `ON CONFLICT DO UPDATE`：加
     `WHERE` 子句，全部字段与 EXCLUDED 逐一 `IS NOT DISTINCT FROM` 时不更新（避免空转刷 `updated_at`）。
   ⇒ **即使将来某个部署把信号表放进了流，值未变就是 0 行更新、就是无事件**，阻尼自带。
3. **`map` 的 relation 白名单**是"新表进流"的**前置条件**（§3.4-4），不是可选加固。
   本迭代不做（表不进流），但要写进 13-sync 的"下一迭代/已知限制"。

---

## 8. 测试用例与验收标准

### 8.1 单元（无需 PG，必跑，本轮可全绿）

1. `signal_ddl()` golden：含 4 列主键 `(collection, doc_id, chunk_id, signal_type)`、
   两个索引、`embedding real[]`、**不含任何 `CREATE EXTENSION`**（不变量 #1）、
   **不含 `FOREIGN KEY`**（§5.4）、**不含 `ALTER PUBLICATION`**（结论 A）。
2. **单表钉死**：`ddl()` 生成的 publication 语句里出现的表名有且只有入参 `table`；
   断言不含 `fastsearch_chunks_signal`。这条是结论 A 的结构性证明（同款先例：
   现有 `ddl_has_extension_table_publication` 断言派生列不在列清单里）。
3. **规则表穷尽性**：对 `SignalType` 全部 5 个变体逐一断言
   `binds_body()` / `binds_artifact()` 与 §5.2 的表一致；实现用穷尽 `match`
   （新增变体不更新规则 ⇒ 编译失败，这条要在测试注释里写明意图）。
4. `stale_body_bound_signals_sql` 形态：含 `signal_type = ANY(`、`md5(`、
   `body_hash IS DISTINCT FROM`、幂等守卫子句。
5. `artifact_key` 表达式形态：`COALESCE(md5(` + `media_bytes` + `(… -> 'asset')::text`。
6. `set_signal_embedding_sql` 含 `IS DISTINCT FROM` 幂等守卫（§7.2 的结构性证明）。
7. `SignalRow ↔ Signal` 往返：含中文 `signal_text`、含 `:` 的 `doc_id`、
   `embedding: None`、`error: Some(...)`；`SignalStatus`/`SignalType` 的 `from_str` 对未知值报错
   （同 `kind_from_str` 的先例）。
8. 信号表名派生 `format!("{table}_signal")` 过 `validate_identifier`（含超长/非法主表名的负例）。

### 8.2 集成（**需 `DATABASE_URL`，本机无 ⇒ 全部标 `待运行验证`**）

写法沿用仓内既有惯例（`crates/fastsearch-pg/src/lib.rs` 的 `b6_set_embedding_idempotent_guard`
与 `crates/fastsearch-engine/tests/cdc_closed_loop.rs`）：
`#[tokio::test]` + 开头 `let Ok(url) = std::env::var("DATABASE_URL") else { eprintln!("skip …"); return; };`
**未设 env 则打印 skip 并 return，不算失败**；测试自清理（用独立 collection/doc_id 前缀）；
涉 publication/slot 的用例与既有 CDC 用例**共享同名 publication，必须串行**
（`cdc_closed_loop.rs` 已有这条注释与做法，照抄）。

| # | 用例 | 断言 |
|---|---|---|
| 1 | **两路共存**（验收原文） | 同一 chunk 写 `vlm_caption` + `image_bytes` → 读回 2 行，各自 model/version/dim/status 独立 |
| 2 | **正文改 → 作废正确子集**（验收原文） | 改 `text` 后 upsert → `user_text`/`ocr`/`asr` 变 `stale` 且 `embedding IS NULL`；`vlm_caption`/`image_bytes` **`status`、`embedding`、`updated_at` 三者全部原封不动**（`updated_at` 不变是"没被无谓写"的硬证据） |
| 3 | **正文未改的重复 upsert** | 零信号被作废；`stale_*` 语句返回 0 行（幂等） |
| 4 | **工件变 → 作废另一子集** | 改 `media_bytes` → `vlm_caption`/`image_bytes`/`ocr`/`asr` 变 `stale`；`user_text` 不动 |
| 5 | **doc 级替换不误伤 + 不留孤儿** | `upsert_doc` 用少一个 chunk 的列表重放 → 仍在的 chunk 的视觉信号存活；消失的 chunk_id 的信号被回收；`orphan_signals_sql` 返回 0 |
| 6 | **删除路径回收** | `delete_doc` / `delete_collection` / `delete_chunks_visible` 之后 `orphan_signals_sql` 均返回 0；`delete_chunks_visible` 对**无权**的 id 既不删 chunk 也不删其信号 |
| 7 | **CDC 零事件**（验收原文"CDC 重放后信号表收敛到真源"） | 写一批信号 → `peek_changes` 事件数与写之前一致（信号表不在流里）；随后正常 chunk 写入仍被 CDC 正确捕获（不回归） |
| 8 | **CDC 重放收敛** | 跑一轮 `cdc_closed_loop` 同款闭环：chunks 变更经 CDC 应用后，信号表状态与规则表一致（body-bound 全 stale、artifact-bound 不动） |
| 9 | **单表契约**（§3.4-3） | 手工 `ALTER PUBLICATION fastsearch_pub ADD TABLE …_signal` → 再 `ensure_schema` → `pg_publication_rel` 只剩 chunks 表 |
| 10 | **可移植不破**（验收原文） | 整套 DDL 在 `pgvector/pgvector:pg17` 上跑通，且 `ddl()` 全文不含 `pgcrypto` / 任何 `shared_preload_libraries` 依赖；PG 版本下限仍是 **15+**（沿用现有列清单 publication 的要求，本表**不抬高**版本底线：`real[]`、`md5()`、`ANY($n::text[])`、4 列主键均为 PG 长期核心能力） |
| 11 | **ACL 不可绕过**（不变量 #3） | 信号表**无 `tenant`/`acl` 列**（结构性断言）；任何读信号的入口必须 JOIN chunks 复用 `AclFilter`——本迭代无此入口，故断言"无新增检索/读取入口"；将来加入口时必须补越权用例 |
| 12 | **并发 boot** | 8 连接并发 `ensure_schema`（沿用 `ensure_schema_concurrent_no_race` 同款）→ 含新表的 DDL 全部成功 |

### 8.3 验收标准

- 收口三件套全过：`cargo fmt --all --check` + `cargo clippy --workspace --all-targets -- -D warnings`
  + `cargo test --workspace`。
- §8.1 的 8 条单测在**无 PG 环境**全绿（这是本项能在本机被验证的全部）。
- §8.2 的 12 条在有 `DATABASE_URL` 时全绿——**在真机跑过之前，一律标 `待运行验证`，
  spec/看板不得写"已完成"**（DEV_SPEC §1「代码完成 ≠ 完成」、迭代计划 §11.3-6）。
- 逐条核对迭代计划 §7 的八条：
  ① 可移植 ✅（零新扩展，§8.2-10）；② PG 真源派生可重建 ✅（向量落 `real[]`，§4.3）；
  ③ ACL 不可绕过 ✅（无第二套 ACL 真源，§8.2-11）；④ 确定性 —— 本迭代不动排序，n/a；
  ⑤ 预过滤两端一致 —— 本迭代不动过滤，n/a；⑥ ADR 边界 ✅（无产品对象/版本/层级，§10-D5）；
  ⑦ 热路径零 docparse ✅（不碰 cli/vendor）；⑧ 诚实记账 ✅（本文全部 PG 结论标 `待运行验证`）。

---

## 9. 迁移与回滚

**迁移（additive，零回填）**：

1. `ensure_schema` 增发 `signal_ddl()` 的三条语句（`CREATE TABLE IF NOT EXISTS` + 两个 `CREATE INDEX IF NOT EXISTS`），
   仍在既有的事务 + `pg_advisory_xact_lock` 内 ⇒ 并发 boot 安全性不变。
2. 老部署升级后得到一张**空表**。既有 chunk 行**不需要回填** `body_hash`——
   信号行还不存在，没有可比较的对象；第一条信号写入时 hash 由 SQL 现算。
3. `fastsearch_chunks` **零改动**（除非采纳 `[待决策 D4]`，本文建议不采纳）。
4. publication、slot、CDC 配置**零改动**（结论 A）。

**回滚**：`DROP TABLE {table}_signal;` + 撤掉 `signal_ddl()` 调用即可，
主表与复制链路不受任何影响。**这份干净的回滚正是把 D4 排除在本项之外的理由**（§5.5）。

## 10. 影响面（只指出，不在本文设计）

| 位置 | 影响 | 何时 |
|---|---|---|
| `crates/fastsearch-pg/src/sql.rs` / `lib.rs` | 新增 `signal_ddl` 等纯函数 + `PgStore` 信号方法；`upsert_doc`/`upsert_chunks`/三条删除路径在**同事务内**增发信号语句 | 本项 |
| `crates/fastsearch-core/src/model.rs` | 新增 `SignalType` / `SignalStatus`（纯类型，无后端依赖） | 本项 |
| `docs/specs/12-pg.md` | 回写表结构、行为规约（作废规则表）、测试与状态 | 本项 |
| `docs/specs/13-sync.md` | 回写"已知限制/下一迭代"：`map` 无 relation 白名单 ⇒ 新表**禁止**进流（§3.2 的误删链路） | 本项 |
| `crates/fastsearch-vector/src/lib.rs` `VectorBackend::upsert(gid, vector, meta)` | **一个 `GlobalId` 只能持一条向量**。多路向量真正进检索时，键必须扩成 `(GlobalId, SignalType)` 或按信号分索引——**这是 KB-2.2 的前置改动，本项不动** | KB-2.2 |
| `crates/fastsearch-engine/src/lib.rs` `ingest_vector` / `apply_upsert` / `run` | 同上：写入与召回都以 gid 为单位；具名 N 路要等 `fuse` 泛化 | KB-2.2 |
| `crates/fastsearch-sync/src/lib.rs` `IndexSink` 三个方法 | 均以 gid/doc 为单位，无信号维度；多路落地后需决定"信号是否经 CDC 进派生索引" | KB-2.2+ |
| `crates/fastsearch-server/src/lib.rs` `/v1/index`、`/v1/chunks`、`/v1/images` | 本项**不改**；将来产 caption/OCR 信号的写路径要在这里挂 | KB-1.x / KB-2.2 |
| `chunks.image_vector_status` | 与 `chunk_signal(image_bytes).status` 语义重叠 ⇒ 见 `[待决策 D3]` | — |

## 11. 不做什么（明确排除）

- **不改检索读路径**：`engine`/`text`/`vector` 零改动，信号表本迭代**只写不读**。
- **不进 publication、不进 CDC 流**（结论 A）；**不改** `fastsearch_pub` 的 `SET TABLE`。
- **不建 ANN 索引、不做信号路的 PG 直查**（§4.3）。
- **不加外键**（§5.4 说明原因）。
- **不做多版本共存与回滚**：PK 是 4 元组，一个 `signal_type` 一行，换模型即覆盖 + 作废。
  "来源版本与回滚"是 ADR 明确划给调用方、且 KB-4 封禁增强的部分——**不从这里做回来**。
- **不给信号独立 `tenant`/`acl` 列**：可见性一律经其 chunk 行判定，**不造第二套 ACL 真源**（不变量 #3）。
- **不引入任何 PG 扩展**（`pgcrypto` 也不行，故用核心 `md5()`）、**不引入 Rust 新依赖**。
- **不实现 caption/OCR/ASR 的生产者**：那是 KB-1.x 与 P1-2 的事，且 VLM 区域级调用形态
  已被仓内实测证伪（FastGPT 参考建议 §8-3），本表只负责"生产者产出来之后存哪、怎么作废"。
- **不动 `chunks.image_vector_status`**（`[待决策 D3]`）。
- **不改 `upsert_chunk_sql` 现有的 `embedding = NULL` 行为**（`[待决策 D4]`，建议单独一轮）。

## 12. 待决策 / 待验证清单

**`[待决策]`**

- **D1 · 信号表名派生规则**：`format!("{table}_signal")`（本文按此写，推荐）vs 固定
  `fastsearch_chunk_signal`。前者随 `PgConfig.table` 隔离多部署，后者名字更短。
  影响面仅限 DDL 与测试常量。
- **D2 · 信号向量的可检索形态**：本迭代 `real[]`（真源可重建、不可 ANN）。
  KB-2.2 若要 PG 直查某一路，选 typed-列-按-dim + 各自 HNSW / 按 `signal_type` 分区表 /
  只在引擎侧持多路。**本文不定夺**，只记两条约束（§4.3）。
- **D3 · `chunks.image_vector_status` 的归宿**：本迭代它**仍是唯一真源**
  （信号表不进读路径 ⇒ 不产生第二个真源）。KB-2.2 让读路径感知信号后，
  要么把它降级为只读镜像、要么移除。**在那之前不得双写。**
- **D4 · `upsert_chunk_sql` 的 `embedding = NULL` 是否也加 `body_hash` 守卫**：
  本文建议**不在 KB-2.1 一起做**（§5.5）。
- **D5 · 信号是否需要对外可读的入口**（`GET /v1/chunks/{id}/signals` 之类）：
  本迭代**不做**。一旦要做，它是新检索/读取入口 ⇒ 必须走 ACL 服务端注入 +
  "越权用例进测试"（不变量 #3），且要先确认它不是在把控制面做回来（ADR）。

**`[待验证]`**（本机无 `DATABASE_URL`，全部无法在本轮确认）

- V1 · §8.2 的**全部 12 条集成用例**——整体标 `待运行验证`。
- V2 · `md5()` 在托管 PG（RDS/Supabase/Neon）与 FIPS 构建下的可用性。
- V3 · `(jsonb)::text` 规范化输出跨 PG 版本/跨 `jsonb` 写入路径的稳定性
  （决定 `artifact_hash` 对 Object/DocRegion 指针是否稳定）。
- V4 · `FASTSEARCH_CDC_PUBLICATION` 传逗号分隔多值（§3.3-4 的逃生门）在
  `pg_logical_slot_peek_binary_changes` 的 `publication_names` 下是否如预期工作——仓内**无测试覆盖**。
- V5 · 方案 B 若将来被重新提起：`ALTER PUBLICATION … DROP TABLE` + `ADD TABLE (collist)`
  在同一事务内对并发逻辑解码的影响（本文以此作为否决 B 的理由之一，但未实证）。

## 13. 已知限制

- **引用完整性靠代码**（无外键，§5.4）：任何绕过 `PgStore` 直接删 chunk 行的写入方会留下孤儿信号。
  缓解：`orphan_signals_sql` 进集成测试 + 作为运维审计查询公开（§6）。
- **外部直写 PG 的调用方改了正文不会即时作废信号**：`stale_*` 语句挂在我方写路径上。
  缓解：判据用 PG 内建 `md5()` ⇒ §6 的审计查询能**算出**漂移，且信号在下一次经我方写路径时收敛。
  这是"PG 是真源、允许外部写入"这条架构的固有代价，不是本设计引入的。
- **信号表本迭代不进检索**：一个 chunk 挂满五路信号，检索结果与今天**逐位一致**——
  这是刻意的（KB-2.2 才接读路径），但意味着 KB-2.1 单独上线**对检索质量零提升**，
  它买的是"表示完备 + 可审计 + 可只重建一路"这三样。**别把它记成质量提升。**
- **`real[]` 存 384~1024 维向量会走 TOAST**，读写有压缩/解压开销。信号表不在热路径上，
  可接受；真要 ANN 时按 D2 另选形态。
- **`SET TABLE` 的替换语义仍会静默移除别人手工加进来的表**——本文把它定义为契约并加测试钉死
  （§3.4-3），但**它依然是静默的**：不打日志、不报错。若将来觉得这不可接受，
  正确的加固是在 `ensure_schema` 里先查 `pg_publication_rel`、发现非预期表时**打警告**，
  而不是改成 `ADD/DROP`（§3.3 的理由不变）。
