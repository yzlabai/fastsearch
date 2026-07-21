# devlog 2026-07-21 — TurboQuant 压缩主索引（借鉴 turbovec）

> 上游：[设计 plan](../plans/2026-07-21-向量量化压缩主索引-TurboQuant借鉴.md)；调研来源 [turbovec](https://github.com/RyanCodrai/turbovec)（Google TurboQuant，arXiv 2504.19874）。spec 回写 [15-vector](../specs/15-vector.md) §6。承接 [RaBitQ 估计器](2026-06-28-RaBitQ估计器.md)（同源 RaBitQ 血统）。

## 做什么

新增第四个向量后端 `TurboVectorIndex`：把量化从**粗筛**（现有二值档：f32 主存 + 派生 1-bit 码做 Hamming/RaBitQ 粗排）升级为**压缩主存 + 主打分**——只存 2–4bit 码 + 每向量一个 f32 修正标量，**f32 根本不存**，直接在码上打分。内存 **↓8~16×**。

**为什么契合 fastsearch**（不是随便挑的库）：TurboQuant 与现有 `binary.rs` 同源（都出自 RaBitQ）。相对现有档它多给三样，且每样都对齐硬约束：

- **无训练**（codebook 由维度/bit 解析确定）→ 守不变量 #2（派生可重建，别引"只在引擎有"的权威态）。FAISS IVFPQ 需 train，会破这条；TurboQuant 不需要。
- **完全确定**（旋转固定种子 + 解析 codebook + `GlobalId` tie-break）→ 守 #4。恰补 HNSW 的痛点：`hnsw_rs` 不可 seed → 非确定，只能 opt-in 容忍。量化暴力档同时拿到 **低内存 + 快 + 确定** 三样，是 HNSW 拿不到的甜点。
- **纯 Rust 可做**：旋转复用现成 `binary::Rotation`（绕开 turbovec 的 BLAS）；codebook 用高维高斯极限的闭式（绕开 turbovec 的 statrs）。零新依赖、零 unsafe。

## 怎么做

**Step 1 — `quant.rs` 量化核**（自足，先验证最难的数学 + 召回）：
- **Gaussian Lloyd-Max codebook**：单位向量随机正交旋转后每维高维 → N(0,1)，故可解析求最优量化点。Lloyd 迭代（边界=质心中点、质心=格条件均值），条件均值/MSE 全用初等高斯 `φ`/`Φ`（erf A&S 近似）**闭式**表出——**不引 statrs**。MSE 逐位对齐经典 Max(1960) 表 {2b:0.1175,3b:0.03454,4b:0.009497}（= turbovec 精确 Beta codebook 的高维极限，同一表）。
- **长度重归一化**：量化重建方向变短 → 内积系统性低估。编码存 `corr=1/⟨u_rot,x̂_rot⟩`，打分乘上 → 无偏，零查询成本。是现有 1-bit `‖x‖₁` 校正的多-bit 正统版。
- **LSB-first 位打包**（2/3/4bit 统一，可跨字节）+ **per-query LUT 标量打分**（`lut[i,c]=q_rot[i]·centroid[c]/√d`，按 code 查表求和——turbovec SIMD 核的**标量等价**，守 `unsafe_code=forbid`）。

**Step 2 — `TurboVectorIndex` 后端 + 接线**：
- `TurboEntry{packed, corr, meta}`（**无 f32**）；upsert 归一化→旋转（固定种子惰性建）→ `codebook.encode`；search 归一化→旋转查询→`query_lut`→**filter/ACL 打分前预过滤**（守 #5）→码上打分→top-k（`GlobalId` tie-break）。
- 持久化：带 **magic + version** 的自描述格式（`bits` 存盘决定码解码、`load` 校验 magic/version/bits/dim/码长；旋转不落盘由种子重建）+ **DoS 护栏**（NaN/Inf 经 normalize 归零、`MAX_DIM` 上限）——两者均借 turbovec。
- 接线 `VectorBackendKind::TurboQuant{bits}` + `VectorStore::Turbo` 全 match 臂 + engine `open_with` 检查点 `"turboquant"` 恢复（bits 由快照自描述，缺文件用默认）+ server `FASTSEARCH_VECTOR_BACKEND=turboquant`（`FASTSEARCH_QUANT_BITS` 调档，默认 4）。

## 怎么验证（本环境，纯 Rust + 实跑）

- **quant 6 单测**：`codebook_mse_matches_max_table`（逐位对齐 Max 表，±2%）/ `codebook_symmetric` / `pack_roundtrip_all_widths`（含跨字节 3-bit）/ `length_renorm_reduces_bias`（相关 q,v：不加 corr 系统性偏负，加 corr 偏差量级更小）/ `recall_gate_vs_exact` / `deterministic`。
- **召回实测**（合成聚簇 d=1024、纯量化分无 f32 重排）：4-bit **exact recall@10≈0.87 / 候选@10-in-100≥0.98**；2-bit 是候选生成器（exact≈0.59 低、候选@100≥0.90，配重排/oversample 用）。
- **turbo 8 单测**：排序最近优先 / filter 预过滤 / ACL 不泄漏 / 覆盖+删除+delete_doc / 维度·空库·零向量 / 确定性 / 持久化往返（**bits 由 2-bit 文件自描述恢复**、结果逐位一致）/ 缺文件→空。
- **engine 回环**：`persist_reopen_restores_turboquant_backend`——首启 TurboQuant → 检查点记 `turboquant` → 重开（默认 Brute 被覆盖）→ 向量检索命中。走的是 server 同款 `Engine::open_with`/`persist`/`ingest_vector`/`search` 真路径。
- **server 实跑**：`FASTSEARCH_VECTOR_BACKEND=turboquant` boot → `/readyz` 200、index-dir 灌入、`GET /v1/collections/kb` 报 `"vector_backend":"turboquant"`。
- **收口三绿**：`cargo fmt --all --check` + `cargo clippy --workspace --all-targets -D warnings` + `cargo test --workspace`（**297 passed / 0 failed**；vector 51）。

## 取舍 / 诚实记账

- **纯暴力扫、无图/IVF**：O(n)/query，常数极小（LUT 标量），但无剪枝。定位 = **中小规模 + 内存受限 + 要确定性**；大规模仍走 HNSW。turbovec 的 SIMD 核让它 100K 打赢 FAISS，但我们守 `unsafe_code=forbid` → 首版标量，牺牲绝对速度换内存 + 确定 + 无训练。
- **无 f32 重排**：本档不存 f32，最终结果即量化分（4-bit exact@10≈0.87 已够多数 RAG；2-bit 需外部重排）。要极致精度者用默认暴力档。
- **低维**：纯高斯 codebook 在 d<~256 略逊精确 Beta（TQ+ 校准补，见下一迭代）；目标负载是高维文本嵌入（384–3072），高斯极限极佳。
- **默认仍精确暴力、零回归**；TurboQuant 全程 opt-in。server 默认 hash embedder 不 upsert 向量（`embedded:false`，与后端无关），故实跑向量检索空——向量路径由 engine 回环测试覆盖。

## 双轴审查 + 迭代（同日续作）

跑 `/code-review`（Standards + Spec 双轴并行子代理）。**Standards：无硬违规**，不变量 #2/#4/#5/#6
逐条核过（预过滤在打分前、GlobalId tie-break、固定种子、诚实记账），数值/位打包/溢出边界安全，
零新依赖/unsafe。**Spec：忠实实现 Step 1–2，无 scope creep**（未做 TQ+/SIMD）。据审查发现迭代：

- **优化（DRY，Standards 采纳）**：三档 `save` 的原子写块（`serde→tmp→fsync→rename`）近逐字重复 →
  抽 `crate::atomic_write(path, &bytes)`（Mem/Hnsw/Turbo 共用），删 ~20 行重复。
- **硬化（Spec §7 缺口）**：`load` 原只校验 magic/version/bits/dim，**未校验 `corr`**；不可信快照的
  `corr` 可经溢出字面量 `1e39`（>f32 max，serde `as f32` 静默得 Inf、不报错）毒化打分 → 加
  `corr.is_finite()` 守卫 + `有条目但 dim 缺失` 守卫，各配 1 测。
- **补测（Spec 发现的测试计划缺口）**：①§8.2 初稿承诺的"重建 MSE ≤ 理论 MSE"未实现（被误标为
  §8.2 的对称性检查占位）→ 补 `reconstruction_mse_within_codebook_bound`；②§8.3 只测了位打包、
  未测"`score`==直接 centroid 点积"→ 补 `score_equals_direct_centroid_dot`；③§8.1 容差 ±2%→**收紧 ±1%**
  （闭式 codebook 本就精确）；④补 turbo `bits=3` 档往返 + `VectorStore` 层往返。
- **回写（代码是真源）**：plan §8 就地标注与初稿的偏差（召回 d=512 高斯→聚簇 d=1024 双指标、
  目标 0.90→实测 0.87 诚实下调、§8.4 适用区纠错）；plan §7 持久化/DoS 描述对齐实现。

**迭代后收口三绿**：fmt + clippy `-D warnings` + `cargo test --workspace` **303 passed / 0 failed**
（vector 51→**57**，+6 测）。

## 下一步

- **TQ+ 每坐标校准**（+1.4pp @1）：需解决流式 CDC 首批 ≥1000 有代表性样本的 fit 时机（首版退化为无校准的基础 TurboQuant）。
- **SIMD 核**（Tier 3 gated）：需产品决策是否为量化子模块局部解禁 unsafe（隔离 + 标量回退 + 严格审查），移植 nibble-LUT + block-skip filter——这是"速度打赢 FAISS"的来源。
- **PG 真源协同**：码常驻 RAM + f32 落 mmap sidecar 供少数候选精排（利用真源架构，比 turbovec 更省内存）。
