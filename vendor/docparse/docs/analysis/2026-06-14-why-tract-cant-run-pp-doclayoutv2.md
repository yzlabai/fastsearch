# 分析:为什么 tract 跑不了 PP-DocLayoutV2,以及要支持需要怎么做(2026-06-14)

> 背景：PP-DocLayoutV2 上车 tract 的探查（评测细节见当时 devlog；结论已并入 [status.md](../status.md) Phase 7）。
> 上一轮结论是"非 drop-in、上车成本 > UniRec"。本轮**钻到 tract 源码层逐行定位根因**,结论需要**重大修正**:挡路的**不是模型架构、不是缺算子**,而是 **tract 0.23.1 对 RT-DETR 动态检测头几个算子的具体 bug / 短板**。其中**头号坎(GatherNd)是一行 bug,已打补丁验证 → 全图能 optimize**;但后面还有第二道(TopK 吃到 TDim 数据)更难的坎。
> 证据:tract 源码 `~/.cargo/.../tract-{hir,core}-0.23.1/`;诊断 `examples/diag.rs` + `ppv2_run.rs`(spike 临时,已删);打补丁的 `vendor/tract-hir`(spike 临时,已删)。

---

## 0. 一句话

PP-DocLayoutV2 的**主干(含 18 个 GridSample 可变形注意力)tract 完全能跑**;真正卡住的是 RT-DETR **动态检测头**(选 query + NMS 去重)里几个张量索引算子,踩中 tract 0.23.1 的 **(a) 一个一行 shape 推断 bug(GatherNd)** 和 **(b) 一个符号维(TDim)漏进数据通路的短板(TopK)**。前者已证可一行修复,后者更深。

## 1. tract 是怎么工作的(理解坎在哪)

tract 跑一个 ONNX 分两个大阶段:

1. **analyse / typecheck(形状推断)**:tract 用 `InferenceModel` 把每个算子的输入/输出形状用"规则求解器"推出来。每个算子在 `tract-hir` 里有 `InferenceRulesOp::rules` 写它的形状约束。**推不出或推矛盾 → optimize 前就报错**。
2. **optimize → eval(真算)**:推断通过后转 `TypedModel`,优化,再逐节点 `eval`。

**关键背景:tract 的"符号维"(TDim)**。tract 支持动态维度(如 batch=N),用 `TDim` 表示。形状计算(`Shape`/`Gather`/`Cast` 等从张量里取出"尺寸"再参与运算)产生的 int64 张量,tract 常常标成 **TDim 类型**而非普通 i64。多数纯算术能处理 TDim,但**需要"具体数字"的算子(排序、索引、dispatch_numbers)不能**。RT-DETR 的检测头大量做"用形状算索引"的事,正好踩这个。

## 2. 根因一:GatherNd 形状推断 bug(tract-hir 0.23.1)——已证一行可修

### 现象
全图(静态化后)analyse 报:
```
Failed analyse for node "GatherND.0" GatherNd:
  outputs[0].shape[2] == inputs[1].shape[0]: Impossible to unify Val(4) with Val(1)
```

### 这个节点在干嘛
RT-DETR 编码器的 **query 选择**:从 13125 个候选锚框里挑 top-300。
- `data`(inputs[0]) = `[1, 13125, 4]`(全部锚框,4=框坐标)
- `indices`(inputs[1]) = `[1, 300, 2]`(选中的 300 个的下标,2=(batch, anchor))
- 正确输出 = `[1, 300, 4]`(选出的 300 个框)

ONNX GatherND(batch_dims=0)语义:`output = indices.shape[:-1] ++ data.shape[n:]`,其中 `n=indices.shape[-1]=2`。即尾部维度来自 **data**。

### bug 在哪(源码逐行)
`tract-hir-0.23.1/src/ops/array/gather_nd.rs` 的规则 B:
```rust
s.given_2(&inputs[1].shape[indices_rank - 1], &inputs[1].rank, move |s, n, input_rank| {
    if let Ok(n) = n.to_i64() {
        for i in 0..(input_rank - n) as usize {
            s.equals(&outputs[0].shape[indices_rank - 1 + i], &inputs[1].shape[i])?;
            //                                                  ^^^^^^^^^ 应是 inputs[0]（data）
        }
    }
})
```
**它把输出尾部维度约束成了 `inputs[1]`(indices)的维度,应该是 `inputs[0]`(data)**;循环上界也错用了 indices 的 rank。对照同包 `tract-core` 的 `compute_shape`(真算时)是**正确**的(`shape.extend(data_shape[n+batch_dims..])`)——所以**只有 hir 推断规则错,typed 真算没错**。

