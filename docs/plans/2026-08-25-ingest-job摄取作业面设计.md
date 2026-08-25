# KB-3.1：PG `ingest_job` 摄取作业面设计

> 日期：2026-08-25 · 类型：设计文档（spec 类作业，**不实施**）· 状态：**待评审 / 全部 PG 结论 `待运行验证`**
> 授权来源与边界：[职责边界修订：把「文档摄取作业面」收回引擎](../governance/2026-08-24-职责边界修订-摄取作业面收回引擎.md)（下称**修订文**）
> 仍然有效的上位 ADR：[职责边界：不承担生产身份与知识库控制面](../governance/2026-08-24-职责边界-不承担身份与控制面.md)
> 上游计划：[知识库引擎迭代计划 §5（KB-3）](2026-08-24-知识库引擎迭代计划.md) · 缺口来源：[文档摄取现状与差距 §3（G1–G7）](2026-08-24-作为知识库使用-文档摄取现状与差距.md)
> 并行编排：迭代计划 §11.2 作业 **A3**（通道 C4）。**本文只产出设计与验收，不改 `crates/` 下任何代码。**
>
> **本机无 `DATABASE_URL`** ⇒ 本文一切涉及 PG 实际行为的结论一律标 `待运行验证`，不得据此宣称"已完成"。

---

## 0. 一页纸摘要

| 维度 | 结论 |
|---|---|
| 新增 PG 表 | **一张**：`fastsearch_ingest_jobs`（无身份列，不进 publication，**同一 (tenant, collection, doc_id) 恒定一行**） |
| 新增 REST 端点 | `POST /v1/documents`（上传，**按大小分档 200/202**）、`GET /v1/jobs/{job_id}`、`GET /v1/documents`（派生只读视图）、`POST /v1/jobs/{job_id}/chunks` + `POST /v1/jobs/{job_id}/status`（**worker 专用**，见 §6.3 的 ACL 论证） |
| 新增 crate | `fastsearch-ingest-worker`（**独立二进制**，唯一持有 docparse 的进程） |
| 领任务 | `SELECT … FOR UPDATE SKIP LOCKED`（PG 9.5+ **核心**特性，零扩展） |
| 幂等键 | `content_sha256`，作用域 `(tenant, collection, doc_id)`；同 hash 已 `indexed` → **跳过**；hash 变化 → **覆盖**（doc 级 delete+insert）；**不做版本** |
| 无 `DATABASE_URL` | 作业面**全部端点 503**，与既有 `management_source` 一字不差 |
| 对 A2 的依赖 | `ALTER PUBLICATION … SET TABLE` 的**替换语义归属 KB-2.1（作业 A2）**；本文只**声明依赖**并给出两种结论下各自的做法，**不自行定夺** |
| 最需要评审的一处 | §6.3：worker 回写 chunk 时 ACL 从**哪里**来（不能来自 worker 自己的 key）——这是本设计唯一触到不变量 #3 表面的地方 |

---

## 1. 要做什么

让调用方（主要是 **agent**）能把**原始文档字节**直接交给 fastsearch，并**如实看到这个过程的状态**，
且这个过程**崩溃可恢复**：

1. `POST /v1/documents` 收原始字节 → 落**对象存储** → 建一条持久作业行；
2. 独立的 `fastsearch-ingest-worker` 进程领作业 → docparse 解析分块 → 写回 chunk → 回写状态；
3. `GET /v1/jobs/{id}` / `GET /v1/documents` 如实报告状态；
4. 任一环节崩溃/重启，**收敛到 PG 真值**，不需要调用方自建重试与台账。

对应缺口表：**G1**（原始文件入口）、**G2**（解析执行位置＝旁挂 worker）、**G3**（作业与状态机）、
**G4**（文档级视图）、**G5**（内容 hash 幂等与替换）、**G6**（长任务承载）。
**G7（分块 profile 可配置）不在本文范围**——归 KB-1.2；本文只把 profile 作为**一列 provenance 记录**带上（见 §3 与 §9）。

## 2. 为什么（一句话版，完整论证见修订文 §3）

"以托管 PG 为真源、派生索引可重建、崩溃恢复靠重放"是本项目的核心卖点（不变量 #2），
但它**今天在摄取路径上是断的**：`/v1/index`（`crates/fastsearch-server/src/lib.rs::index`）在一次同步 HTTP
请求里做完"嵌入 → 写 PG → 写派生索引 → commit"，**没有 job、没有恢复点、失败只返回 500**
（证据：该 handler 末尾 `Ok(Json(json!({ "indexed": n })))`，中途任一 `map_err` 直接返回 5xx，无任何持久记账）。
结果是每个调用方都必须自建重试与台账。收回作业面＝把已经付过的账补齐，**不是**把预算从检索质量挪走。

---

## 3. 边界：七条红线 → 可执行验收（本文的核心交付）

修订文 §4 的七条红线，逐条翻译成**可以在 CI 里跑的断言**。测试编号见 §10。

| # | 红线（修订文 §4 原文口径） | 本设计如何落成 | 验收项 |
|---|---|---|---|
| **R1** | `ingest_job` **无身份列**（无 owner/role/member/group/继承），只有 `tenant` + `acl`，与 chunk 行同构 | §4 表结构只有 `tenant text` + `acl text[]`，与 `sql.rs::ddl` 的 chunk 表同名同型；`lease_owner` 是 **worker 进程标识**（主机名+pid+随机），**不参与任何可见性判断**，且带 `NOT NULL` 注释钉死 | **T1**（DDL 字符串黑名单）、**T1b**（`lease_owner` 不出现在任何可见性 SQL 里） |
| **R2** | **不进 publication**；且须先解决 `SET TABLE` 替换语义（归 KB-2.1） | 本表 DDL **一条 publication 语句都没有**（§4.4）；替换语义问题**声明依赖 A2，不自行定夺**（§5） | **T2**（DDL 不含 `PUBLICATION`）、**T18**（`pg_publication_rel` 不含本表，`待运行验证`） |
| **R3** | 托管 PG 可移植不破：普通表 + `FOR UPDATE SKIP LOCKED`，零原生扩展 | §4.5 领任务 SQL；DDL 里**无 `CREATE EXTENSION`**（`vector` 扩展由既有 chunk 表 DDL 负责，作业表不碰）；不用 `pg_cron`/`pgmq`/`LISTEN` 依赖 | **T3**（DDL 无 `CREATE EXTENSION`/无扩展函数）、**T4**（claim SQL 含 `FOR UPDATE SKIP LOCKED`）、**T15**（并发领取不重复，`待运行验证`） |
| **R4** | 热路径隔离不破：解析只在独立 worker 二进制 | §6 新 crate；server **零 docparse 依赖不变**；CI 名单不变 + **新增一条反向断言**（server 不得依赖 worker） | **T23**（`hot-path-isolation` 仍绿）、**T24**（`cargo tree -p fastsearch-server` 不含 `fastsearch-ingest-worker`） |
| **R5** | 无 `DATABASE_URL` → 作业面一律 **503**，不造第二个内存真源 | §7 四个新端点全部先过 PG 可用性判据再做任何事（**先于读取 body**）；无内存 job map、无 SQLite、无本地文件台账 | **T10/T11**（无 PG 时上传/查询/列表全 503）、**T11b**（源码不含任何 job 的内存/文件回退容器） |
| **R6** | **不新增产品对象**：无 dataset/source/version 表，无层级，无回滚 | 只有一张作业表；**同一 (tenant, collection, doc_id) 恒定一行**（唯一索引强制），旧作业行被 UPDATE 覆盖 ⇒ **结构上不可能长出版本历史**；`GET /v1/documents` 响应字段走**白名单** | **T5**（响应 key ⊆ 白名单，且显式断言不含 `owner`/`dataset_id`/`version`/`parent_id`）、**T5b**（DDL 只新增一张表） |
| **R7** | ACL 服务端注入不可绕过：每个新入口都要有"越权用例进测试" | 上传走 `ingest_acl_for` + `apply_ingest_identity` 同款注入（§7.1）；查询/列表两端都带 ACL 谓词（§7.3/§7.4）；worker 回写的 ACL 来自 **job 行**而非 worker 身份（§6.3） | **T12/T13/T14**（未授权/无标签/请求体自带 acl 被忽略）、**T19**（跨租户不可见，`待运行验证`）、**T21**（worker 不能替别的 job 写，`待运行验证`） |

