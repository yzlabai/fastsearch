# spec · fastsearch-ingest-worker

> 模块 #14（[00-模块拆分](00-模块拆分.md)），上游：[FS-303 实施计划](../plans/2026-09-01-FS-303-独立摄取worker与MCP闭环.md)。
> 状态：✅ FS-303 已完成；FS-304 故障分类与恢复契约已实施（2026-09-01）。

## 1. 目的与范围

唯一持有 docparse 的独立作业进程。它从 PostgreSQL claim durable job、从配置一致的 ObjectStore
读取原始字节、调用 adapter 解析，并把无身份 chunk 交回 server 原子发布。

不嵌入 server，不直接写 `fastsearch_chunks`，不创建身份或产品对象，不拥有检索派生索引。

## 2. 公开接口与配置

```rust
pub struct WorkerConfig { /* database/server/lease/concurrency/resource limits */ }
impl WorkerConfig { pub fn from_env() -> anyhow::Result<Self>; }
pub async fn run_from_env() -> anyhow::Result<()>;
pub async fn run(config: WorkerConfig, objects: Arc<dyn ObjectStore>) -> anyhow::Result<()>;
```

必填：`DATABASE_URL`、`FASTSEARCH_WORKER_KEY`，以及 S3 五项或 `FASTSEARCH_OBJECT_DIR`。
可选：`FASTSEARCH_SERVER`、`FASTSEARCH_WORKER_CONCURRENCY`(默认1)、`_LEASE_MS`(60000)、
`_HEARTBEAT_MS`(20000)、`_IDLE_MIN_MS/_MAX_MS`(500/5000)、PG 表名与文档字节上限。

## 3. 状态与 fencing 行为

1. 每并发槽创建独立 `JobStore`，`claim(owner,1,lease)`；空队列指数退避，PG connect/claim
   短断连也退避并丢弃旧 client 重建连接，不退出 worker 进程。
2. claim 后启动独立 heartbeat；对象读取与 docparse 放入 `spawn_blocking`。
3. parse_profile 校验 chunk profile、images、ocr/tables/vlm；请求未编译的重能力必须报错而非降级。
4. 解析完成 POST `state=chunking`，再 POST job-scoped chunks。每次带同一
   `lease_job_id/lease_owner/lease_epoch`；409/404 立即按 lease lost 停止。
5. 失败经 worker status 端点记录 stage、截断错误、确定性退避时间和显式 `retryable`。
   对象 NotFound/Transient、server 408/425/429/5xx/transport 可重试；非法 profile/解析输入、
   对象越权/元数据/大小、server 其余 4xx 为 terminal；404/409 只表示 lease lost，401/403
   表示 worker API 凭据缺少 `worker` capability，并终止进程。无法回报时依赖租约过期重领。

worker chunk wire 白名单仅含 chunk 内容/媒资/metadata/searchable，不含
`collection/doc_id/tenant/acl`。server 从 fenced job 行恢复身份。`store_media` 仅允许 inline/object。

## 4. 依赖

pg（claim）、engine（既有 ObjectStore 实现）、ingest-adapter、core、tokio、ureq、tempfile。
server/engine/默认 CLI 不反向依赖 worker。

## 5. 测试用例

- wire 序列化禁字段；profile 类型/范围与重 feature fail-loud；安全临时后缀；退避确定性。
- 故障矩阵逐项锁定 HTTP、对象存储、profile/parse 的 retryable/terminal/fencing/fatal 分类，
  failure wire 必须携带分类且旧 worker 缺省为 retryable。
- 真 PG：并发 claim 不重复、租约续期、旧作业租约发布被拒、PG 断连后原 worker 重连、
  worker 重启后重领并收敛。
- 真进程：上传 Markdown → worker → indexed → search → citation；worker/server 可独立启停。
- hot-path dependency tree、fmt、clippy、workspace tests 全绿。

## 6. 验收与回滚

上述真 PG/真进程用例与双轴 review 全部通过才从“代码完成”改“已完成”。回滚只需停止 worker；
既有 `/v1/index` 与 CLI 客户端解析不受影响，PG job 仍保留为唯一状态真值。
