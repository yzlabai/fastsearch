# 向量量化压缩主索引（TurboQuant 借鉴）· 设计与开发计划

> 状态：**设计（施工中，Step 1 进行）**｜日期：2026-07-21｜上游：[15-vector spec](../specs/15-vector.md)、
> [A9 HNSW+量化设计](2026-06-26-A9-向量HNSW与量化设计.md)、CLAUDE.md 不变量 #1/#2/#4/#5/#6。
> 调研来源：`../../githubworks/turbovec`（Google TurboQuant 的 Rust 实现，arXiv 2504.19874）。
>
> **代码是真源**：落地以代码为准并回写本文。

## 1. 为什么 / 何时

当前向量层的量化只用作**粗筛**：`MemVectorIndex` 主存仍是**全精度 f32**（`Entry.vector`），
二值码（`code`/`l1`，`binary.rs`）只是派生出来做 Hamming/RaBitQ 粗排，最后仍用 f32 精排。
内存 = `4·d·n`，**没有压缩红利**。

TurboQuant 把量化当作**压缩主存 + 主打分**：每向量只存 2–4bit 码 + 一个 f32 修正标量，
f32 根本不存，直接在码上打分。100K×d1536 从 586MB → 37MB（2-bit，15.8×）/ 74MB（4-bit，8×），
纯量化分（无重排）recall@1 已达 0.891（2-bit）/ 0.974（4-bit），recall@4 ≥ 0.998。

**这解决 fastsearch 的"内存"约束**——外部单二进制、可气隙、内存常是绑定资源。且与三条硬约束
天然契合（见 §3）：**无训练**（契合 #2 派生可重建）、**完全确定**（契合 #4，恰补 HNSW 非确定之短）、
**纯 Rust 可做**（复用现成 `binary::Rotation`，不引 BLAS）。

**定位**：新的 opt-in 后端档，**默认仍走暴力精确**。适用 = 中小规模 + 内存受限 + 要确定性；
大规模仍走 HNSW（TurboQuant 是纯暴力扫，O(n)/query，常数极小但无图剪枝）。

## 2. 不做什么（范围收口）

- **不动默认档**：`VectorBackendKind::Brute` 及其 `BruteBinary*` 全保留、行为零变化。新档纯增量。
- **不引 BLAS / 不引 statrs / 不引新第三方依赖**：旋转复用 `binary::Rotation`（已在仓、纯 Rust）；
  codebook 用高维高斯极限 + 自实现 Simpson 积分（见 §5），不需要 turbovec 的 `statrs`/`ndarray+BLAS`。
- **不写 unsafe / 不写手写 SIMD**：守 `unsafe_code = "forbid"`（forbid 级、CI 硬门禁）。首版标量打分
  （带 per-query LUT，是 turbovec SIMD 核的标量等价）。手写 NEON/AVX 核=**Tier 3 gated**（§9），
  需产品决策是否为量化子模块局部解禁 unsafe，非本计划范围。
- **不入 PG 真源**：量化码是引擎侧**派生**产物（守 #2）；PG 侧只存 f32（pgvector）不变。
- **不做 TQ+ 校准（首版）**：TQ+（每坐标 2 参数分位对齐）是 +1.4pp 的锦上添花且需 ≥1000 有代表性
  首批，与流式 CDC 摄取有适配成本 → **Step 3**，首版退化为无校准的基础 TurboQuant。

## 3. 与现有代码的关系（不重复造轮子）

fastsearch 向量层**已与 TurboQuant 同源**（都源自 RaBitQ）：

| 能力 | 现状（`binary.rs`/`lib.rs`） | 本计划复用/升级 |
|---|---|---|
| 数据无关随机正交旋转 | ✅ `binary::Rotation`（高斯 + 改进 Gram-Schmidt，固定种子惰性建） | **直接复用**（turbovec 用 QR+BLAS，我们用现成的，绕开 C 依赖） |
| 长度重归一化无偏估计 | ✅ RaBitQ `⟨q,sign(x)⟩/‖x‖₁`（1-bit） | 升级为多-bit 版 `corr=1/⟨u_rot, x̂⟩`（§6） |
| 符号位(1-bit)量化 | ✅ `pack_signs` | 泛化到 2–4bit 标量量化（新 `quant.rs`） |
| 确定性 tie-break | ✅ 同分按 `GlobalId` 升序 | 保持 |
| filter-aware 真预过滤 | ✅ 打分前 `Filter::eval`+`AclFilter::visible` | 保持（新档同口径） |

**新增的只有**：Beta/高斯 Lloyd-Max codebook（`quant.rs`）、多-bit 位打包、码上打分、
一个只存码的后端 `TurboVectorIndex`。

## 4. 架构：新后端，trait 边界接入

```
VectorBackend (trait)
  ├── MemVectorIndex     暴力精确 / +二值粗筛（默认；f32 主存）
  ├── HnswVectorIndex    HNSW 图 + u8 量化（大规模 opt-in；近似+非确定）
  └── TurboVectorIndex   2–4bit 标量量化压缩主存（新；确定、低内存、中小规模）  ← 本计划
```