> **红线 R6 的结构性保证值得单独强调**：把"一文档一行"做成**唯一索引**，比写一句"我们不做版本"强得多——
> 想加版本就必须先删这个索引，那是一次显眼的、必然被评审看到的改动。修订文 §5 的回滚触发因此可观测。

---

## 4. `ingest_job` 表结构

### 4.1 落点与命名

- SQL 生成落在 **`crates/fastsearch-pg/src/sql.rs`**（纯函数、可单测，与既有 `ddl`/`insert_sql`/`delete_chunk_visible_sql` 同处），
  新增 `job_ddl(jobs_table)`、`claim_jobs_sql`、`heartbeat_sql`、`finish_job_sql`、`fail_job_sql`、`get_job_visible_sql`、`list_documents_sql`。
- ⚠️ **文件冲突提示（给主循环）**：`sql.rs` 同时是 **A2（KB-2.1）** 的独占文件。迭代计划 §11.1 已把
  KB-2.1 → KB-3.1 排在同一通道 **C4 并要求串行**：实施顺序必须 **A2 先合并，KB-3.2 再动 `sql.rs`**。
- 表名默认 `fastsearch_ingest_jobs`，来自 `PgConfig` 新增字段 `jobs_table`（与既有 `table` 一样过
  `fastsearch_pg::validate_identifier` 校验后才拼接进 SQL）。

### 4.2 列（**只有 `tenant` + `acl` 两个身份相关列，与 chunk 行同构**）

| 列 | 类型 | 说明 |
|---|---|---|
| `job_id` | `text PRIMARY KEY` | 服务端生成（UUIDv4/v7 的 hex 文本）。**在 Rust 侧生成**，不用 `gen_random_uuid()`——避免对 PG 版本/扩展的任何假设（红线 R3） |
| `collection` | `text NOT NULL` | 与 chunk 行同名同型 |
| `doc_id` | `text NOT NULL` | 同上 |
| `tenant` | `text` | **可空**，与 chunk 表一致（`sql.rs::ddl` 里 `tenant text`） |
| `acl` | `text[] NOT NULL` | 与 chunk 表同型。**刻意不给 `DEFAULT '{public}'`**：chunk 表的那个默认值是历史遗留，`ingest_acl_for` 已经 fail-closed 拒绝无标签写入；作业行只由服务端在 `ingest_acl_for` 成功后创建，给默认值等于给"忘配标签→静默公开"留门。**这是与 chunk 行唯一的差异，且方向是更严，不是更松** |
| `source_uri` | `text NOT NULL` | 原始字节在对象存储的 URI（`s3://` / `minio://` / `local://`，见 `fastsearch-engine::parse_object_uri`）。**绝不落 `bytea`** |
| `content_sha256` | `text NOT NULL` | 原始字节全量 SHA-256 hex，**幂等键** |
| `content_bytes` | `bigint NOT NULL` | 原始大小，用于分档与观测 |
| `media_type` | `text` | 上传声明的 MIME |
| `filename` | `text` | 原始文件名（仅 provenance / 派生 `doc_id` 用） |
| `parse_profile` | `jsonb NOT NULL DEFAULT '{}'` | **provenance 记录**（ocr/tables/vlm 开关、分块参数、docparse 版本）。修订文 §1 明确：引擎侧的 profile *记录* 属 provenance，**不是产品化管理**——因此**没有 profile 表、没有 profile 端点、不可按名字引用** |
| `state` | `text NOT NULL DEFAULT 'queued'` | `queued`/`parsing`/`chunking`/`embedding`/`indexed`/`failed`，`CHECK` 约束钉死这六个 |
| `stage_detail` | `jsonb NOT NULL DEFAULT '{}'` | 阶段进度（`pages_done`/`pages_total`/`chunks_parsed`/`chunks_indexed`）。放 jsonb 而非一堆列：进度字段会随解析器演进，不该每次都改 schema |
| `chunk_count` | `integer NOT NULL DEFAULT 0` | 终态记账，供 `GET /v1/documents` 免 `count(*)` |
| `lease_owner` | `text` | **worker 进程标识**（`hostname:pid:rand`）。**不是身份**：不出现在任何 ACL 谓词里（验收 T1b） |
| `lease_epoch` | `bigint NOT NULL DEFAULT 0` | **fencing token**：每次领取 +1；worker 的每一次回写都带 epoch，租约被抢走后旧 worker 的写入必然 0 行 |
| `lease_until` | `timestamptz` | 租约到期时刻；过期即可被重领 |
| `heartbeat_at` | `timestamptz` | 最近一次心跳 |
| `retry_count` | `integer NOT NULL DEFAULT 0` | 已失败次数 |
| `max_retries` | `integer NOT NULL DEFAULT 3` | 每作业记录重试上限，使"为什么这条不再重试"可解释 |
| `next_attempt_at` | `timestamptz NOT NULL DEFAULT now()` | 退避到期时刻（指数退避 + 抖动，值由 worker/server 算好写入，**不在 SQL 里算**，保持纯函数可单测） |
| `error` / `error_stage` | `text` | 最后一次失败的消息与所处阶段 |
| `created_at` / `updated_at` / `started_at` / `finished_at` | `timestamptz` | 时间戳；`created_at`/`updated_at` `NOT NULL DEFAULT now()` |

**死信不是第七个状态**：`state='failed' AND retry_count >= max_retries` 即死信，`dead_letter` 是
**API 层的派生布尔**，不是列。理由：修订文 §2 枚举的状态机只有六个状态，凭空加状态属于扩边界；
而"能不能再被领取"本来就已经由领取 SQL 的 `retry_count < max_retries` 条件表达，加一个状态是冗余的第二真源。

### 4.3 索引与约束

```sql
-- 一文档一行（红线 R6 的结构性保证；tenant 可空 ⇒ 用 coalesce 表达式，避免 NULL 不参与唯一性）
CREATE UNIQUE INDEX IF NOT EXISTS {jobs}_doc  ON {jobs} (coalesce(tenant,''), collection, doc_id);
-- 领取扫描（部分索引：终态行不参与）
CREATE INDEX IF NOT EXISTS {jobs}_claim ON {jobs} (next_attempt_at, created_at) WHERE state <> 'indexed';
-- 列表视图排序/过滤
CREATE INDEX IF NOT EXISTS {jobs}_list  ON {jobs} (coalesce(tenant,''), collection, doc_id);
-- 幂等查重
CREATE INDEX IF NOT EXISTS {jobs}_hash  ON {jobs} (coalesce(tenant,''), collection, content_sha256);
```

`[待运行验证]` `coalesce(tenant,'')` 作为索引表达式在 RDS/Supabase/Neon 上均需 `IMMUTABLE` 判定通过——
本机无 PG，未实跑。若某托管 PG 拒绝，退路是把 `tenant` 改为 `NOT NULL DEFAULT ''`（**但那会与 chunk 行不同构**，
属需要重新评审的改动，不得在实施期擅自决定）。

**向后兼容**：沿用既有 DDL 的写法（`CREATE TABLE IF NOT EXISTS` + 逐列 `ALTER TABLE … ADD COLUMN IF NOT EXISTS`，
见 `sql.rs::ddl` 里对 `metadata`/`searchable` 的处理），并同样跑在 `PgStore::ensure_schema` 的
`pg_advisory_xact_lock(SCHEMA_DDL_LOCK_KEY)` 内（多副本并发 boot 安全）。这直接对应修订文 §5 的回滚触发之一
（"schema 迁移不可向后兼容"）：**加列可回滚、不改既有列语义**是本表演进的硬约束。

### 4.4 DDL **不碰 publication**（红线 R2 的本文侧动作）

