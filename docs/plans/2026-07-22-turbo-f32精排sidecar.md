# TurboQuant f32 精排 sidecar（磁盘）· 设计与开发计划

> 状态：**Step 1+2+3 已落地**（核心 + 引擎/server 接线 + 洞回收/压实）｜日期：2026-07-22｜上游：
> [TQ+ 评估决策 §5](../governance/2026-07-22-TQ+校准评估-不做.md)（点名此为更高价值项）、
> [TurboQuant plan](2026-07-21-向量量化压缩主索引-TurboQuant借鉴.md)、[15-vector spec](../specs/15-vector.md)。
> **代码是真源**。

## 1. 为什么

`TurboVectorIndex` 只存码（内存 ↓8~16×），但纯量化分近似召回（4-bit exact@10 **0.885**、2-bit 更需重排）。
加一个**可选精排阶段**：码粗筛 top-`k·oversample` → 读**磁盘 f32**做精确余弦重排 → top-k。恢复近精确召回，
**RAM 仍只放码**（f32 落盘、按需读）。契合「PG 真源 / 派生可重建」，纯 Rust 无数据依赖。

**与 BruteBinary 的区别**：BruteBinary f32 在 **RAM**（两阶段但不省内存）；本档 f32 在**磁盘 sidecar**，
RAM 只码 → 大集合 + 高召回 + 中等磁盘 的新权衡点。

## 2. 关键约束：无 unsafe → 不用 mmap

真 mmap（memmap2 `map()`）是 `unsafe fn`，违 `unsafe_code=forbid`。故用**安全定位文件 I/O**：
`Mutex<File>` + `seek(offset)` + `read_exact`（写同）。同价值（f32 离 RAM、按 slot 随机读），不碰 unsafe。
候选批小（`k·oversample`，~几十条/查询）→ 每查询几十次 pread 级读，够快。mmap 作未来 opt（若剖析成瓶颈）。

## 3. 数据结构

```
struct RerankSidecar {
    file: Mutex<File>,   // 磁盘 f32：slot i 在 byte offset i·dim·4；search 按 slot 读
    dim: usize,          // stride = dim·4
    next_slot: u32,      // 追加游标
    free: Vec<u32>,      // 删除回收的 slot（新 upsert 复用，控文件增长）
}
// TurboVectorIndex 加：
    rerank: Option<RerankSidecar>,   // None=纯量化（默认，零回归）
    rerank_oversample: usize,        // 粗筛候选 = k·oversample
// TurboEntry 加：
    slot: u32,                       // sidecar slot（rerank 开时有意义）
```

存**归一化** f32（turbo upsert 已归一）→ 重排 `dot(归一 query, sidecar f32)` = 余弦。

## 4. 操作

- **upsert(gid,v)**：归一化 → 量化码（同现在）。若 rerank 开：分配 slot（已存 gid 复用其 slot / `free` 弹出 /
  否则 `next_slot++`）→ 写 f32 到 `slot·stride`。
- **search**：filter+ACL 预过滤（守 #5，不变）→ 码粗筛（LUT 打分）取 top-`k·oversample` → 对候选**读 sidecar
  f32 精确余弦** → 降序 + GlobalId tie-break → top-k。rerank 关则同现在（纯量化分）。
- **delete**：移除 entry；rerank 开则 slot 入 `free`（f32 留盘、不再读）。
- **save**：码快照（`TurboSnapshot` 每 entry 加 `slot`、加 `rerank_oversample`）原子写；sidecar 已随 upsert 落盘，
  save 时 `fsync`。**格式 v2→v3**（加 slot/oversample 字段；v2 无 sidecar → load 兼容为 rerank 关）。
- **load(path,bits)**：读码快照；若 `rerank_oversample>0` → sidecar 路径 = `companion(path)`（`path.f32`）重开，
  重建 `next_slot=max(slot)+1`、`free=空`（洞待压实）。

## 5. 持久化 / 一致性

- sidecar 路径 = 码快照路径的**兄弟**（`vector.bin` → `vector.bin.f32`）；构造 `with_rerank(bits,oversample,path)`
  即定（引擎按 data_dir 传，Step 2）。
