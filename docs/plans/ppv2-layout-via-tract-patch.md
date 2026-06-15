# 迭代计划 · 让 tract 跑通 PP-DocLayoutV2(路 A:补丁 tract + 内嵌版面)

> 承接深挖结论 [analysis/2026-06-14-why-tract-cant-run-pp-doclayoutv2.md](../analysis/2026-06-14-why-tract-cant-run-pp-doclayoutv2.md) 与对照实测 [testresults/2026-06-14-ppv2-tract-gate-and-unirec-alignment.md](../testresults/2026-06-14-ppv2-tract-gate-and-unirec-alignment.md)、spike 计划 [layout-model-pp-doclayoutv2-spike.md](layout-model-pp-doclayoutv2-spike.md)。
>
> **立项依据(已证事实)**:① PP-DocLayoutV2 质量明显优于现役 DocLayout-YOLO(25 类语义 + 原生阅读顺序 + 召回,S3-lite 已证);② 落地的"墙"是 **tract 0.23.1 对 RT-DETR 动态检测头的具体 bug/短板**,不是缺算子——`GridSample×18` tract 能 optimize(虚惊),头号坎 **GatherNd 推断 bug 是一行,打补丁后全图 optimize 通过(已验证)**;③ 仅剩 eval 期的 TDim 类坎(TopK 起)待修。
>
> **目标**:走路 A——**修几处 tract 算子 → eval 跑通 → 对齐 ORT → 内嵌落地 `--layout-model ppv2`(与 YOLO 共存)**。(原计划含"上游化 tract 修复";**已于 2026-06-15 决定:补丁长期 vendored 留 main,不发上游 PR**,见 [vendor/README.md](../../vendor/README.md)。)
>
> ---
> **✅ 实施进度(2026-06-14,devlog [2026-06-14-ppv2-tract-integration.md](../devlogs/2026-06-14-ppv2-tract-integration.md)):P0–P6 完成。**
> - **P1/P2** 两处 tract 补丁(GatherNd 推断 + TopK TDim,见 [vendor/PATCHES.md](../../vendor/PATCHES.md))→ optimize + eval **端到端跑通**;闸门 G 仅用 2 处(≤6)。
> - **P4** tract == ORT:5/5 页逐框一致(class 100% / score Δ<1.3e-6 / box Δ<5e-4px / order 同)。
> - **P5** 速度:2391ms = YOLO(1738ms)的 **1.38×**,过 1.5× 门。
> - **依赖分析**:vendored 补丁进 main 低风险且可证明对现有模型零影响(PP-OCR/UniRec/SLANet/TATR 不用这两算子;YOLO 的 TopK 走非 TDim 原路径)。见 [analysis/2026-06-14-vendored-tract-patch-on-main.md](../analysis/2026-06-14-vendored-tract-patch-on-main.md)。
> - **P6** 内嵌:`layout.rs` 双后端(**按 ONNX 输入数自动识别** YOLO/PPV2)、`RegionKind` 25→统一语义、原生 order 定序、标题 `tag` 流入输出、表区 seed;`formula.rs`/`transcribe.rs` 迁移到 `kind`(顺带支持 PPV2);YOLO 路径零回归(单测 25/25、e2e 16 区域不变)。**用法:`--layout --layout-model models/layout-ppv2/PP-DoclayoutV2_simp.onnx`**。
> - **P7(已收尾,2026-06-15)**:记分牌 A/B 已跑(General 端到端表 0.206→0.654 ≈3×,见 [testresults/2026-06-15](../testresults/2026-06-15-ppv2-vs-yolo-omnidocbench.md))、status.md 已更;**上游 PR 决定不发**(补丁长期 vendored,[vendor/README.md](../../vendor/README.md);草稿留 [vendor/UPSTREAM-PRS.md](../../vendor/UPSTREAM-PRS.md) 备用)。**本计划全部收官。**
>
> **边界(延续 G2/G3-R 身份)**:纯 Rust、确定性核心独立、模型可插拔;主流程不渲染像素,难页按需渲染;版面是 **opt-in** 路径,**快路径(born-digital 无模型)零触碰**。不追:GPU、自训模型、paddlex/onnxruntime 运行时依赖(onnxruntime 仅 spike 期做金标准对照,不进产物)。