`job_ddl()` 生成的语句里**不得出现 `PUBLICATION` 字样**——不 CREATE、不 ALTER、不 ADD、不 SET。
这条可以直接单测（T2），且**在 A2 的两种可能结论下都成立**（见 §5）。
理由（不变量 #2 侧）：作业状态是**摄取过程的记账**，进了复制流就会被 CDC 解码器当成源变更再喂回引擎——
既无意义又制造反馈环。chunk 表通过"publication 列清单排除派生列"断环（`sql.rs::ddl` 的注释），
作业表则用更彻底的办法：**整张表不发布**。

### 4.5 领任务：`FOR UPDATE SKIP LOCKED`

```sql
WITH cand AS (
  SELECT job_id FROM {jobs}
   WHERE state <> 'indexed'
     AND retry_count < max_retries
     AND next_attempt_at <= now()
     AND (state IN ('queued','failed') OR lease_until IS NULL OR lease_until < now())
   ORDER BY next_attempt_at, created_at
   FOR UPDATE SKIP LOCKED
   LIMIT $1
)
UPDATE {jobs} j
   SET state = 'parsing',
       lease_owner = $2,
       lease_epoch = j.lease_epoch + 1,
       lease_until = now() + make_interval(secs => $3),
       heartbeat_at = now(),
       started_at = COALESCE(j.started_at, now()),
       updated_at = now()
  FROM cand
 WHERE j.job_id = cand.job_id
RETURNING j.job_id, j.lease_epoch, j.collection, j.doc_id, j.tenant, j.acl,
          j.source_uri, j.content_sha256, j.media_type, j.filename, j.parse_profile;
```

- `FOR UPDATE SKIP LOCKED` 是 **PG 9.5+ 核心特性**，不需要 `shared_preload_libraries`、不需要任何扩展 ⇒ **不变量 #1 不破**（红线 R3）。
- **不引入 `LISTEN/NOTIFY` 依赖**：某些托管 PG 的连接池（pgbouncer transaction 模式）下 `LISTEN` 不可靠。
  worker 用固定间隔轮询（默认 500ms，空转时指数退避到 5s）——慢一点，但可移植性零风险。
- `[待运行验证]` 事务内 `now()` 是**事务开始时刻**而非语句时刻；租约到期判定用 `now()` 在毫秒级租约上会有偏差，
  但租约按**秒级**（默认 60s）设置，偏差可忽略。若实跑发现问题，改用 `clock_timestamp()`。
- **单进程内的并发限制**：`fastsearch_pg::PgStore` 是 `client: Mutex<Client>` 的**单连接**（该结构体的文档注释
  已说明原因与后续演进方向）。因此 worker 侧若要真并发领取多条作业，需要**每个并发槽一条连接**
  （`JobStore::connect` 建多个），而不是共享一个 `PgStore`。见 §6.4。

### 4.6 心跳 / 完成 / 失败（全部带 fencing）

```sql
-- 心跳（顺带推进阶段与进度）：0 行 ⇒ 租约已失 ⇒ worker 立刻弃活，不得继续写
UPDATE {jobs} SET heartbeat_at = now(), lease_until = now() + make_interval(secs => $4),
       state = COALESCE($5, state), stage_detail = COALESCE($6, stage_detail), updated_at = now()
 WHERE job_id = $1 AND lease_owner = $2 AND lease_epoch = $3 AND state <> 'indexed'
RETURNING job_id;

-- 完成（**只能由 server 在 chunk 写入成功之后调用**，见 §8 排序铁律）
UPDATE {jobs} SET state='indexed', chunk_count=$3, error=NULL, error_stage=NULL,
       lease_owner=NULL, lease_until=NULL, finished_at=now(), updated_at=now()
 WHERE job_id=$1 AND lease_epoch=$2 RETURNING job_id;

-- 失败（退避时刻由调用方算好传入，保持 SQL 无策略）
UPDATE {jobs} SET state='failed', error=$3, error_stage=$4, retry_count = retry_count + 1,
       next_attempt_at = $5, lease_owner=NULL, lease_until=NULL, updated_at=now()
 WHERE job_id=$1 AND lease_epoch=$2 RETURNING retry_count, max_retries;
```

---

## 5. 对 A2（KB-2.1）的依赖声明——**本文不定夺**

迭代计划 §11.1 写明：*"KB-2.1 与 KB-3.1 都要回答 `ALTER PUBLICATION … SET TABLE` 是替换语义、新表怎么办
（§10 待决策 #5）。**归属：C4 的 KB-2.1**。KB-3.1 只能在 spec 里声明依赖，不得自行定夺。"* 本节即那份声明。

**现状（已读代码，`crates/fastsearch-pg/src/sql.rs::ddl`）**：publication 段是一个 `DO $$` 块，
① 无 `fastsearch_pub` → `CREATE PUBLICATION … FOR TABLE {table} ({collist})`（带 `EXCEPTION` 防并发首建）；
② `fastsearch_pub` 已含**本表** → `ALTER PUBLICATION fastsearch_pub SET TABLE {table} ({collist})`；
③ publication 属于别的表 → **不动**（注释写明"避免并发实例互抢同名 publication"）。
`SET TABLE` 是**替换**语义：分支 ② 触发时，publication 里除 `{table}` 外的任何表都会被**静默移除**。

**两种可能结论下，本设计各自怎么办**：

| A2 可能结论 | `ingest_job` 侧动作 | 验收怎么写 |
|---|---|---|
| **X · publication 永远单表**（保留 `SET TABLE`） | **零动作**。作业表本就不发布；分支 ② 每次 boot 反而会把"被谁误加进来的作业表"清掉，是一层免费的自愈 | **T18**：连上 PG 后查 `pg_publication_rel` 断言不含作业表（`待运行验证`） |
| **Y · DDL 改为精确 `ADD/DROP` 收敛**（为 `chunk_signal` 之类的第二张表让路） | 仍**零动作**，但多两条硬约束：(a) 作业表**不得**出现在 A2 的 `ADD TABLE` 清单里；(b) A2 的收敛逻辑必须是"把清单内的表 ADD、把**清单内应删的**表 DROP"，而**不能**是"DROP 一切不在清单里的表"——后者行为上等价，但会让作业表的缺席变成一条需要维护的负向清单 | **T18** 不变，另加 **T22**：跑一次 A2 的收敛 DDL 后，作业表仍不在 publication（`待运行验证`） |

**顺序依赖（给主循环）**：结论 Y 下，KB-3 的实施必须排在 A2 合并**之后**，否则 T22 无对象可测。
结论 X 下无顺序要求，但 `sql.rs` 的文件冲突仍要求 C4 通道内串行。

---

## 6. `fastsearch-ingest-worker`（新 crate，红线 R4）

### 6.1 为什么必须是独立二进制

CI 的 `hot-path-isolation` job（`.github/workflows/ci.yml`）对
`fastsearch-server fastsearch-engine fastsearch-core fastsearch-text fastsearch-vector fastsearch-mcp fastsearch-cli`
七个 crate 逐个跑 `cargo tree -p <c> -e normal | grep -qi docparse`，命中即失败。
**把解析塞进 server 会直接让这个 job 变红**——这不是风格偏好，是一条已经在跑的门禁。
（注意：门禁实际名单是**七个** crate，比迭代计划 §5.4 文字里写的"五个"多了 `fastsearch-mcp` 与 `fastsearch-cli`；
以代码为准，本文按七个记。）

**新增反向断言（T24）**：`cargo tree -p fastsearch-server -e normal` 不得出现 `fastsearch-ingest-worker`。
理由：worker 依赖 docparse，若哪天有人为了"复用一个结构体"让 server 依赖 worker，
docparse 会顺着这条边爬进 server ——现有门禁会抓到，但报错信息指向的是 docparse 而非真正的根因。
加这条断言让**根因**直接暴露。

### 6.2 依赖与复用

- **允许依赖**：`fastsearch-core`（`Chunk` 类型）、`fastsearch-pg`（作业行 SQL + 连接）、
  `fastsearch-engine`（**仅为 `ObjectStore`/`S3ObjectStore`/`LocalObjectStore`** 取原始字节）、
  docparse（`docparse-core`/`-pdf` + 可选重档 feature）、`ureq`、`tokio`。