- 码快照原子（tmp→rename）；sidecar 随 upsert 增量写（非原子）。崩溃中途 → sidecar 可能半写 → **PG 真源重建**
  （守不变量 #2，派生可重建）。save 时 fsync sidecar。
- 洞（delete/覆盖遗留）：MVP 靠 `free` 复用控增长；彻底压实（重写紧凑 sidecar）留后续（同 HNSW 墓碑）。

## 6. 确定性（守 #4）

粗筛（量化分）+ 精排（f32 余弦）均确定；同分按 `GlobalId` 升序 tie-break。同输入+同快照 → 同结果。
（sidecar slot 分配顺序依赖 upsert 顺序，但**不影响检索结果**——结果只由 f32 值 + tie-break 定。）

## 7. 施工顺序（每步收口三绿 + 回写）

1. ✅ **核心**：`RerankSidecar`（安全定位 I/O，无 mmap/unsafe）+ `TurboVectorIndex::with_rerank` +
   upsert/search（码粗筛→读盘 f32 精排）/delete（slot 回收）/save（fsync sidecar）/load（重开兄弟 sidecar）+
   格式 v3。**实测**：4-bit rerank exact@10 **1.000** vs 纯量化 0.885；2-bit rerank ≫ 纯 2-bit（>+10pt）。
   +5 测（召回恢复/2-bit 受益/双文件往返逐位一致/delete 复用 slot 无脏读+文件不增长/默认关）。
   **未接引擎**（同 TurboQuant/FHT 分阶段）。
2. ✅ **接引擎/server**：`VectorBackendKind::TurboQuant{bits, rerank_oversample}`；`VectorStore::load` 首启且
   rerank>0 → `with_rerank(sidecar_path(path))`（`new`/内存态纯量化——sidecar 需磁盘）；`kind_str` 加
   `"turboquant_rerank"`（观测 + 检查点，rerank 值由 v3 快照自描述，重开取默认 `DEFAULT_RERANK_OVERSAMPLE`）；
   engine `open_with` 映射 `turboquant`/`turboquant_rerank`；server env `FASTSEARCH_TURBO_RERANK=<oversample>`。
   engine 回环测试 `persist_reopen_restores_turboquant_rerank`（首启→精排档→持久化→重开恢复→检索正确）；
   server 实跑 boot（rerank env 解析、listening）。
3. ✅ **洞回收 + 压实**：**修 reload 泄漏**——`load`/`ensure_sidecar` 经 `slot_state()` 重建 `free`（`[0,next)`
   未占的洞），reload 后新 upsert 复用洞、文件不超 `max-ever-live·stride`（此前 `free` 清空 → churn 型删增无界增长）。
   + `compact()`：把活条目 f32 重写成紧凑连续 slot（原子 写临时→rename→重开），文件缩到 `live·stride`——供
   永久删除后回收磁盘（churn 已由 `free` 控，`compact` 处理永久缩量；参照 HNSW compact）。+2 测。
   **mmap/`read_at` opt** 仍留待剖析/unsafe 决策（当前候选批小、`Mutex<File>` 未见瓶颈——不做属避 Speculative）。

## 8. 测试计划（Step 1）

1. **召回恢复**：聚簇 d=1000、4-bit，rerank(oversample=8) exact@10 ≥0.98（对比纯量化 0.885）。
2. **2-bit 尤其受益**：2-bit + rerank exact@10 显著 > 纯 2-bit。
3. **持久化往返**：save→load（双文件）后检索结果一致、rerank 档恢复。
4. **delete 复用 slot**：删后新 upsert 复用 `free`，文件不增长；结果正确。
5. **默认关零回归**：不开 rerank = 现纯量化行为逐位一致。
6. **确定性**：同输入两次同结果。
7. 维度校验 / 空库 / 覆盖同 gid 复用 slot。

## 9. 风险与回退

- 磁盘 I/O 每查询：候选批小（几十），`Mutex<File>` 串行化 rerank 读——MVP 可接受；高并发下可换 `read_at`
  （Unix 免锁）或 mmap（需 unsafe 决策）。
- 双文件一致性：崩溃靠 PG 真源重建（不变量 #2）。
- 全程 **opt-in、默认关** → 上线零风险。