代入本例:`out.shape[2]` 被错约束成 `indices.shape[0]=1`,而下游已把它钉成 `4` → `unify 4 with 1` 矛盾。

> 为什么最小复现(单独一个 GatherND)"过了"?因为孤立时 `out.shape[2]` 没有别的约束,错规则把它**悄悄设成错值(1)也不报矛盾**——只有在全图里下游钉了正确值 4 才暴露。即这 bug 平时**静默产出错形状**,本图恰好撞上冲突才显形。

### 验证:打补丁 → 全图 optimize 通过 ✅
把规则 B 改成引用 `inputs[0]`(data)的尾部维度(`vendor/tract-hir` 一处改动,`[patch.crates-io]` 注入),重编 tract:
```
simplified PP-DocLayoutV2 → tract typecheck OK (3377 nodes) → OPTIMIZE OK (2850 nodes) ✅
```
**头号坎是一行 bug。** 18 个 GridSample、5 个 GatherND、4 个 TopK、ScatterND 等全部通过形状推断与优化。

## 3. 根因二:TopK 吃到 TDim 数据(tract-core 0.23.1)——更深的坎

打完补丁后,**optimize 过了但 eval 仍挂**:
```
Evaluating "TopK.3" Topk: Running legacy eval: TDim is not a number
```

### 根因
`tract-core-0.23.1/src/ops/array/topk.rs` 的 eval 里:
```rust
dispatch_numbers!(Self::inner_loop_t(dt)( ... ))   // dt = input.datum_type()
```
`dispatch_numbers!` 只接受数值类型;当输入张量的 `datum_type` 是 **TDim** 时抛 `"TDim is not a number"`(`tract-data/src/macros.rs:194`)。

TopK.3 排序的输入(`p2o.pd_op.cast.4.0 [1,300]`)是**从形状计算/Cast 链得来的 int64**,被 tract 标成了 **TDim 类型**。检测头里"按某个分数/索引排序"这类操作,数据是 dim 衍生的,于是 TDim 漏进了需要"真数字"的 TopK。

### 为什么比根因一难
- 这不是某条规则写错一行,而是 **tract 的 TDim 类型在"形状推导出的 int64"上传播过广**,渗进了数据算子。
- 要根治得么 (a) 让那条 Cast/分支产出**普通 i64** 而非 TDim,(b) 让 TopK(及其它 dispatch_numbers 算子)**容忍并具体化 TDim**,(c) 把这些 dim 计算**常量折叠**掉(onnxsim 已折掉一大批 4700→1480 节点,但这条仍残留)。
- eval 在第一个错处停,**TopK 之后是否还有同类坎(GatherElements/ScatterND/Range 也在动态头里)未知**——很可能还有几处。

## 4. 全景:坎的分布

| 区域 | 算子 | tract 0.23.1 状态 |
|---|---|---|
| 主干 backbone + FocalNet/HGNet | Conv/BN/MatMul/LayerNorm/Softmax… | ✅ 一直能跑 |
| **可变形注意力解码器** | **GridSample ×18** | ✅ **能 optimize(隔离实测)** ——头号担心是虚惊 |
| 编码器 query 选择 | GatherND ×2, TopK | ⚠️ GatherND=一行 bug(已修);TopK=TDim 坎 |
| NMS 去重 / 排序头 | GatherND ×3, GatherElements ×4, ScatterND ×2, TopK ×3, Range | ⚠️ 至少 TopK 撞 TDim;其余未验(eval 在 TopK 先停) |
| 后处理(按 scale_factor 还原 + 出 [N,8]) | Div/Clip/Concat… | 在图内,optimize 已含;待 eval 验 |

**结论**:无"缺算子"硬墙(对照 SLANet 的 `Loop`、TATR 的导出失败);是 **tract 对 RT-DETR 动态头的一串具体 bug/短板**,**一个已证一行可修,其余是 TDim 传播这类更系统的问题**。