- 依赖 `fastsearch-engine` **不会**让门禁变红：门禁查的是 `engine` 会不会拉入 docparse，
  而边的方向是 worker → engine。代价是 worker 二进制里多了 tantivy 等一批用不到的东西。
  `[待决策]` 更干净的做法是把 `ObjectStore` 三件套下沉到独立 crate（`fastsearch-object`，engine re-export 保兼容），
  本文**默认取"直接依赖 engine"**（零 API 变动、最小改动面），下沉列为可选优化。
- **适配器复用（`from_docparse_chunk`）**：该函数今天住在 `crates/fastsearch-cli/src/ingest.rs`，
  是 `pub fn`，但由 `parse` feature 门控。三条路：
  1. worker 依赖 `fastsearch-cli/parse` —— 会把 clap/ureq 等 CLI 依赖拖进 worker，且让 `cargo tree -p fastsearch-cli`
     的默认档是否仍零 docparse 变成一个 **feature unification 的运气问题** `[待验证]`。**不推荐**。
  2. **把 `parsers()`/`apply_ocr`/`apply_vlm`/`apply_tables`/`map_kind`/`map_bbox`/`map_image`/`from_docparse_chunk`
     整体下沉到新 crate `fastsearch-ingest-adapter`（parse-gated），`fastsearch-cli` 与 worker 都依赖它。** ← **推荐**
  3. worker 复制一份 —— **明确拒绝**：`ingest.rs` 的模块注释写明这个适配器正是融合要消除"跨仓手工锁步"的焊点
     （"改任一侧 schema，本适配器编译即报错"），复制等于把焊点又劈成两半。
  `[待决策]` 1/2 之间取 2，需在 KB-3.2 实施前确认（下沉会动 `fastsearch-cli/src/ingest.rs`，与 C2 通道的 KB-1.1/1.2 冲突，**必须排在它们之后**）。

### 6.3 ⚠️ 最需要评审的一处：worker 回写 chunk 时，ACL 从哪里来

**问题（读代码发现的真问题，必须正面解决）**：`crates/fastsearch-server/src/lib.rs::apply_ingest_identity`
**无条件**把每个 chunk 的 `tenant`/`acl` 覆盖成**调用者 principal** 的值。
若 worker 拿自己的一把 API key 去 `POST /v1/index`，写出来的 chunk 就会带 **worker key 的身份**，
而不是原上传者的——worker key 若配成 `worker=:public`，**全租户的文档会变成公开**。
这是一个会静默破坏多租户正确性的坑，**照迭代计划 §5.4 的字面写法（"worker → POST /v1/index"）实施就会踩上**。

**采纳方案：job 作用域的写入入口**

- `POST /v1/jobs/{job_id}/chunks`：worker 提交解析产物。server 端：
  1. 认证 worker key（必须在 `FASTSEARCH_WORKER_KEYS` 声明的那一类 key 里）；
  2. 按 `job_id` + `lease_epoch` 取作业行（**epoch 不匹配 → 409，直接丢弃**）；
  3. **`tenant`/`acl` 从作业行读取并强制注入 chunk**，请求体里带的 `tenant`/`acl` 一律**忽略**；
  4. `collection`/`doc_id` 同样**只**取自作业行，请求体不得指定；
  5. 之后复用与 `index` handler **完全同一段**写入内核（对象存储归一 → 维度校验 → 嵌入 → `upsert_doc` → `set_embedding` 写穿 → 引擎 `remove_doc`+`ingest`+`commit`）。实施时把该段抽成内部函数，两个入口共用，**不允许出现第二份实现**。
- `POST /v1/jobs/{job_id}/status`：心跳 / 阶段推进 / 失败上报，同样带 `lease_epoch` fencing。

**为什么这不是 FastGPT `authTmbId=false` 那种口子**（必须逐条对上，否则就是在拆自己的不变量）：

| 判据 | `authTmbId=false` | 本方案 |
|---|---|---|
| 是否跳过鉴权 | 是（分支直接不查成员） | **否**：worker key 照常认证，且必须是被显式声明的 worker key |
| ACL 从哪来 | 无（被跳过） | **上传时已认证身份**编译出的 tags，**已固化在作业行里**，客户端与 worker 都改不了 |
| 可写范围 | 任意 | **只有该 job 行的 `(collection, doc_id)`**，且 epoch 必须当期有效 |
| 是否引入身份模型 | 是（绕过成员概念） | **否**：没有 role/成员/继承；worker key 只是"能否代表作业行写入"的单一布尔能力位 |

**必须进测试的越权用例（红线 R7）**：T14（请求体自带 `acl`/`tenant`/`collection` 被忽略）、
T21（worker key 拿 job A 的 epoch 去写 job B → 403/409）、T21b（普通用户 key 调 `/v1/jobs/{id}/chunks` → 403）、
T19（跨租户读作业/文档 → 404）。

`[待决策·需评审]` 本节是整份设计里唯一触到不变量 #3 表面的地方。若评审否决"worker key 能力位"，
退路是**每租户一把 worker key、worker 按 job 的 tenant 选 key**——运维上不可扩展，但不需要新能力位。
不接受的第三条路：worker 直接写 PG 绕过 server（会产生**第二条 ACL 注入路径**，且绕过 B6 的 `set_embedding` 写穿）。

### 6.4 worker 主循环

```
loop {
  job = claim(FOR UPDATE SKIP LOCKED, lease=60s)   // 无作业 → 退避 0.5s→5s
  guard: 后台心跳任务每 20s 打一次；心跳返回 0 行 ⇒ 取消本作业的所有后续动作
  bytes  = object_store.get(job.source_uri, max_bytes)          // 阶段 parsing
  doc    = parsers().find(supports).parse(bytes)                 // + apply_ocr/apply_vlm/apply_tables（feature+env 门控）
  chunks = chunk_document(doc).map(from_docparse_chunk)          // 阶段 chunking
  POST /v1/jobs/{id}/chunks {lease_epoch, chunks}                // 阶段 embedding（嵌入在 server，worker 不做）
  //   ↑ 200 之后，由 **server** 在同一请求内把 job 置 indexed（排序铁律，见 §8）
  失败 → POST /v1/jobs/{id}/status {state:"failed", error, error_stage, next_attempt_at}
}
```

- **嵌入留在 server**：worker 只做"字节 → chunk"。理由：嵌入后端配置（`FASTSEARCH_EMBEDDER`/维度/写穿）
  全在 server，复制一份到 worker 等于复制一份配置真源。
- **worker 配置**：`DATABASE_URL`（领任务）、`FASTSEARCH_SERVER` + `FASTSEARCH_WORKER_KEY`（回写）、
  对象存储 env（与 server 同名：`FASTSEARCH_S3_*` / `FASTSEARCH_OBJECT_DIR` / `FASTSEARCH_OBJECT_BUCKET`）、
  `FASTSEARCH_WORKER_CONCURRENCY`（默认 1；**每个并发槽一条 PG 连接**，见 §4.5 的 `Mutex<Client>` 说明）、
  解析重档 env（`FASTSEARCH_OCR_MODELS` / `FASTSEARCH_UNIREC_MODELS` / `FASTSEARCH_VLM_URL`+`_MODEL`）。
- **独立部署与限流（复活的 P2-6）**：worker 与 server 是两个二进制、两个镜像、两套扩缩容；
  worker 的并发上限由 `FASTSEARCH_WORKER_CONCURRENCY` × 副本数决定，server 侧 `/v1/documents`
  走既有 `RateLimiter`（`crates/fastsearch-server/src/lib.rs::RateLimiter`，鉴权之后限流，见 `index` handler 的 M21 注释）。

---

## 7. REST 端点

**四个端点的共同前置**（顺序固定，任何一个新入口都照抄）：
`principal_from_headers` → 401 ⇒ `allow(rate_key)` → 429 ⇒ **作业面 PG 可用性** → 503 ⇒ `ingest_acl_for`（写入类）→ 403。
其中 **503 判据必须早于读取 request body**，否则无 PG 的实例会先吃满一个大文件再拒绝。

