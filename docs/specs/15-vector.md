# spec · fastsearch-vector

> 模块 #5，依赖：fastsearch-core。阶段 P2。上游：[产品设计 §3.3](../plans/2026-06-24-产品设计文档.md)、需求 F10–F13。
> 状态：**已完成 v2.2**（暴力 + HNSW/u8 量化 + pgvector 直查 + **TurboQuant 压缩主索引**四档）。
> VecMeta 另含多模态 `modality/time/media`（MM1/MM4）。

## 1. 目的与范围

引擎侧向量检索后端。

- `VectorBackend` trait：upsert/delete/search（带 filter + ACL 的 **filter-aware 召回**）。
- `MemVectorIndex`：内存暴力余弦（精确、filter-aware）——正确、可测、无需模型；适合中小集合，也作正确性基线。
- **真预过滤**：过滤/ACL 在打分前/打分中施加（非后过滤），这正是超越 pgvector 后过滤召回崩的点。

四个后端档（同 `VectorBackend` trait）：`MemVectorIndex`（暴力，默认确定）、`HnswVectorIndex`
（HNSW+u8 量化，A9，大规模近似）、**pgvector 直查**（ANN 在 PG 跑，B6，经 `fastsearch-pg::PgStore::vector_search`）、
**`TurboVectorIndex`**（TurboQuant 2–4bit 压缩主索引，只存码不存 f32、内存 ↓8~16×、**无训练 + 完全确定**，
借鉴 turbovec，见 [plan](../plans/2026-07-21-向量量化压缩主索引-TurboQuant借鉴.md)）。

**不做**：嵌入计算（embed 模块）；旋转档 engine 接线 / filtered-traversal（下一迭代；二值粗筛 + RaBitQ 估计器 + 随机旋转**已落地**于 vector crate，见 §6）；CDC 自动写穿 PG embedding（**已落地**，见 §6 已知限制 + [pg spec](12-pg.md)）；**多向量 MaxSim（ColPali，M2/MM11）`gated`**——只在引擎派生层、不入 PG 真源（不变量 #1），待多模态模型与规模信封。当前后端全是**单向量**（文本嵌入产出；视觉/跨模态向量属 M1 gated，本 crate 不感知模态、只存 `VecMeta.modality` 供过滤下推）。

## 2. 公开接口

```rust
pub trait VectorBackend {
    fn upsert(&mut self, gid: GlobalId, vector: Vec<f32>, meta: VecMeta) -> anyhow::Result<()>;
    fn delete(&mut self, gid: &GlobalId) -> anyhow::Result<()>;
    fn delete_doc(&mut self, collection: &str, doc_id: &str) -> anyhow::Result<()>;
    /// filter-aware 余弦近邻：先按 filter+acl 过滤候选，再算分取 top-k。
    fn search(&self, query: &[f32], k: usize,
              filter: Option<&Filter>, acl: Option<&AclFilter>) -> anyhow::Result<Vec<Scored>>;
}
/// 过滤/ACL/引用所需的随项元数据（实现 core::FieldSource）。
pub struct VecMeta { pub kind, doc_id, collection, tenant, page, section_id, heading_path, acl, bbox, chunk_id }
```

- 距离：余弦（向量入库时归一化，内积即余弦）。返回 `Scored{id, score∈[-1,1]}`。

## 3. 行为规约

- **filter-aware**：先用 `Filter::eval` + `AclFilter::visible` 筛候选，再算余弦、取 top-k。保证选择性强的过滤不掉召回（对位 pgvector 后过滤坑）。
- **upsert 幂等**：同 gid 覆盖。
- **delete_doc**：删 collection+doc_id 全部项。
- **确定性**：同分 tie-break 按 gid 升序。
- **维度校验**：query 维度与库不一致 → 显式错误。
- **健壮**：空库返回空；零向量/NaN 防护（norm=0 时跳过或置 0 分）。

## 4. 依赖

`fastsearch-core`、`anyhow`、`hnsw_rs`（纯 Rust，无 C 依赖）、`serde(_json)`、`tempfile`。