---

## 0. 决策闸门(先说清"什么时候掉头")

路 A 赌的是"tract 的坎是一串*小而局部*的修复"。设一个**硬闸门**防止陷进 tract 内部泥潭:

- **闸门 G(P3 末)**:从 GatherNd 之后到 eval 端到端跑通,**累计需要的 tract 非平凡修复 ≤ 6 处**,且每处都是"局部算子/规则级"(非重写 tract 的 TDim 子系统)。
- 超闸门 → **掉头到路 C**(ONNX 切图 + Rust 重写动态头,见 analysis §5),或**搁置维持 DocLayout-YOLO**。届时已修补丁与认识不浪费(切图也要它们)。

## 1. 里程碑

### P0 · 可复现 spike 基建 —— *0.5d* 🎯 先做
把上轮一次性脚本固化为可重跑流程(上轮临时件已删,需重建)。

- [ ] **静态化流水线**:Python 一次性工序——onnx `update_inputs_outputs_dims`(batch=1)+ `infer_shapes` + `onnxsim`(实测 4700→1480 节点)→ 产出 `models/layout-ppv2/PP-DoclayoutV2_static.onnx`(gitignored)。脚本入 `scripts/spike/ppv2/`(eval-only,不进产物路径)。
- [ ] **金标准**:ORT 跑静态模型,dump 数页 `[N,8]` 框(class/score/box/order)为对照基线(`1901.03003 p0` 等,坐标在原图空间)。
- [ ] **tract 探针**:重建 `crates/docparse-ocr/examples/diag.rs`(parse+optimize 门,`DIAG_NOFIX`)与 `ppv2_run.rs`(eval + 逐框比 ORT)。标注"跑完即删/不提交"。
- **验收**:一条命令从 PDF→静态 onnx→ORT 金标准;diag/ppv2_run 可跑。

### P1 · tract fork 基建 + GatherNd 推断修复 —— *0.5d 已验证* 🎯
- [ ] **vendoring 策略**:`vendor/` 放需改的 tract 子 crate(已知 `tract-hir`;TDim 修复大概率涉 `tract-core`/`tract-data`),根 `Cargo.toml` 用 `[patch.crates-io]` 注入;只改必要文件,其余依赖仍走 crates.io。**记一份 `vendor/PATCHES.md`** 列每处改动(文件/行/理由/对应上游 issue)。
- [ ] **GatherNd 修复**(已验证):`tract-hir/src/ops/array/gather_nd.rs` 规则 B 把输出尾部维度约束从 `inputs[1]`(indices)改为 `inputs[0]`(data),循环上界用 data rank、偏移 `n+batch_dims`。
- **验收**:`diag` 跑静态模型 **optimize OK**(上轮已得 3377→2850 节点);记录节点数。

### P2 · TopK 的 TDim 坎 —— *1–2d* ⚠️ 第一个真硬骨头
现象:optimize 过、eval 报 `TopK Running legacy eval: TDim is not a number`——排序输入被标成 TDim 类型(dim 衍生 int64 漏进数据通路)。

- [ ] **先诊断定位**:确认是哪条链把 int64 变成 TDim(`Shape`/`Gather`/`Cast` 之后参与排序);判断更优解是 (a) 让该 Cast/分支产出普通 i64,(b) 让 TopK eval 容忍并具体化 TDim,(c) 上游常量折叠掉该 dim 计算。**改前先量**:把该张量的 datum_type 与上游算子打印出来。
- [ ] 选最小侵入的修法落到 `vendor/`,更新 `PATCHES.md`。
- **验收**:eval 越过所有 TopK 节点(推进到下一个错或跑完)。

### P3 · 逐个推进 eval 到端到端跑通 —— *1–3d*(受闸门 G 约束)
动态头还有 `GatherND×3`(NMS 去重段)、`GatherElements×4`、`ScatterND×2`、`Range×4` 未验(eval 之前在 TopK 先停)。