**503 判据**：`ServerState` 新增 `jobs: Option<Arc<JobStore>>`（boot 时若有 `DATABASE_URL` 则建），
判据与既有 `management_source`（`s.engine.lock().await.source_pg_clone()`）**同源同义**：同一个 `DATABASE_URL` 决定两者有无。
错误文案照抄风格：`"ingest jobs require a configured PostgreSQL source store"`。

> **为什么作业面用独立连接而不是复用 `management_source` 的 `PgStore`**：`PgStore` 是 `client: Mutex<Client>`
> 单连接（其文档注释明确写了"读路径经 engine.lock 串行化""后续演进迁到连接池"）。
> §7.1 的同步等待窗口会以 50–100ms 的节奏轮询作业行，**把这些轮询压在真源那把 Mutex 上会直接拖慢检索与写入**。
> 独立 `JobStore` 连接是本设计对既有性能特性的一处主动保护，`[待运行验证]`。

### 7.1 `POST /v1/documents`（multipart，**按大小分档 200/202**）

**multipart 字段**（解析方式照抄 `image_upload`：`Multipart::from_request` + `next_field` 循环）：
`file`（必填，字节）、`collection`（必填）、`doc_id`（选填，缺省取 `filename`）、`media_type`（选填）、
`parse_profile`（选填 JSON 串）、`wait`（选填 `auto`(默认)/`never`）、`source_uri`（选填，见"大文件"）。

**流程**：
1. 前置四步（上方）；`ingest_acl_for(&principal)` 得到 `acl`（**无标签 key → 403**，fail-closed 复用）。
2. 对象存储未配置 → **503**（`engine.put_object` 在无 store 时返回 `"object store not configured"`；
   本端点必须**提前**判定并给出 503 而不是 500）。
3. 读字节 → `sha256_hex(bytes)`（全量 32 字节 hex；注意 server 现有的 `short_sha` 只取前 8 字节、
   `fastsearch-engine::sha256_hex` 是**私有**函数 ⇒ 实施时需在 server 内加一个全量 hex helper 或把 engine 的提升为 `pub`）。
4. 幂等判定（§7.2）。
5. `object_namespace(principal.tenant.as_deref())` → key = `{ns}/{collection}/{doc_id}/raw-{sha}.{ext}` → `engine.put_object`。
   （租户前缀 + `validate_object_ref` 的归属校验是既有机制，直接复用。）
6. UPSERT 作业行（`ON CONFLICT (coalesce(tenant,''), collection, doc_id) DO UPDATE`），置 `state='queued'`、
   `retry_count=0`、`next_attempt_at=now()`。
7. **分档返回**（见下）。

**分档规则（迭代计划 §5.2 的"2026-08-24 复审修正"，不得写回无条件 202）**：

| 条件 | 行为 | HTTP |
|---|---|---|
| `content_bytes <= FASTSEARCH_INGEST_SYNC_MAX_BYTES`（默认 **1 MiB**）**且** profile 不含重增强（ocr/vlm/tables）**且** `wait != "never"` **且** 当前同步等待并发未超 `FASTSEARCH_INGEST_SYNC_MAX_INFLIGHT`（默认 32） | 进入**等待窗口**：server 每 100ms 轮询作业行，上限 `FASTSEARCH_INGEST_SYNC_WAIT_MS`（默认 3000ms）。窗口内 `state='indexed'` → 返回终态 | **200** |
| 窗口耗尽仍非终态，或不满足上述任一条件 | 立即返回，带 `poll_after_ms` | **202** |
| 窗口内变成死信（`failed` 且 `retry_count >= max_retries`） | 同构体带 `state:"failed"` 与 `error` | **422** |
| 同 hash 且已 `indexed`（幂等命中） | 直接返回既有终态，`deduplicated: true` | **200** |

> **关键：同步档不是"server 里解析"**。server 一行 docparse 都不碰（红线 R4）；同步档只是
> **server 替 agent 把那个轮询循环吃掉**——解析仍然发生在 worker 进程里。
> 副作用是显式的：**没有 worker 在跑时，同步档必然超时退化成 202**，这恰好是如实报告（诚实记账）。
> `GET /v1/jobs/{id}` 的响应因此还应带一个 `workers_seen_recently` 布尔（由"最近 N 秒内是否有任何作业行的
> `heartbeat_at` 被更新"派生），让 agent 能自己区分"排队中"与"根本没人干活"。`[待决策]` 该字段可留到 KB-3.3。

**两档返回体同构**（同一套 serde 结构，只有值不同 —— 验收 T7）：

```json
{
  "job_id": "0f2b…",
  "collection": "kb",
  "doc_id": "r.pdf",
  "state": "indexed",
  "searchable": true,
  "chunk_count": 12,
  "content_sha256": "9ab3…",
  "deduplicated": false,
  "dead_letter": false,
  "retry_count": 0,
  "poll_after_ms": 0,
  "job_url": "/v1/jobs/0f2b…",
  "error": null
}
```
202 档：`state:"queued"`、`searchable:false`、`chunk_count:0`、`poll_after_ms:2000`（与迭代计划 §9 的例子一致）。

**大文件与 20MB body 上限**（现状证据：`crates/fastsearch-server/src/lib.rs::router` 末尾
`.layer(DefaultBodyLimit::max(20 * 1024 * 1024))`，作用于**全部路由**）：

1. **抬高本路由的上限**：给 `/v1/documents` 单独挂一层 `DefaultBodyLimit::max(FASTSEARCH_MAX_DOCUMENT_BYTES)`（默认 **32 MiB**）。
   `[待验证]` axum 0.8 中 per-route `DefaultBodyLimit` 覆盖 router 级 layer 的**顺序语义**需实测确认。
2. **同时抬高对象存储上限**：`LocalObjectStore`/`S3ObjectStore` 的 `max_bytes` 默认 20 MiB
   （构造函数里写死，由 `FASTSEARCH_S3_MAX_IMAGE_BYTES` 覆盖）。**两个上限必须同调**，否则 body 过得去、`put` 报错。
   这是一个很容易漏的配置耦合，要写进 README 与启动日志。
3. **超过上限 → 413**，错误消息里给出第 4 条的出路。
4. **`source_uri` 直传（本轮的大文件正解，零新机制）**：运维已有 S3/MinIO 时，调用方自己把文件 PUT 进桶，
   然后 `POST /v1/documents` 只传 `source_uri` + `collection` + `doc_id`（无 `file` 字段）。
   server 用既有 `engine.validate_object_ref(uri, principal.tenant.as_deref())` 校验**归属**（该函数已按 tenant 校验），
   再 `fetch_object_bytes` 算 hash（`[待决策]` 大文件算 hash 要整块读进内存，是否改为"信任调用方传入 hash + 只在 worker 侧校验"）。
5. **不做的**：预签名 **PUT** 直传与分片上传。诚实记账：`fastsearch-engine::ObjectStore` trait 只有
   `put`/`get`/`presign_get`/`validate_ref`/`delete`，**没有 `presign_put`**，也**没有流式 put**（`put` 取 `&[u8]`）——
   要做就得改 trait 与两个实现，属独立迭代。分片上传还会长出"上传会话"状态，逼近产品对象（红线 R6），
   本轮**明确不做**。

### 7.2 幂等与替换语义（G5；迭代计划 §5.5 要求"定死"）

**定死如下**（作用域 `(tenant, collection, doc_id)`，因为唯一索引就是这三元组）：