## 5. 测试用例

1. upsert 3 向量 + search → 按余弦降序、top-k 截断、Scored.id 正确。
2. filter-aware：加 `kind=table` 过滤后只在 table 项里排（验证预过滤，不是先 top-k 再过滤）。
3. ACL：越权项不出现在结果。
4. upsert 覆盖：改向量后排序变化。
5. delete / delete_doc 生效。
6. 维度不匹配报错；空库空结果；零向量不 panic。
7. 确定性：同分按 gid。

## 6. 验收标准与状态

- [x] v1 完成：VectorBackend trait + MemVectorIndex（filter-aware 余弦，真预过滤）+ 7 单测绿（余弦排序/预过滤/ACL/覆盖/删除/维度校验/零向量）。clippy 净、fmt 净。已接入 engine 做真混合融合（engine 9 测试含 real_hybrid/vector_only）。
- [x] v1.1（2026-06-25）：**持久化** `MemVectorIndex::save/load`（JSON 快照 + 原子写 tmp→fsync→rename；存归一化向量，load 行为不变）+ `len/is_empty/dim`。+2 单测（往返、缺文件→空）。供 engine 落盘恢复（不重嵌）。压缩二进制格式（bincode）为后续优化。
- [x] v2（2026-06-26，A9）：**HnswVectorIndex**（hnsw_rs，纯 Rust）——增量 insert + 墓碑删除 +
  over-fetch 后过滤 + **u8 量化图（省 ~4× 图内存）+ 全精度 f32 重排**（recall@10≈0.99）+ 持久化
  （存向量、load 重建图）+ **小集合回退暴力**（≤1000 精确）。`VectorStore` 门面（在 engine）+
  `VectorBackendKind` 选档 + 检查点记录/恢复。诚实：HNSW 档近似 + 非确定（hnsw_rs 不可 seed），
  默认暴力档仍完全确定。
- [x] v2.1（2026-06-26，B6）：**pgvector 直查档**——`fastsearch-pg::PgStore::vector_search`
  （filter/ACL→SQL 下推 + iterative scan + Rust 精确后过滤 + 完整引用）；接 engine（block_in_place
  同步↔异步桥）+ server `FASTSEARCH_VECTOR_BACKEND=pgvector`。Docker 实测。