- [ ] 循环:跑 `ppv2_run` → 命中错 → 定位算子/规则 → 最小修复入 `vendor/` → 重跑;每修一处更新 `PATCHES.md` 并对 **修复计数** 记账(闸门 G)。
- [ ] 同类坎(多半是 TDim 传播或同款推断 bug)尽量**一处修复覆盖多算子**。
- **验收**:静态 PP-DocLayoutV2 在 tract 上 **eval 端到端无错跑完**,输出 `[N,8]`。**闸门 G 检查点**:累计非平凡修复数落档;超 6 处或触及 TDim 子系统重写 → 触发掉头评估。

### P4 · 正确性:tract == ORT —— *1d* 🎯 决定能不能用
- [ ] `ppv2_run` 对 P0 金标准**逐框比对**:score>0.5 的框数、class、box(容差,如 ≤2px)、order 序一致;跨 5+ 样例页(论文双栏 / 中文扫描 / 财报表 / 多图页)。
- [ ] 差异溯源:若某算子修复引入数值偏差(非纯形状),回到 P2/P3 修正(typed 真算逻辑须与 ONNX 语义一致——参考已知 `tract-core` GatherNd `compute_shape` 是对的)。
- **验收**:tract 与 ORT 输出在容差内一致;**这一关过 = 模型可用**。不过则回修或掉头。

### P5 · CPU 速度门 —— *0.5d*
- [ ] 本机(Apple Silicon CPU)测 PP-DocLayoutV2 单页延迟(预处理 + tract run + 解析 `[N,8]`),对比 DocLayout-YOLO 现役同页基线。**改前先量** YOLO 基线。
- [ ] 门槛:版面是难页 opt-in,容忍略慢换质量;建议 **≤ YOLO 的 1.5×**;超门评估 int8 量化或记为已知代价。
- **验收**:单页延迟落档,过门或给量化/接受结论。

### P6 · 内嵌落地 `--layout-model ppv2`(与 YOLO 共存)—— *2–3d*
形态:**新增**模型选项,**不替换**默认;后处理在图内(无需 Rust 重写 DETR 解码)。

- [ ] **预处理(纯 Rust)**:渲染页(`docparse-raster`)→ resize **800×800 bilinear** → RGB `/255`(无 mean/std)→ NCHW;喂 `image`+`im_shape[[800,800]]`+`scale_factor[[800/oh,800/ow]]`(输入顺序 `im_shape,image,scale_factor`)。
- [ ] **后处理(纯 Rust)**:解析 `[N,8]`=`[class,score,x1,y1,x2,y2,order,_]`,过 `score>0.5`,按 `order` 升序 → 区域阅读顺序;坐标已在原图像素空间,按 raster scale + y 翻转折算回 PDF 用户空间(复用 [layout.rs](../../crates/docparse-ocr/src/layout.rs) `detect` 的映射约定)。
- [ ] **接线**:在 [ocr/layout.rs](../../crates/docparse-ocr/src/layout.rs) 抽象出"版面后端"——YOLO(letterbox 1024 + 项目 XY-cut 定序)与 PPV2(resize 800 + 原生 order 定序)两实现;25 类标签映射进 `Region.class` 语义(table→seed `--table-model`、formula 类→公式路由、header/footer→可剔除、doc_title/paragraph_title→标题分级);PPV2 路径**直用模型 order 跳过 XY-cut**(保留 XY-cut 作回退)。
- [ ] **CLI**:[cli/main.rs](../../crates/docparse-cli/src/main.rs) 加 `--layout-model {yolo|ppv2}`(默认 yolo);模型外置 `models/layout-ppv2/`(gitignored),`find_file` 定位;缺模型给清晰报错。
- [ ] **依赖**:tract patch 经 `[patch.crates-io]` 生效;**onnxruntime 不进任何产物路径**。
- **验收**:`docparse <pdf> --layout --layout-model ppv2` 跑通,输出阅读顺序/类别明显优于 yolo(对照 S3-lite 同页);yolo 路径零回归。