| 情形 | 语义 | 说明 |
|---|---|---|
| 同 `doc_id` + **同 `content_sha256`** + 现有行 `state='indexed'` | **跳过** | 不重跑解析，直接 200 + `deduplicated:true`。原始字节也不重复写（key 里含 hash，`put` 覆盖同 key 等价） |
| 同 `doc_id` + 同 hash + 现有行**在途**（`queued`/`parsing`/`chunking`/`embedding` 且租约未过期） | **合流** | 返回同一 `job_id`，202。不新建作业 |
| 同 `doc_id` + 同 hash + 现有行 `failed` | **重开** | 同一行 UPDATE：`state='queued'`、`retry_count=0`、`next_attempt_at=now()` |
| 同 `doc_id` + **不同 hash**，现有行终态 | **覆盖** | 同一行 UPDATE 换 hash/uri/profile，`state='queued'`。索引侧是 **doc 级替换**（`PgStore::upsert_doc` 事务内 delete+insert；引擎侧 `remove_doc` + 重 ingest）——**既有语义，不新增机制** |
| 同 `doc_id` + 不同 hash，现有行**在途** | **409 `job_in_flight`** + 既有 `job_id` | 不静默丢弃前一次上传的意图；调用方可轮询后重试 |
| **新版本** | **不做**（红线 R6） | 唯一索引在结构上就不允许第二行 |

**旧内容的原始字节**：覆盖时旧 `source_uri` 与新的不同（key 含 hash）⇒ 需要删除旧对象，
复用 `index` handler 已有的"新旧 object uri 差集清理"套路（`engine.delete_object`）。
`[待决策]` 是否保留旧原始字节一段时间（便于失败排查）——保留会像"版本"，**默认不保留**。

### 7.3 `GET /v1/jobs/{job_id}`

- 前置四步；PG 不可用 → **503**。
- SQL 带 ACL 谓词，与既有管理端点**同款翻译**（`sql.rs::acl_clause` / `delete_chunk_visible_sql` 的
  `tenant = $n AND ('public' = ANY(acl) OR acl && $m)`，admin（principal 无 tenant）走无 tenant 子句的变体），
  语义与 `core::AclFilter::visible` 对齐（调用者有 tenant、行无 tenant ⇒ 不可见）。
- **不可见 → 404**（不是 403）：避免把"存在这么一个 job_id"泄露给别的租户。
- 响应 = §7.1 的同构体 + `stage`（当前阶段）+ `stage_detail` + `created_at`/`updated_at`/`finished_at` + `parse_profile`。

### 7.4 `GET /v1/documents`（**派生只读视图**，红线 R6 的重灾区）

修订文 §2 的定性原文：*"对既有 chunk 行与作业行的派生只读视图，不新建产品对象表、不引入层级、不带版本。"*

- 数据来源 = **chunk 行 ⟗ 作业行的 FULL OUTER JOIN**（按 `(collection, doc_id)`）：
  - 有 chunk 无 job：直接 `POST /v1/index` 或经 CDC 灌入的文档 → `state:"indexed"`、`job_id:null`；
  - 有 job 无 chunk：尚未完成的上传 → `state` 取作业行；
  - 两者都有：以作业行为准，`chunk_count` 取 chunk 行的实计数。
  **这正是"派生视图"的字面含义**：没有它，列表就只能看见走过上传端点的文档，等于偷偷造了一个产品对象。
- 两侧都加 ACL 谓词（chunk 侧本就有现成翻译）。
- 查询参数：`collection`（可选）、`state`（可选）、`after_collection`+`after_doc_id`（**keyset 游标**）、`limit`
  （默认 100、上限 500，与既有 `DEFAULT_CHUNK_PAGE_SIZE`/`MAX_CHUNK_PAGE_SIZE` 同口径）。
  **排序恒定 `(collection, doc_id)` 升序**——确定性（不变量 #4 的同一精神：可复现、可分页）。
- **响应字段白名单**（验收 T5 逐字断言）：
  `collection, doc_id, state, searchable, chunk_count, job_id, content_sha256, media_type, filename,
   created_at, updated_at, finished_at, retry_count, dead_letter, error`。
  **显式禁列**：`owner`、`owner_id`、`dataset_id`、`source_id`、`version`、`revision`、`parent_id`、`permissions`、`role`、`members`。
  出现其中任何一个即视为越界，按修订文 §5 回滚。
- `[待验证]` `count(*) GROUP BY (collection, doc_id)` 在大语料上的代价：chunk 表有现成的
  `{table}_doc ON (collection, doc_id)` 索引（`sql.rs::ddl`），预期走索引扫描但仍随 doc 数线性。
  提供 `?counts=false` 关掉计数列作为逃生口。

### 7.5 观测

`/metrics` 增加（沿用既有 `Metrics` 的原子计数风格）：`fastsearch_ingest_jobs_total{state=…}`（gauge，按需查 PG，带缓存）、
`fastsearch_ingest_uploads_total`、`fastsearch_ingest_sync_hit_total` / `_timeout_total`（同步档命中/退化次数——
**这是"分档是否真的在帮 agent"的唯一证据**）、`fastsearch_ingest_dead_letter_total`。

---

## 8. 失败注入与收敛（迭代计划 §5 的验收原文）

**先立一条排序铁律（贯穿全部四个注入点）**：

> **作业状态的"前进"永远写在副作用成功之后**：`state='indexed'` 只能由 server 在 chunk 写入内核返回成功
> **之后**、同一请求内写。绝不允许 worker 先报成功再写、或 server 先标 indexed 再写索引。
> 反过来"后退"（租约过期 → 被重领）可以随时发生，因为重跑是幂等的。

| # | 注入点 | 崩溃后的现场 | 怎么收敛 | 验收 |
|---|---|---|---|---|
| **F1** | **PG 写入后**（`upsert_doc` 已提交，进程立刻死） | chunk 行已在真源；作业行停在 `embedding`，租约未释放 | 租约到期（默认 60s）→ 被重领 → 重跑解析（同 `content_sha256` + 同 `parse_profile` ⇒ 确定性同产物）→ 再次 doc 级替换 → `indexed` | **T20a**：最终 `GET /v1/jobs/{id}` = `indexed`，且 chunk 集合与一次成功跑**逐字段相等**（golden 比对） |
| **F2** | **embedding 后**（`set_embedding` 已写 PG `embedding` 列，派生未建） | PG 有向量，引擎侧派生索引没有 | PG 是真源：CDC 重放或从快照重建即补齐。`set_embedding` 侧已有幂等守卫（`sql.rs::ddl` 的注释描述了 `IS DISTINCT FROM` 阻尼；`[待验证]` 需核对 `PgStore::set_embedding` 实现细节） | **T20b**：重启 + 消费 CDC 后，向量检索能命中该 doc |
| **F3** | **派生 commit 前**（`engine.commit()` 未执行） | PG 有全部数据；派生索引缺该 doc；**作业行必须仍非 `indexed`**（铁律） | 重启后由 CDC/重建补派生；作业被重领后重跑 → 幂等替换 | **T20c**：断言"崩溃瞬间的作业状态 ≠ indexed"（这是铁律的可执行形式）；重启后检索命中 |
| **F4** | **CDC advance 前** | slot 未推进 | **不改这条链路**：`fastsearch-engine::Engine::consume_once` 已是"先 `persist(data_dir, slot_lsn)` 后 `advance_slot`"（代码注释原话："先落盘…后推进 slot —— 崩溃安全铁律"）⇒ 重放同批，apply 幂等 | **T20d**：断言作业面**没有引入新的 slot 推进点**（源码断言：`advance_slot` 的调用点数量不变） |

**作业表不进 publication ⇒ 作业状态变更不产生 CDC 事件 ⇒ 作业面自身不可能造反馈环**（红线 R2 的收益）。
这一点比 chunk 表的"列清单排除派生列"更彻底——后者只过滤列的值、**不抑制 Update 事件本身**
（`sql.rs::ddl` 注释里的 H3/R4 实测更正已经把这个坑记下来了）。

**重复投递的边界情形**：租约被抢后，旧 worker 可能仍在解析。它的 `/v1/jobs/{id}/chunks` 会因
`lease_epoch` 不匹配被 **409 拒绝**（§6.3 步骤 2）⇒ 不会出现两个 worker 都写。
`[待验证]` 若两个 worker 用**不同 profile** 交错（例如中途改了 env），结果取决于谁先拿到当期 epoch —— 属可接受的最后写入者胜，但需实跑确认无半写。

---

## 9. 不做什么（明确排除，越界即违反 ADR）