- 新档存储：`packed_codes: Vec<u8>`（位打包）+ `scales: Vec<f32>`（每向量修正）+ `metas` + 旋转矩阵
  （固定种子重建，不落盘）。**不存 f32**（内存红利来源）。
- 选择入口：`VectorBackendKind::TurboQuant { bits }`（`bits∈{2,3,4}`），engine 配置/env 决定。
- `VectorStore` 门面加一支，对 engine 排序管线零改动（trait 后换实现）。

## 5. codebook：为何用高斯极限、且无需 statrs

TurboQuant 的核心洞见：单位向量随机正交旋转后，每坐标 ≈ 服从 **Beta((d−1)/2,(d−1)/2)** on [−1,1]，
高维时 `√d·` 该坐标 → **N(0,1)**。turbovec 用 `statrs` 的精确 Beta 求 Lloyd-Max codebook。

**我们的决策：用 N(0,1) 高斯极限 + 自实现数值积分。** 理由：

1. **零新依赖**：Lloyd-Max 的条件均值 = `∫x·φ(x) / ∫φ(x)`（cell 内），只需初等高斯 pdf
   `φ(x)=exp(−x²/2)/√(2π)` + 自适应 Simpson 积分，不需要 Beta CDF/PDF → 不引 statrs。
2. **数值等价**：Gaussian Lloyd-Max 的 MSE 就是经典 Max(1960) 量化表值——
   **{2-bit: 0.1175, 3-bit: 0.03454, 4-bit: 0.009497}**，正是 turbovec `tests/distortion.rs` 对齐的
   paper 值。即高维极限下两者收敛到同一 codebook（单测断言这三个常数，见 §8）。
3. **诚实取舍**：低维（d<~256，如 GloVe d200）Beta 与高斯有偏差，此处纯高斯 codebook 略逊；
   但 fastsearch 目标负载是文本嵌入（384–3072 维），高斯极限极佳，且 **TQ+（Step 3）本就用来补
   有限维/各向异性漂移**。低维退化在 spec 诚实标注。

编码：归一化向量 → 旋转 `u_rot=R·u` → 标准化 `s[i]=u_rot[i]·√d` → 对 N(0,1) 边界量化得 code →
位打包。重建 `x̂_rot[i]=centroid[code_i]/√d`。

## 6. 长度重归一化（无偏、零查询成本）

标量量化系统性低估内积（重建方向略短）。编码时每向量存一个修正标量：

```
corr = 1 / ⟨u_rot, x̂_rot⟩      // u_rot=旋转后单位向量，x̂_rot=其码重建
```

打分（q 已归一化、q_rot=R·q）：`估计⟨q_unit,v_unit⟩ = ⟨q_rot, x̂_rot⟩ · corr`。正交旋转不改内积，
故这是 `cos(q,v)` 的**无偏估计**，零查询开销、零额外存储（一个 f32/向量）。这是 fastsearch 现有
1-bit `‖x‖₁` 校正的多-bit 正统版。

## 7. filter-aware / 确定性 / 持久化

- **filter-aware（守 #5）**：打分前用 `Filter::eval`+`AclFilter::visible` 筛候选（同暴力档口径），
  再对候选做码上打分取 top-k。精度/ACL **精确不放松**；召回由量化引入近似（诚实标注 recall@k）。
  （turbovec 的 block-skip bitmask 是 SIMD 档优化 → Tier 2/3。）
- **确定性（守 #4）**：旋转矩阵固定种子；codebook 是维度/bit 的确定函数；打分同分按 `GlobalId`
  升序 tie-break。**同输入 + 同快照 → 同结果**（不像 HNSW 非确定，这是本档的关键卖点）。
- **持久化**：快照存 `{magic, version, dim, bits, entries:[{gid, packed, corr, meta}]}`（**存码不存 f32**）；
  原子写经**共用** `atomic_write`（tmp→fsync→rename，Mem/Hnsw/Turbo 三档共享，去重复——审查采纳）。
  旋转矩阵不落盘（固定种子重建）。load 重建旋转矩阵即可打分。**加 magic + version**（借 turbovec io.rs：
  防不可信文件、平滑升级）——现有 JSON 快照无版本，本档新格式起步就带。
- **DoS 护栏**（借 turbovec）：**encode 侧** NaN/Inf 经 `normalize` 归零（不 panic、不毒化；与既有暴力档
  同口径）；**load 侧**校验 magic/version/bits∈{2,3,4}/dim∈[1,`MAX_DIM`]/码长，且**拒非有限 `corr`**
  （不可信快照 `1e39`→f32 Inf 会毒化打分——审查发现的缺口，已补守卫 + 测）；`MAX_DIM` 上限防小文件
  声明巨维触发 d×d 旋转矩阵内存爆炸。

## 8. 测试计划（纯 Rust、本环境可跑）