## 5. 要支持,怎么做(三条路)

### 路 A:打 tract 补丁(fork/vendor,逐个修)— 最干净、保留整模型
- 修 GatherNd 推断规则(**一行,已验证**)。
- 修 TopK 的 TDim 问题:让 dim 衍生 int64 不标 TDim,或 TopK 容忍 TDim 具体化。
- 顺着 eval 继续验,修掉 GatherElements/ScatterND/Range 可能的同类坎(预计 2–4 处,均小而局部)。
- **优点**:官方模型整包直用,**后处理在图内,无需 Rust 重写 DETR 解码**;补丁可上游化(给 tract 提 PR,GatherNd 那条是明确 bug,大概率被接受)。
- **代价**:在上游合并前需维护一个 tract fork;`vendor/` + `[patch.crates-io]` 已验证可行;需把 PP-OCR/YOLO/UniRec 三件套回归一遍确认补丁无副作用。
- **风险**:中。坎是"一串小的",不是"一个大的";但确切数量要逐个 eval 才知道。

### 路 B:升级 tract(赌上游已修)— 最省力但需大回归
- tract 活跃开发,0.23.1 之后的版本可能已修 GatherNd / 改善 TDim。先读 changelog/源码确认,再 bump。
- **优点**:若已修,几乎零自研。
- **代价/风险**:版本跳跃 API 破坏(项目记录过 0.21→0.23 的破坏:`to_array_view`→`to_plain_array_view` 等),**全栈(PP-OCR/YOLO/UniRec 推理)必跑回归**;不保证上游已修这些冷门路径。

### 路 C:ONNX 手术绕开动态头 — 不碰 tract,但工作量大
- 把图**切到解码器 logits/box**(`[1,300,25]`+`[1,300,4]`,GridSample 在此段之前,已证能跑),后面的 query 选择/NMS/排序/还原**全部用 Rust 重写**(类似 UniRec 把 AR 循环搬宿主)。
- 或在 ONNX 里把 GatherND→Gather(已验证 Gather 在 tract 没问题)、把 TopK 的 k 钉成常量、消除 TDim 源头。
- **优点**:不维护 tract fork。
- **代价**:RT-DETR 的后处理(锚框解码 + 选择 + 去重 + scale 还原)逻辑不小,Rust 重写 + 对齐 ORT 要时间;切点要找准。
- **风险**:中高(重写正确性)。

### 推荐
**路 A 优先**(必要时 A+B 合用):GatherNd 既是明确 bug 又已验证一行可修,TopK/其余大概率同级别小修;整模型直用、后处理在图内,是最少"自研逻辑"的路。落地形态仍建议 `--layout-model ppv2` 与 DocLayout-YOLO **共存**(零回归切换 + 按页型路由),非默认替换。

## 6. 对总裁决的修正

- 之前:"非 drop-in、成本 > UniRec、暂不采用"——基于"卡在核心、像一堵墙"。
- 现在:**墙是 tract 的一串具体小 bug/短板,头号已证一行可修,GridSample 虚惊**。成本从"未知的大手术"降为"**修几处 tract 算子 + 回归**"(路 A),且可上游化。
- **更新建议**:PP-DocLayoutV2 质量更好(S3-lite 已证)+ 落地路径now清晰可行 → **值得排期做路 A 的"修到 eval 跑通 + 对齐 ORT"小专项**(估:GatherNd 已修;TopK + 余下坎逐个修,几天量级)。先不动主线,作为版面质量纵深的独立任务。

## 附:复现要点(临时件已清理)
- 静态化:onnx `update_inputs_outputs_dims`(batch=1)+ `infer_shapes`;再 `onnxsim`(4700→1480 节点)。
- GatherNd 补丁:`tract-hir/src/ops/array/gather_nd.rs` 规则 B 把 `inputs[1]` 改 `inputs[0]`、循环上界用 data rank、偏移 `n+batch_dims`。
- 验证命令:`diag.rs`(parse+optimize 门)、`ppv2_run.rs`(eval + 比 ORT)。ORT 金标准:1901.03003 p0 → 22 框,坐标在原图空间。
- 模型:`models/layout-ppv2/`(gitignored)。
</content>