- **不做身份**：无 user/org/member/role/继承/登录/会话/密钥生命周期；**不存在任何"跳过鉴权"开关**
  （FastGPT `authTmbId=false` 那种口子在任何面上都不得出现）。
- **不做产品对象**：无 dataset/collection/source 表，无层级归属，无"文档属于哪个知识库"字段。
- **不做版本与回滚**：一文档一作业行，覆盖即覆盖（唯一索引强制）。
- **不做 UI**、不做定时同步/增量抓取/网页爬虫。
- **不做 profile 的产品化管理**：`parse_profile` 只是**一列 provenance jsonb**，没有 profile 表、没有 profile 端点、
  不能按名字引用（G7 的"可配置分块 profile"归 **KB-1.2**，本文只负责把它原样记下来）。
- **不在 server 内解析**（红线 R4），**不把原始字节塞 PG `bytea`**。
- **不做预签名 PUT / 分片上传**（§7.1 第 5 条，诚实记账：trait 里没有这个能力）。
- **不造第二个真源**：无 `DATABASE_URL` 就是 503，没有内存 job map、没有本地文件台账。
- **不关闭既有路径**：调用方仍可完全绕开摄取面，自己解析后直接 `POST /v1/index`（修订文 §3-③ 承诺"那条路一天都不会关"）。
- **不碰 KB-4**：层级权限/角色/继承依然封禁，且因本次推翻把"引擎对身份没有观点"当对价押上，封禁**更强**。

---

## 10. 测试用例与验收标准

### 10.1 纯函数单测（`fastsearch-pg`，无需 PG，CI 必跑）

| 编号 | 断言 | 守哪条 |
|---|---|---|
| T1 | `job_ddl()` 拼出的字符串**不含** `owner`/`role`/`member`/`group`/`parent`/`inherit`/`permission` 任一子串 | R1 |
| T1b | `get_job_visible_sql` / `list_documents_sql` 的 WHERE 子句**不含** `lease_owner` | R1 |
| T2 | `job_ddl()` **不含** `PUBLICATION`（大小写不敏感） | R2 |
| T3 | `job_ddl()` **不含** `CREATE EXTENSION`；不含 `pgmq`/`pg_cron`/`shared_preload` | R3 |
| T4 | `claim_jobs_sql()` 含 `FOR UPDATE SKIP LOCKED`，且含 `retry_count < max_retries`（死信不被领取） | R3 |
| T5 | `GET /v1/documents` 的响应结构体序列化后 key 集合 **== 白名单**（§7.4），并逐个断言禁列不在其中 | R6 |
| T5b | `job_ddl()` 里 `CREATE TABLE` 出现次数 == 1 | R6 |
| T6 | 状态机纯函数：合法迁移表（`queued→parsing→chunking→embedding→indexed`、任意→`failed`、`failed→queued`）之外的迁移被拒 | — |
| T7 | 200 档与 202 档共用同一 serde 结构体 ⇒ key 集合逐字相同（**两档同构**） | 易用性修正 |
| T8 | 幂等判定纯函数：§7.2 那张表的六个情形逐行断言 | G5 |
| T9 | 退避计算确定性（同 `retry_count` → 同区间；上界封顶） | — |
| T1c | `heartbeat_sql`/`finish_job_sql`/`fail_job_sql` **都**含 `lease_epoch = $` 谓词（fencing 不可漏） | §6.3 |

### 10.2 server 集成（axum 内存 app，无 PG，CI 必跑）

| 编号 | 断言 | 守哪条 |
|---|---|---|
| T10 | 无 PG 时 `POST /v1/documents` → **503**，且**在读取 body 之前**返回（用超大 body 观察不被消费） | R5 |
| T11 | 无 PG 时 `GET /v1/jobs/{id}`、`GET /v1/documents`、两个 worker 端点 → **503** | R5 |
| T11b | 源码断言：`ServerState` 里不存在任何 job 的内存容器（无 `HashMap<JobId, _>`） | R5 |
| T12 | 无标签 key（`ingest_acl_for` 拒绝）上传 → **403**；把新入口加进既有 `untagged_key_cannot_write_on_any_path` 的路径清单 | R7 |
| T13 | 缺 key / 错 key → **401**（沿用 `principal_from_headers`） | R7 |
| T14 | 请求体/表单里自带 `tenant`、`acl`、（worker 端点上）`collection`/`doc_id` → **一律被忽略**，落库值取自身份/作业行 | R7 |
| T21b | 普通用户 key 调 `POST /v1/jobs/{id}/chunks` → **403** | R7 |
| T7b | 上传超过 `FASTSEARCH_MAX_DOCUMENT_BYTES` → **413**，错误消息包含 `source_uri` 出路 | G1 |

### 10.3 PG 集成（`DATABASE_URL` env-gated，**全部 `待运行验证`**）

| 编号 | 断言 | 守哪条 |
|---|---|---|
| T15 | 两个并发 claim 事务领到**不相交**的作业（`FOR UPDATE SKIP LOCKED` 生效，无重复、无阻塞） | R3 |
| T16 | 租约过期后作业可被重领，且 `lease_epoch` 递增；旧 epoch 的回写返回 0 行 | 恢复性 |
| T17 | 同 hash 重复上传 → 跳过（不新建行、`deduplicated:true`）；hash 变化 → 覆盖（chunk 集合被替换，无残留旧 chunk） | G5 |
| T18 | `SELECT … FROM pg_publication_rel …` 断言 `fastsearch_pub` **不含**作业表 | R2 |
| T22 | （仅 A2 结论 Y）跑一次 A2 的 publication 收敛 DDL 后，T18 仍成立 | R2 / 依赖 A2 |
| T19 | 跨租户：租户 B 的 key `GET /v1/jobs/{A 的 job}` → **404**；`GET /v1/documents` 不含 A 的文档 | R7 |
| T21 | worker key 拿 job A 的 `lease_epoch` 往 job B 写 → **409/403**，且 B 的 chunk 未被改动 | R7 |
| T20a–d | §8 四个注入点的收敛（含 F3 的"崩溃瞬间状态 ≠ indexed"断言） | 不变量 #2 |
| T25 | schema 迁移可向后兼容：对**已有旧版作业表**跑新 `job_ddl()` 幂等无损（只加列） | 修订文 §5 回滚触发 |

### 10.4 CI 门禁

| 编号 | 断言 | 守哪条 |
|---|---|---|
| T23 | `hot-path-isolation` 七 crate 名单**不变**且**仍绿**（新增 worker 不得让任何一个变红） | R4 |
| T24 | **新增**：`cargo tree -p fastsearch-server -e normal` 不含 `fastsearch-ingest-worker` | R4 |
| — | 收口三件套：`cargo fmt --all --check` + `cargo clippy --workspace --all-targets -- -D warnings` + `cargo test --workspace` | DEV_SPEC §2 |
| — | 活服务验证：起 server + worker，实跑 §11 的例子（改了契约/CLI ⇒ 本仓惯例必跑） | DEV_SPEC §1 |

### 10.5 验收标准（一句话版）

**七条红线各自至少有一个红/绿可判的自动化断言**（R1→T1/T1b、R2→T2/T18、R3→T3/T4/T15、R4→T23/T24、
R5→T10/T11/T11b、R6→T5/T5b、R7→T12/T13/T14/T19/T21），
**外加** §8 四个注入点全部收敛到 PG 真值，**外加** 迭代计划 §7 的八条不变量对照清单逐条过。
任一红线的断言缺席 ⇒ 本阶段不算完成（修订文 §4 的原话是"违反即回滚"，没有断言＝无法判定是否违反）。

---

## 11. 用户使用例子