**已知限制 / 下一迭代：**
- ✅ **二值量化（1-bit）两阶段粗筛已落地**（2026-06-27，RaBitQ/BQ 核心）：`MemVectorIndex::with_binary_prefilter(oversample)` 开启——符号位 bit code（`binary.rs` `pack_signs`）Hamming 粗筛 top-`k·oversample`（`popcount`，~`d/64` 字操作 vs 精确 `d` flops）→ f32 精确重排，filter/ACL 仍在粗筛前（守 #5）、重排 + GlobalId tie-break 仍确定。+5 单测：全覆盖 oversample **逐条等于精确**、recall@10 ≥0.85(oversample=8)、filter-aware、pack/hamming 原语。**默认仍精确暴力**（`None`，零回归）。**✅ 已后端化**（2026-06-27）：`VectorBackendKind::BruteBinary(oversample)` + `VectorStore` 接线（`kind_str="brute_binary"`、与 brute 共享 on-disk f32 格式、load 后翻档）+ engine `open_with` 检查点恢复（记格式、oversample 取默认 `DEFAULT_BINARY_OVERSAMPLE=8`，同 HNSW 参数策略）+ server `FASTSEARCH_VECTOR_BACKEND=brute_binary`（`FASTSEARCH_BINARY_OVERSAMPLE` 调档）。+2 测试（VectorStore 落盘往返保持粗筛档；engine 重开恢复 `brute_binary` 覆盖默认）+ server 实跑 boot 200。
- ✅ **RaBitQ 无偏估计器粗筛已落地**（2026-06-28，替换对称 Hamming 为粗排打分）：粗筛改用 `binary::rabitq_estimate` = `⟨q, sign(x)⟩ / ‖x‖₁`——**用查询真实分量**（非对称）+ **逐向量 `‖x‖₁` 校正**（`Entry.l1`，由归一化向量派生、不落盘）。比 Hamming（只数符号一致维、丢 `q` 幅度）更接近真实余弦：同符号库向量 Hamming 必打平、估计器仍能按 `q` 幅度分开。只读 `code`（不取全精度 x），内存轻；估计降序 + GlobalId tie-break 仍确定；filter/ACL 仍在粗筛前（守 #5）；全覆盖 oversample 仍逐条等于精确。**实测**：recall@10 估计器 **0.87** vs Hamming **0.71**（oversample=4，~16pt 增益）。+2 单测（`estimate_separates_what_hamming_ties`、`rabitq_estimator_beats_hamming` 头对头 ≥+5pt 门禁）。devlog [2026-06-28](../devlog/2026-06-28-RaBitQ估计器.md)。
- ✅ **RaBitQ 随机旋转已落地**（2026-06-28 迭代②）：opt-in `MemVectorIndex::with_binary_prefilter_rotated(oversample)`——量化前对向量/查询做一次**数据无关固定种子正交变换**（`binary::Rotation`，高斯随机 + 改进 Gram-Schmidt；首次 upsert 惰性建矩阵）把信息摊匀，符号码更有信息 → 对**各向异性**（能量集中少数维）数据召回大增。正交不改内积 → 精排仍用原向量、全覆盖 oversample 仍逐条等于精确；固定种子 → 多副本/重开同矩阵（无需持久化）、确定。**实测**：各向异性集 recall@10 旋转 **0.97** vs 不旋转 **0.78**（oversample=3，~19pt 增益）。+3 单测（各向异性 A/B ≥+5pt、全覆盖=精确、确定性）。**搜索策略不落盘**（同 `binary_oversample`，调用方设；`load` 默认不旋转）。
- ✅ **旋转档 engine/server 接线已落地**（2026-06-28 迭代③）：`VectorBackendKind::BruteBinaryRotated(oversample)` + `VectorStore`（`kind_str="brute_binary_rotated"`、`load` 重建旋转矩阵 + 旋转空间重算 code）+ `MemVectorIndex::set_rabitq_rotation`（load 后翻档）+ engine `open_with` 检查点恢复（记 `brute_binary_rotated`）+ server `FASTSEARCH_VECTOR_BACKEND=brute_binary_rotated`（`FASTSEARCH_BINARY_OVERSAMPLE` 调档）。+2 测试（VectorStore 落盘往返保持旋转档；engine 重开恢复 `brute_binary_rotated` 覆盖默认）。**下一迭代**：查询分平面量化恢复 popcount 级粗筛速度。
- ✅ **HNSW 自适应过取（强过滤召回安全网）已落地**（2026-06-28 迭代④）：强选择性 filter/ACL 下固定 `over_fetch` 候选会被过滤殆尽 → 召回崩；现有 filter/acl 时若过滤后不足 `k` 就**翻倍 `want` + 调高 `ef` 重搜**，上限=全集（最坏退化为对 filter 精确的全扫，守不变量 #5）。无 filter/acl 则单发不升级；同输入→同升级路径→同结果。+1 测试 `adaptive_overfetch_selective_filter_recall`（~1.7% 命中率 filter，recall@10 ≥0.85，对比固定过取仅 ~0.13）。
- ✅ **真·图内 filtered-traversal 已落地**（2026-06-30）：hnsw_rs 0.3.4 提供 `search_filter(data, k, ef, Option<&dyn FilterT>)`（`FilterT::hnsw_filter(d_id)` 在遍历期裁剪），原 spec「需 hnsw_rs 支持」的判断有误——上游本就支持。现有 filter/acl 时把 filter+ACL 谓词下推进遍历（谓词=活条目且过 `Filter::eval`+`AclFilter::visible`，同口径精确后过滤兜底），结果堆只收合规候选、命中更密；遍历仍穿过被裁节点保连通性。自适应过取保留作安全网。墓碑亦经谓词滤除。无 filter/acl 走纯 `search`（零谓词开销）。确定：同输入→同遍历→同结果。+1 测试 `filtered_traversal_acl_no_leak_at_scale`（n=1500>暴力回退阈值、ACL 谓词，断言无越权泄漏 + 有合规召回）。
- ✅ **HNSW 墓碑自动压实已落地**（2026-06-30）：删除/更新只置墓碑（`entries[id]=None`、向量留图中），长跑高频 upsert/delete 下 `entries`/图原会无界增长。现删除/更新后按比例**自动压实**——`HnswVectorIndex::compact`（用活条目原地重建图 + 稠密 id 映射，等价 `save`→`load` 但纯内存、不落盘）经 `maybe_compact` 在「总槽位 > `COMPACT_MIN_TOTAL`(32) 且墓碑过半（dead > live）」时触发，「过半才压实」给摊还 O(1) 重建代价（仿动态数组倍增）。纯新增 dead=0 永不触发（不扰 bulk load）；活集/检索语义不变。亦可手动 `compact`。+2 单测（`auto_compaction_reclaims_tombstones` 跨阈值后槽位回落=活条目数、墓碑清零；`manual_compact_preserves_live_set` 暴力档压实前后 top-k 完全一致）。守不变量 #6（墓碑增长项）。
- HNSW 大 N 的 p95 与暴力交叉点实测（见 [容量/SLO](../governance/2026-06-26-容量与SLO.md)）；查询分平面量化恢复二值粗筛 popcount 级速度。
- ✅ pgvector 直查的 **CDC 自动写穿已落地**（2026-06-27，B6 续作）：engine `apply_upsert` 在配了 `set_pg_vector` 时把嵌入写回 PG `embedding` 列（`PgStore::set_embedding`，block_in_place 桥），而非引擎派生索引——直查读 PG、写也归 PG，闭环。**CDC 反馈环**经"列清单 publication 排除派生列（`embedding`/`embed_model`/`updated_at`）+ `set_embedding` 幂等守卫（0 行不复制）"双防线断开。Docker pgvector 验证（见 [pg spec §7](12-pg.md)、devlog）。
- 向量经 CDC 落地路径自动嵌入（`engine.set_embedder` + `apply_upsert`）或 `ingest_vector` 灌入。
- ✅ **TurboQuant 压缩主索引已落地**（2026-07-21，借鉴 [turbovec](https://github.com/RyanCodrai/turbovec) / Google TurboQuant，arXiv 2504.19874）：新后端 `TurboVectorIndex`（`turbo.rs`）**只存 2–4bit 量化码 + 每向量一个 f32 修正标量**（`⌈d·bits/8⌉` 字节/条 + 4B），f32 根本不存 → 内存 **↓8~16×**（vs 现有档全 f32 主存）。量化核 `quant.rs`：**Gaussian Lloyd-Max codebook**（旋转后坐标高维→N(0,1)，解析求最优量化点，MSE 逐位对齐经典 Max 表 {2b:0.1175,3b:0.03454,4b:0.009497}——**不引 statrs/BLAS**，用初等高斯 pdf/cdf 闭式 + 复用现成 `binary::Rotation`）+ **长度重归一化**（`corr=1/⟨u_rot,x̂_rot⟩`，无偏内积估计、零查询成本，是现有 1-bit `‖x‖₁` 校正的多-bit 正统版）+ per-query LUT 标量打分（turbovec SIMD 核的**标量等价**，守 `unsafe_code=forbid`）。**无训练**（守 #2）、**完全确定**（旋转固定种子 + 解析 codebook + `GlobalId` tie-break，守 #4——相对 HNSW 非确定的关键卖点）、**filter-aware 真预过滤**（守 #5）。持久化：带 magic/version 的自描述格式（`bits` 存盘、旋转不落盘由种子重建）+ DoS 护栏（NaN/Inf、`MAX_DIM`）。接线 `VectorBackendKind::TurboQuant{bits}` + `VectorStore::Turbo` + engine `open_with` 检查点恢复（`"turboquant"`）+ server `FASTSEARCH_VECTOR_BACKEND=turboquant`（`FASTSEARCH_QUANT_BITS` 调档，默认 4）。**实测**（纯量化分、无 f32 重排、合成聚簇 d=1024）：4-bit exact recall@10≈**0.87** / 候选@100≥**0.98**；2-bit 是候选生成器（exact 低、候选@100≥0.90，需重排）。+14 单测（quant 6：MSE/对称/位打包往返/无偏/召回门禁/确定性；turbo 8：排序/预过滤/ACL/覆盖删除/维度·空·零/确定/持久化往返 bits 自描述/缺文件）+ engine `persist_reopen_restores_turboquant_backend` 回环 + server 实跑 boot（`vector_backend:"turboquant"`）。**默认仍暴力精确、零回归**；TurboQuant 全程 opt-in。**下一迭代**：TQ+ 每坐标校准（+1.4pp，需流式首批 fit 适配）；SIMD 核（Tier 3 gated，需产品决策为量化子模块局部解禁 unsafe）；PG 真源 f32 落 mmap sidecar 供少数候选精排（比 turbovec 更省内存）。
- ✅ **旋转维度上限收紧 + DoS 闸补齐**（2026-07-21，复审跟进，见[决策记录](../governance/2026-07-21-向量旋转维度上限与DoS.md)）：物化 d×d 旋转的上限 `65536→8192`（`binary::MAX_ROTATION_DIM`，覆盖所有真实嵌入模型 ≤4096 +2× 余量，最坏分配 17GB→**268MB**）；闸移到**唯一分配点** `Rotation::new`（改返 `Result`，`dim==0||>MAX`→`Err`），turbo + **`BruteBinaryRotated`（此前漏防）** 都经此、不可绕过（`ensure_rotation`/`set_rabitq_rotation` 链改 Result 传播）。+4 测（`rotation_dim_guard`、turbo upsert/load 超维拒含小文件声明巨维、旋转档 upsert 超维拒）。真正根治=结构化旋转（FHT，O(d·log d)、无 d×d 矩阵），下一迭代。
- ✅ **FHT 结构化旋转替物化 d×d（turbo，Step 1+2）**（2026-07-22，见 [FHT plan](../plans/2026-07-22-FHT结构化旋转.md)、[devlog](../devlog/2026-07-22-FHT结构化旋转.md)）：`TurboVectorIndex` 的旋转从物化 d×d 高斯矩阵（存 O(d²)/建 O(d³)/apply O(d²)）换成 **`fht::StructuredRotation`**（随机化 Walsh-Hadamard：零填充到 `D=next_pow2(d)` → 3 轮 [±1 符号翻转 + 原地归一化 WHT]）——存 **O(d)**、apply **O(d·log d)**、零建矩阵、纯 Rust 无 unsafe/无依赖。正交→保内积（`⟨R(q),R(v)⟩=⟨q,v⟩`）→ 量化估计器/精排语义不变；固定种子→确定。码/打分按 D，快照存原始 d、D 派生，**格式 v1→v2**（码空间 d→D 不兼容，load 拒 v1；turbo 新档无生产数据）。**无条件替换**（非 opt-in）：实测 `recall_vs_exact_fht`（d=1000→D=1024）4-bit exact@10 **0.885** vs 物化 ~0.87 **同量级/略优**→ 召回不劣。代价：非 2 幂维码宽 +33%（换 apply ~100–200×↓、存储 MB→KB）。+8 测（fht 原语 7 + turbo 召回 1）；物化 `Rotation` 仍供 `BruteBinaryRotated`。**下一迭代**：BruteBinaryRotated 亦切 FHT + 放宽其 MAX_DIM。
- ✅ **FHT 成唯一旋转实现 / 退役物化 d×d（Step 3）**（2026-07-22）：`MemVectorIndex` 的 `BruteBinaryRotated` 档亦从物化 `binary::Rotation` 切 `fht::StructuredRotation`（code/l1 转 D=next_pow2(d) 空间、精排仍用原 d 向量；该档快照存 f32、code/l1 派生 → **无格式变更**）。**删除 `binary::Rotation`/`next_unit`**（FHT 成唯一旋转实现）；`MAX_ROTATION_DIM`→`fht::MAX_DIM`（含义从"d×d DoS 闸"变"sanity/`next_pow2` 溢出界"，8192 不变——真实维 ≤4096，放宽属 speculative 故不做）；`ensure_rotation`/`set_rabitq_rotation` revert 非 Result（FHT 无巨分配，显式 MAX_DIM 早检即够，优雅收掉上轮 fallible-Rotation 机制）。实测 `rabitq_rotation_helps_anisotropic`（FHT）rot **0.925** vs norot 0.775（+15pt，过 +5pt 门禁；略低于物化 0.97，ROUNDS=3 经扫描为 turbo 主用例最优）。删 `rotation_dim_guard` −1 → workspace 314 绿。
- ⛔ **TQ+ 每坐标校准：评估 → 不做**（2026-07-22，见[决策记录](../governance/2026-07-22-TQ+校准评估-不做.md)）：实测 FHT 旋转后**高维目标**（d=1024 聚簇）坐标离理想 N(0,1) 的 5/95 分位仅漂移 **0.04**（各向异性 0.14、低维 0.06）→ TQ+ 至多做 ~2.4% 重标定、召回增益亚 0.5pp（与 turbovec「高维 ~0」一致）。零收益换数据依赖（削「无训练」卖点）+ 快照 v3 + 流式 fit 复杂度 → **不做**。更高价值替代：f32 精排 mmap sidecar（补 turbo 召回、PG 真源协同）/ SIMD 核（需 unsafe 产品决策）。
- ✅ **TurboQuant f32 精排 sidecar（磁盘，Step 1 核心）**（2026-07-22，见 [plan](../plans/2026-07-22-turbo-f32精排sidecar.md)、[devlog](../devlog/2026-07-22-turbo-f32精排sidecar.md)）：`TurboVectorIndex::with_rerank(bits, oversample, sidecar_path)` 开可选精排——码粗筛 top-`k·oversample` → 读**磁盘 f32** 精确余弦重排 → top-k，**RAM 仍只放码**（f32 落兄弟 sidecar 文件 `vector.bin.f32`、按需读）。**无 mmap**（mmap 需 unsafe）：安全定位 I/O（`Mutex<File>`+`seek`+`read_exact`）。**实测**：4-bit rerank exact@10 **1.000** vs 纯量化 0.885（完全恢复召回）；2-bit rerank ≫ 纯 2-bit（>+10pt）。持久化 **v2→v3**（码快照加 `slot`/`rerank_oversample`、f32 在兄弟 sidecar 增量写 + save fsync、load 重开）；delete 回收 slot 复用（无脏读、文件不增长）；崩溃靠 PG 真源重建（守 #2）；确定（粗筛+精排+GlobalId tie-break，守 #4）。**默认关（`new`）零回归**。+5 测。**下一迭代**：接引擎/server（`VectorBackendKind::TurboQuant{bits, rerank_oversample}` + sidecar 路径 + env）；sidecar 压实去洞。
- ✅ **f32 精排 sidecar 接引擎/server（Step 2）**（2026-07-22）：`VectorBackendKind::TurboQuant{bits}`→`{bits, rerank_oversample}`（0=纯量化）；`VectorStore::load` 首启且 rerank>0 → `with_rerank(sidecar_path(path))`，`new`（内存态）纯量化（sidecar 需磁盘）；`kind_str` 加 `"turboquant_rerank"`（观测/检查点，oversample 由 v3 快照自描述、重开取 `DEFAULT_RERANK_OVERSAMPLE=8`）；engine `open_with` 映射 `turboquant`/`turboquant_rerank`；server `FASTSEARCH_TURBO_RERANK=<oversample>`。engine 回环 `persist_reopen_restores_turboquant_rerank`（首启→精排档→persist→重开恢复→检索命中）+ server 实跑 boot 解析 env。workspace 320 绿。
