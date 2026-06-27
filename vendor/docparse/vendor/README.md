# `vendor/` — 内嵌（vendored）tract 补丁说明

> **一句话**:这里是 tract 两个子 crate(`tract-hir`、`tract-core`)的本地副本,各带一处最小修复,让 **PP-DocLayoutV2(RT-DETR)版面模型**能在 `tract` 上跑。根 `Cargo.toml` 的 `[patch.crates-io]` 把构建指向这里。
>
> **决定(2026-06-15):这两处补丁以 vendored 形式长期留在 `main`,暂不发上游 PR。** 理由、维护方式、未来回退条件见下。

---

## 1. 这是什么

| 文件/目录 | 作用 |
|---|---|
| `vendor/tract-hir/` | crates.io `tract-hir 0.23.1` 副本 + 1 处补丁(GatherNd 形状推断) |
| `vendor/tract-core/` | crates.io `tract-core 0.23.1` 副本 + 1 处补丁(TopK 接受 TDim 输入) |
| `vendor/PATCHES.md` | **每处补丁的精确 diff、原因、最小复现** —— 改/查必读 |
| `vendor/UPSTREAM-PRS.md` | 两个上游 PR 的现成草稿(若将来决定上游) |
| 根 `Cargo.toml` `[patch.crates-io]` | 把 `tract-hir`/`tract-core` 解析到 `vendor/` |

补丁内容都在调用点用 `docparse PATCH` 注释标出。完整根因分析见
[docs/analysis/2026-06-14-why-tract-cant-run-pp-doclayoutv2.md](../docs/analysis/2026-06-14-why-tract-cant-run-pp-doclayoutv2.md);
决策论证见 [docs/analysis/2026-06-14-vendored-tract-patch-on-main.md](../docs/analysis/2026-06-14-vendored-tract-patch-on-main.md)。

## 2. 为什么需要(背景)

PP-DocLayoutV2 质量明显优于现役 DocLayout-YOLO(25 类语义 + 原生阅读顺序;**杂版面端到端表识别 ≈3×**,见 [status.md 记分牌](../docs/status.md))。它此前"跑不了 tract"被深挖证明只是 tract 0.23.1 的两个具体 bug:

1. **`tract-hir` GatherNd 推断**:输出尾维错误地约束到 indices 张量,应为 data(一行 bug;typed 层 `compute_shape` 本是对的)。
2. **`tract-core` TopK 收 TDim**:`tract-onnx` 把 `Cast(to=INT64)` 映射成 TDim,RT-DETR 的 int64 掩码喂进 TopK 即 `"TDim is not a number"`;补丁在 eval 加 TDim→i64 旁路。

两处修完即 **eval 端到端跑通、tract 输出与 ONNX Runtime 5/5 页逐框一致**。

## 3. 决定:长期 vendored 留 main,不发上游 PR

**取舍**:上游 PR 能让我们将来删掉 `vendor/`、改回普通版本依赖(甩掉维护负担),但需对外提交并等上游合并发版(数周+)。综合判断**保留 vendored、不发 PR**:

- ✅ **自洽可复现**:path 式补丁,无外部 git/网络依赖,clean build 完全 hermetic;
- ✅ **低风险、可证明零影响**:补丁只触及 GatherNd/TopK 两算子的代码路径——现有模型 PP-OCR/UniRec/SLANet/TATR **不用这两算子**,DocLayout-YOLO 的 TopK 走非-TDim 原路径(详见 [analysis/vendored-tract-patch-on-main](../docs/analysis/2026-06-14-vendored-tract-patch-on-main.md));149 单测 + 文本三件套 + 双后端回归通过;
- ✅ **代价有界**:唯一负担是 bump tract 时需重做补丁(见 §4),已记档;
- ✅ **许可干净**:tract = Apache-2.0/MIT,vendored 副本保留 `LICENSE*`,与本项目 Apache-2.0 兼容。

> 草稿仍留在 `UPSTREAM-PRS.md`——若将来改主意(或上游恰好修了),可随时启用。

## 4. 维护:bump tract 时怎么办（重要）

**升级 tract 版本会让 `[patch]` 失效或编译失败,必须重做补丁:**

1. 把目标版本的 `tract-hir` / `tract-core` 源码重新拷到 `vendor/`(覆盖)。
2. 按 `vendor/PATCHES.md` 把两处补丁重新打上(找上游对应代码,语义照搬;若上游已自行修复某处,则跳过那处)。
3. 根 `Cargo.toml` 的版本号同步(`[workspace.dependencies]` 的 `tract-onnx` / `tract-linalg`)。
4. **回归必跑**(确认补丁 + 新版无副作用):
   - `cargo test --workspace`(应全绿)、`cargo clippy --all-targets`(零 warning);
   - 文本三件套(CLAUDE.md §1);
   - 版面 A/B:`scripts/eval/omnidocbench/compare_layout_backends.sh`(需数据集);
   - PPV2 正确性:`scripts/spike/ppv2/{prepare,golden,compare}.py` + `examples/ppv2_run.rs`(tract == ONNX Runtime)。
5. 更新 `PATCHES.md`(若行号/写法变)。

CLAUDE.md §4 已挂提醒"bump tract 前先读 `vendor/PATCHES.md`"。

## 5. 何时可以删掉 `vendor/`

满足任一即可回退为普通依赖(删 `vendor/`、去 `[patch.crates-io]`、`tract-* = "x.y"`):

- 上游 tract 某版本已修这两处(GatherNd 推断 / TopK 收 TDim)——升级到该版,跑 §4 回归确认即可;
- 或不再需要 PP-DocLayoutV2 后端(回到仅 DocLayout-YOLO)。

回退后跑一遍 §4 回归;PPV2 后端的代码(`layout.rs` 双后端)无需改动——它不依赖补丁的"存在",只依赖"tract 能正确跑 RT-DETR"。
</content>
