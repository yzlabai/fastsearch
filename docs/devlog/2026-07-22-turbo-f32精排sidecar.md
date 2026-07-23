# devlog 2026-07-22 — TurboQuant f32 精排 sidecar（Step 1 核心）

> 上游：[设计 plan](../plans/2026-07-22-turbo-f32精排sidecar.md)、[TQ+ 评估 §5](../governance/2026-07-22-TQ+校准评估-不做.md)（点名此为更高价值项）。spec 回写 [15-vector](../specs/15-vector.md) §6。

## 做什么 / 为什么

`TurboVectorIndex` 只存码（RAM ↓8~16×），但纯量化分近似召回（4-bit exact@10 0.885、2-bit 更低）。
加**可选精排**：码粗筛 top-`k·oversample` → 读**磁盘 f32** 精确余弦重排 → top-k。**RAM 仍只放码**、
f32 落盘按需读 → 大集合 + 高召回 + 中等磁盘的新权衡点。契合「PG 真源 / 派生可重建」，无数据依赖。

## 关键决策：不用 mmap（守 unsafe_code=forbid）

真 mmap（memmap2 `map()`）是 `unsafe fn`。故用**安全定位文件 I/O**：`Mutex<File>`+`seek`+`read_exact`/
`write_all`。同价值（f32 离 RAM、按 slot 随机读），不碰 unsafe。候选批小（`k·oversample` 几十条）→
每查询几十次 pread 级读，够快。mmap/`read_at`（免锁）作未来 opt。

## 怎么做

- **`RerankSidecar`**：`file: Mutex<File>` + `dim`（stride=dim·4）+ `next_slot` + `free`（删除回收 slot）。
  slot `i` 存归一化 f32 于 byte offset `i·dim·4`。`alloc`（复用 free / 追加）、`write`、`read_batch`（一次锁读多条）、`fsync`。
- **`TurboVectorIndex`**：加 `rerank: Option<RerankSidecar>` + `rerank_oversample` + `rerank_path`；
  `with_rerank(bits, oversample, sidecar_path)` 构造（默认 `new` 无精排、零回归）。sidecar 惰性建（首次 upsert dim 已知）。
- **upsert**：量化码（同前）+ 若 rerank 开写归一化 f32 到 slot（同 gid 复用其 slot / free 弹 / 追加）。
- **search**：filter+ACL 预过滤（守 #5，不变）→ 码量化打分粗排 → rerank 开则 top-`k·oversample` 读 sidecar
  f32 精确余弦重排 → top-k；关则纯量化分 top-k（与旧逐位一致）。
- **delete/delete_doc**：移除 entry + slot 入 `free`。
- **持久化 v2→v3**：码快照加 `slot`/`rerank_oversample` 字段（`#[serde(default)]`）；**f32 在兄弟 sidecar
  文件**（`vector.bin`→`vector.bin.f32`，随 upsert 增量写），save 时 fsync。load 重开兄弟 sidecar、
  重建 `next_slot=max(slot)+1`。
- **一致性**：码快照原子（tmp→rename）为准；sidecar 多余 slot（洞）被无视、referenced slot 有正确 f32；
  崩溃/半写靠 PG 真源重建（守 #2）。**确定**（守 #4）：粗筛+精排均确定、GlobalId tie-break；slot 分配顺序
  不影响结果（结果只由 f32 值 + tie-break 定）。

## 怎么验证

- **召回恢复**（`rerank_recovers_recall`，聚簇 d=1000）：4-bit rerank exact@10 **1.000** vs 纯量化 **0.885**——
  **精排完全恢复召回**，RAM 仍只码。
- **2-bit 尤其受益**（`rerank_helps_2bit_most`，d=768）：2-bit rerank ≫ 纯 2-bit（>+10pt；2-bit 本是候选生成器）。
- **双文件持久化往返**（`rerank_persistence_roundtrip`）：save→load（码快照 + 兄弟 sidecar）后检索**逐位一致**、
  rerank 档恢复。
- **delete 复用 slot**（`rerank_delete_reuses_slot`）：删后新 upsert 复用回收 slot——① 新向量检索到（**无脏读**）、
  删的不出现；② sidecar 文件**不增长**（size=活 slot 数·stride）。
- **默认关零回归**：`new` `rerank_oversample()==0`；既有 68 测在 rerank 关下全绿。
- **收口三绿**：fmt + clippy `-D warnings` + `cargo test --workspace` **319 passed / 0 failed**
  （vector 68→**73**，+5）。

## 取舍 / 诚实记账

- **磁盘换召回**：rerank 档 = 码 RAM + f32 磁盘（disk footprint = f32）；vs BruteBinary 的 f32 全 RAM。
  新权衡点：大集合 + 近精确 + 中等磁盘。
- **每查询磁盘 I/O**：候选批小；`Mutex<File>` 串行化 rerank 读（高并发下可换 `read_at` 免锁 / mmap 需 unsafe）。
- **洞**：delete/覆盖遗留 slot 靠 `free` 复用控增长；彻底压实（重写紧凑 sidecar）留后续。
- **v3 不向后兼容 v2**（加字段 + 语义）：turbo 新档无生产数据，load 拒 v2。
- 全程 **opt-in、默认关** → 零回归、上线零风险。

## Step 2（同日续作）：接引擎 / server

把 rerank 档接到端到端，从 server 环境变量即可用：

- **`VectorBackendKind::TurboQuant{bits}` → `{bits, rerank_oversample}`**（0=纯量化）。
- **`VectorStore::load`**：首启（快照缺）且 rerank>0 → `TurboVectorIndex::with_rerank(bits, oversample,
  sidecar_path(path))`（sidecar=码快照兄弟 `path.f32`）；快照存在 → `load` 自描述（v3 快照带 rerank）。
  **`VectorStore::new`（内存态，无 data_dir）→ 纯量化**（sidecar 需磁盘落点，仅经持久化引擎启用）。
- **`kind_str` 加 `"turboquant_rerank"`**（观测 + 检查点区分，同 `brute_binary_rotated` 模式）；rerank 具体
  oversample 由 v3 快照自描述，重开取默认 `DEFAULT_RERANK_OVERSAMPLE=8`（同 HNSW/二值档参数策略）。
- **engine `open_with`**：检查点 `turboquant`/`turboquant_rerank` → 对应 kind（rerank 值取默认，路径存在时快照覆盖）。
- **server**：`FASTSEARCH_TURBO_RERANK=<oversample>` 开精排（0/未设=纯量化）。
- **验证**：engine 回环 `persist_reopen_restores_turboquant_rerank`（首启 `kind_str="turboquant_rerank"`→
  ingest→persist→重开【默认 brute】→检查点 + v3 快照恢复精排档→检索命中）；server 实跑 boot（rerank env
  解析、listening、无 crash）。**收口 workspace 320 绿**（vector 73 + engine +1）。
  （注：server 默认 hash embedder 不 upsert 向量，故 boot 不建 sidecar——向量/精排路径由 engine 回环 + 库测覆盖。）

## 下一步

- Step 3（可选）：sidecar 压实（去洞）；mmap/`read_at` opt（需剖析 / unsafe 决策）。