> **落地实测回写**（代码是真源）：下列为**已实现**的测试与实测门禁，与初稿计划的偏差已就地标注。

Step 1（`quant.rs` 核心，6 测）：
1. **§8.1 codebook MSE 门禁**：Gaussian Lloyd-Max 2/3/4-bit MSE ≈ {0.1175, 0.03454, 0.009497}
   **（±1%，实测通过——差距主由表值 4 位有效数舍入主导，闭式本身收敛到 1e-12）**。
2. **§8.2 重建误差界**：N(0,1) 采样量化→反量化经验 MSE ≤ 理论 `cb.mse()`·1.03（量化器即为 N(0,1)
   求 min-MSE）。（另有辅助 `codebook_symmetric` 结构性检查。）
3. **§8.3 打分路径一致**：§8.3a 位打包 pack→get_code 逐位往返（覆盖跨字节 3-bit）；
   **§8.3b `score`(LUT) == 直接 centroid 点积**（补足初稿只测打包、未测打分等价的缺口）。
4. **§8.4 长度重归一化减偏**：**相关** q,v（真近邻、truth 显著为正），不加 corr 系统性偏负、
   加 corr 后偏差量级显著更小（初稿"独立随机 q,v"是错误适用区，truth≈0 无低估可纠——已纠正）。
5. **§8.5 召回门禁**：合成**聚簇 d=1024**（改自初稿"d=512 高斯"——聚簇才有真实近邻结构、更贴真实嵌入），
   双指标：**exact@10**（量化 top-10 vs 精确 top-10 逐集重合）+ **cand@10-in-100**（量化作候选
   生成器的召回）。实测门禁：4-bit exact ≥0.85（实测 0.87）/ cand ≥0.98；2-bit exact ≥0.55 /
   cand ≥0.90（2-bit 是候选生成器、需重排）。**初稿目标 0.90/0.75 未达，按实测诚实下调**（exact 是
   最严指标；候选召回才是"粗筛→重排"真实关注点）。
6. **§8.6 确定性**：同数据两次 encode+score 逐位一致。

Step 2（后端 `TurboVectorIndex`，8 测 + engine 回环 + VectorStore 往返）：upsert 排序最近优先、
filter-aware 预过滤、ACL 不泄漏、覆盖+删除+delete_doc、维度校验/空库/零向量不 panic、确定性、
持久化往返（bits 自描述、内存=`⌈d·bits/8⌉`）、缺文件→空、**bits=3 档往返**、**DoS：毒丸 corr
（`1e39`→f32 Inf）/ 有条目无 dim → load 拒之**。engine `persist_reopen_restores_turboquant_backend`
回环 + VectorStore `vectorstore_turbo_roundtrip`。

## 9. 施工顺序（每步收口三绿 + 回写 spec）

1. **（施工中）`quant.rs`**：Gaussian Lloyd-Max codebook（2/3/4bit）+ 位打包/解包 + 标量 encode
   （复用 `binary::Rotation`）+ per-query LUT 标量打分 + 长度重归一化。测试 §8.1–8.6。
   **不接 engine**（自足模块，先验证最难的量化数学 + 召回）。
2. **`TurboVectorIndex` 后端**：实现 `VectorBackend`（只存码）+ `VectorStore`/`VectorBackendKind`
   接线 + 持久化（带 version 的新格式）+ engine `open_with` 检查点恢复 + server
   `FASTSEARCH_VECTOR_BACKEND=turboquant`（`FASTSEARCH_QUANT_BITS` 调档）。测试 Step 2 用例 + 真服务冒烟。
3. **TQ+ 校准**（gated）：首批 ≥1000 有代表性时 fit 每坐标 (shift, scale)、冻结复用；否则退化 identity。
   解决流式首批适配后再上。
4. **SIMD 核**（Tier 3 gated，**需产品决策**）：为量化子模块评估是否局部解禁 unsafe（隔离 crate +
   标量回退 + 严格审查），移植 nibble-LUT + block-skip filter。这是"速度打赢 FAISS"的来源，非必需。

**功能性完成 = Step 2**（经 engine/server 端到端可用、持久化、内存红利实测、召回门禁）。
Step 3/4 为显式下一迭代（非阻塞）。

## 10. 风险与回退

- **低维召回**：纯高斯 codebook 在 d<256 略逊 → 目标负载是高维文本嵌入；低维诚实标注，TQ+（Step 3）补。
- **无重排的召回**：本档不存 f32，无法 f32 精排 → 靠量化分 + 长度重归一化（recall@4≈0.998 已够多数
  RAG）；需极致精度者仍可用默认暴力档，或（下一迭代）码常驻 + f32 落 mmap sidecar 供少数候选重排
  （利用 PG 真源架构，比 turbovec 更省内存）。
- **默认档不变**（暴力），新档全程 opt-in → 上线零风险，按需开。
- 参数不达预期 → 保持暴力/HNSW，本档标 `下一迭代`，不强上。