### P7 · 收尾 —— *已完成 2026-06-15*
- [x] **vendoring 去留(决定)**:**长期 vendored 留 main,不发上游 PR**(理由/维护/回退见 [vendor/README.md](../../vendor/README.md);PR 草稿留 [vendor/UPSTREAM-PRS.md](../../vendor/UPSTREAM-PRS.md) 备用)。
- [x] **回归**(§1 lorem/bialetti/1901.03003)+ PP-OCR/YOLO/UniRec + 149 单测——确认 tract patch 对现栈零副作用;算子级证明零影响。
- [x] **记分牌 A/B**(YOLO vs PPV2,同页同打分器):General 端到端表 0.206→0.654 ≈3×,见 [testresults/2026-06-15](../testresults/2026-06-15-ppv2-vs-yolo-omnidocbench.md)。
- [x] devlog + testresults 落档;[docs/status.md](../status.md) Phase 7 已更;README/CLAUDE/roadmap/agent-integration/CLI help 全量同步。
- **验收**:clippy 零 warning、`cargo fmt`、全单测绿、三件套回归无异、记分牌无回归。

## 2. 工作量与关键路径

| 里程碑 | 估时 | 闸门 |
|---|---|---|
| P0 基建 | 0.5d | — |
| P1 fork + GatherNd | 0.5d(已验) | optimize OK |
| **P2 TopK/TDim** | 1–2d | 第一硬骨头 |
| **P3 eval 跑通** | 1–3d | **闸门 G:≤6 处修复** |
| **P4 正确性 == ORT** | 1d | **可用性闸门** |
| P5 速度 | 0.5d | ≤1.5× YOLO |
| P6 内嵌落地 | 2–3d | yolo 零回归 |
| P7 收尾 + 回归 + A/B | 0.5d | 记分牌无回归 |

**合计 ≈ 7–11d**。关键路径 = P2→P3→P4(tract 修复链 + 正确性);P0/P1 已基本就绪。**P4 不过或闸门 G 触发 → 掉头路 C 或搁置。**

## 3. 风险与缓解

| 风险 | 缓解 |
|---|---|
| TDim 坎不止 TopK,蔓延成 tract 子系统重写 | **闸门 G**(≤6 处)硬限;超则掉头路 C |
| tract 修复引入数值偏差(非纯形状) | P4 逐框对齐 ORT;参考 `compute_shape` 正确实现校准 |
| 维护 tract fork 的长期成本 | 接受为已知有界成本(bump tract 时重打补丁,流程见 [vendor/README.md](../../vendor/README.md) §4);PR 草稿留存,需要时再上游 |
| 静态化把 batch 钉死=1(单页) | 版面本就逐页跑,无需 batch>1;若要批处理另议 |
| 速度比 YOLO 慢(RT-DETR 更重) | P5 门 + int8 量化备选;opt-in 路径容忍 |
| onnxsim/onnx 版本漂移致静态化不稳 | P0 脚本固定版本;产物是静态 onnx(一次性) |

## 4. 不做 / 显式排除

- 不替换默认版面模型(YOLO 保留,ppv2 共存,按需/按页型路由)。
- 不引入 onnxruntime/paddlex 运行时依赖(仅 spike 期金标准用)。
- 不动快路径(born-digital 无模型)与确定性核心。
- 不自训/微调模型;用官方 Apache-2.0 权重。

## 5. 参考
- 根因与三路对比:[analysis/2026-06-14-why-tract-cant-run-pp-doclayoutv2.md](../analysis/2026-06-14-why-tract-cant-run-pp-doclayoutv2.md)
- 实测(算子门/质量/对齐):[testresults/2026-06-14-ppv2-tract-gate-and-unirec-alignment.md](../testresults/2026-06-14-ppv2-tract-gate-and-unirec-alignment.md)
- 现役版面后端:[crates/docparse-ocr/src/layout.rs](../../crates/docparse-ocr/src/layout.rs)、[core/layout.rs](../../crates/docparse-core/src/layout.rs)
- tract 源码(本机 cargo registry):`tract-hir-0.23.1`/`tract-core-0.23.1`(GatherNd/TopK 算子)
- 模型:HF `topdu/PP_DoclayoutV2_onnx`(Apache-2.0,gitignored 于 `models/layout-ppv2/`)
- 经验铁律:[docs/status.md](../status.md)(看图核对 > 代理指标;改前先量;依赖版本本身可是性能特性)
</content>