```bash
# 起 server（真源 + 对象存储；作业面要求 DATABASE_URL）
DATABASE_URL=postgres://u@localhost/kb FASTSEARCH_DATA=./data \
FASTSEARCH_KEYS="alice=acme:team-a; worker=acme:team-a" FASTSEARCH_WORKER_KEYS="worker" \
FASTSEARCH_OBJECT_DIR=./objects \
  cargo run -p fastsearch-server --bin fastsearch-server

# 起 worker（**另一个进程/镜像**：唯一持有 docparse 的地方）
DATABASE_URL=postgres://u@localhost/kb \
FASTSEARCH_SERVER=http://localhost:8642 FASTSEARCH_WORKER_KEY=worker \
FASTSEARCH_OBJECT_DIR=./objects FASTSEARCH_WORKER_CONCURRENCY=2 \
  cargo run -p fastsearch-ingest-worker --features parse --bin fastsearch-ingest-worker

# —— 小文档：一次 tool call 就完，agent 不必轮询 ——
curl -H 'X-API-Key: alice' -F collection=kb -F file=@note.md http://localhost:8642/v1/documents
# 200 {"job_id":"0f2b…","state":"indexed","searchable":true,"chunk_count":12,"poll_after_ms":0,…}

# —— 大文档 / 需 OCR：给 job，带建议轮询间隔与"当前能否检索" ——
curl -H 'X-API-Key: alice' -F collection=kb -F file=@report.pdf -F 'parse_profile={"ocr":true}' \
     http://localhost:8642/v1/documents
# 202 {"job_id":"7c1a…","state":"queued","searchable":false,"chunk_count":0,"poll_after_ms":2000,…}

curl -H 'X-API-Key: alice' http://localhost:8642/v1/jobs/7c1a…
# 200 {"state":"chunking","stage_detail":{"pages_done":12,"pages_total":80},"searchable":false,…}

# —— 文档列表：既有 chunk 行 + 作业行的派生只读视图（无 owner / 无 dataset / 无版本） ——
curl -H 'X-API-Key: alice' 'http://localhost:8642/v1/documents?collection=kb&limit=50'

# —— 超过 body 上限的大文件：自己 PUT 进桶，只把 URI 交给引擎 ——
curl -H 'X-API-Key: alice' -F collection=kb -F doc_id=big.pdf \
     -F source_uri=s3://fastsearch-assets/acme/kb/big.pdf http://localhost:8642/v1/documents

# —— 一天都不会关的老路：调用方自己解析后直接喂 chunk ——
cargo run -p fastsearch-cli --features parse --bin fastsearch -- ingest \
  --server http://localhost:8642 --key alice --collection kb --doc-id r.docx r.docx
```

---

## 12. 影响面（实施期需同步更新的文档）

- `docs/specs/19-server.md`：新增四个端点、503 判据、body 上限分层、worker key 能力位。
- `docs/specs/00-模块拆分.md`：新增 `fastsearch-ingest-worker` 一行（并给它一个 spec 号）。
- `docs/specs/` 新增 `fastsearch-ingest-worker` 的 spec（本文是 plan，不替代模块 spec）。
- `CLAUDE.md`：架构大图补一条 `POST /v1/documents → ingest_job → worker → /v1/jobs/{id}/chunks` 的支线；
  命令区补 worker 起法。**"PG 是真源"的表述不变**（作业表同样在 PG）。
- `README.md`：摄取入口从"三个客户端入口"变为"四个（多一个服务端作业面）"。
- 迭代计划 §5：KB-3.1 状态从"待设计"改为"已设计 / 待实施"；§10 待决策 #5 **保持归属 A2**。
- `docs/plans/2026-08-24-作为知识库使用-文档摄取现状与差距.md`：§5 的 C 方案（旁挂 worker）落地后回写。

---

## 13. 状态、待决策与待验证（诚实记账）

**状态**：`设计完成 / 未实施 / 全部 PG 结论待运行验证`。本机无 `DATABASE_URL`，本文没有任何一条 SQL 被实跑过。

**`[待决策]`（需评审拍板，不得在实施期擅自决定）**
1. **§6.3 worker 写入口的 ACL 来源**（本文最重要的一条）：采纳 `POST /v1/jobs/{id}/chunks` + worker key 能力位？
   —— 它是本设计唯一触到不变量 #3 表面的地方，必须评审。**照迭代计划 §5.4 字面用 `/v1/index` 会静默错配 ACL。**
2. **§6.2 适配器复用方式**：推荐下沉新 crate `fastsearch-ingest-adapter`（会动 `fastsearch-cli/src/ingest.rs`，
   **必须排在 C2 通道的 KB-1.1/1.2 之后**）；备选是 worker 直依赖 `fastsearch-cli/parse`（不推荐）。
3. **§6.2 `ObjectStore` 是否下沉独立 crate**：本文默认"worker 直接依赖 `fastsearch-engine`"（零 API 变动）。
4. **§7.1 `source_uri` 直传时的 hash 来源**：server 整块读取算 hash，还是信任调用方声明、由 worker 校验。
5. **§7.1 `workers_seen_recently` 字段**：本轮给还是留到 KB-3.3。
6. **§7.2 覆盖时旧原始字节是否保留**：默认**不保留**（保留像"版本"，逼近红线 R6）。
7. **同步档默认阈值**：`SYNC_MAX_BYTES=1MiB` / `SYNC_WAIT_MS=3000` / `MAX_INFLIGHT=32` 是拍的数，
   需要真实 agent 用例校准（观测指标已在 §7.5 备好）。

**`[待验证]` / `待运行验证`**
- 全部 §10.3 的 PG 集成用例（T15–T25）。
- `coalesce(tenant,'')` 作为唯一索引表达式在 RDS/Supabase/Neon 的可用性（§4.3）。
- axum 0.8 per-route `DefaultBodyLimit` 覆盖 router 级 layer 的顺序语义（§7.1）。
- `PgStore::set_embedding` 的幂等守卫具体实现（本文只读到 `sql.rs::ddl` 注释里对它的**描述**，未读实现）。
- 独立 `JobStore` 连接对检索延迟的实际影响（§7 的性能论证基于 `PgStore` 的 `Mutex<Client>` 结构，未实测）。
- 两个 worker 用不同 `parse_profile` 交错时的最终态（§8 末）。
- `cargo tree -p fastsearch-cli`（默认档）在 workspace 里存在一个开着 `parse` 的 worker 时，
  是否仍解析为零 docparse（feature unification 行为，§6.2 路线 1 的风险点）。

---

## 参考

- [职责边界修订：把「文档摄取作业面」收回引擎](../governance/2026-08-24-职责边界修订-摄取作业面收回引擎.md)——**本文的授权来源与七条红线**。
- [职责边界：不承担生产身份与知识库控制面](../governance/2026-08-24-职责边界-不承担身份与控制面.md)——除"摄取任务队列"外**全部继续有效**。
- [知识库引擎迭代计划 §5 / §7 / §11](2026-08-24-知识库引擎迭代计划.md)——技术方案、八条不变量、并行硬规则。
- [作为知识库使用：文档摄取现状与差距 §3](2026-08-24-作为知识库使用-文档摄取现状与差距.md)——缺口 G1–G7。
- [fail-closed 默认与运行档如实标注](2026-08-24-fail-closed默认与运行档如实标注.md)——`ingest_acl_for` 的 fail-closed 先例与本文的结构模板。
- 代码证据（路径 + 符号名）：`crates/fastsearch-pg/src/sql.rs::{ddl, PUBLICATION, COLUMNS, acl_clause, delete_chunk_visible_sql}`、
  `crates/fastsearch-pg/src/lib.rs::{PgConfig, PgStore, ensure_schema, upsert_doc, set_embedding}`、
  `crates/fastsearch-server/src/lib.rs::{router, index, apply_ingest_identity, ingest_acl_for, management_source,
  principal_from_headers, object_namespace, short_sha, image_upload, RateLimiter, Metrics}`、
  `crates/fastsearch-engine/src/lib.rs::{ObjectStore, LocalObjectStore, S3ObjectStore, put_object, validate_object_ref,
  delete_object, fetch_object_bytes, consume_once}`、
  `crates/fastsearch-cli/src/ingest.rs::{cmd_ingest, parsers, from_docparse_chunk, map_kind, map_bbox, map_image}`、
  `crates/fastsearch-core/src/filter.rs::AclFilter::visible`、`.github/workflows/ci.yml` 的 `hot-path-isolation` job。
