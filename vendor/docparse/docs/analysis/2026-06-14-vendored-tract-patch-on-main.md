# 分析:vendored tract 补丁直接进主分支有什么问题(2026-06-14)

> 回答"把 `vendor/tract-hir` + `vendor/tract-core`(经 `[patch.crates-io]`)带进主构建,直接在 main 上有什么问题"。
> 补丁内容见 [vendor/PATCHES.md](../../vendor/PATCHES.md);根因见 [why-tract-cant-run-pp-doclayoutv2.md](why-tract-cant-run-pp-doclayoutv2.md)。
> **裁决:低风险、自洽、可进 main——前提是 (a) 补丁最小且记档(已做)、(b) 立即提上游 PR 并跟踪、(c) 上游合并后回退为版本依赖。**

## 实测证据

| 检查 | 结果 |
|---|---|
| 补丁是否一致覆盖整棵 tract 树 | ✅ `cargo tree`:tract-hir/tract-core 的**所有引用**(含 tract-nnef/extra/pulse/onnx 传递依赖)都解析到 `vendor/`,**无版本分裂** |
| 许可 | ✅ tract = Apache-2.0/MIT,vendored 副本保留 `LICENSE*`,与本项目 Apache-2.0 兼容 |
| 单测 | ✅ workspace 全绿(134 passed / 0 failed),clippy 零 warning |
| 文本三件套回归 | ✅ lorem / bialetti / 1901.03003 文本正确(文本路径不经 tract) |
| YOLO 版面(tract) | ✅ `--layout` 跑通 15 页,无 panic |
| **算子级回归证明** | ✅ 见下 |

### 算子级证明:对现有模型"可证明零影响"

补丁只改两个算子的代码路径(`GatherNd` 推断规则、`TopK` 对 TDim 输入的 eval)。逐模型核对是否用到:

| 模型 | GatherND | TopK | 受影响? |
|---|---|---|---|
| PP-OCR det/rec v4/v5 | — | — | **不可能**(都不用) |
| UniRec encoder/decoder | — | — | **不可能** |
| SLANet / TATR | — | — | **不可能** |
| DocLayout-YOLO | — | ✅×2 | **否**:TopK 输入是 **float**(非 TDim)→ 我的 `in_dt==TDim` 分支不触发 → 走原路径,输出不变(且实测 15 页正常) |
| **PP-DocLayoutV2** | ✅×5 | ✅×4 | 是(目标模型) |

- **GatherNd 修复**:只在有 `GatherND` 的图生效——现有模型里只有 PPV2 用;且旧规则本是 bug(静默产出错形状),修复只会更正确。
- **TopK 修复**:只新增一条 `TDim` 输入分支;数值输入(含 YOLO)走**完全不变**的老路径。

→ **现有所有模型输出可证明不变**,非"测了没崩"而是"代码路径未被触及"。

## 进 main 的收益(为何可行)

1. **自洽、可复现**:path 式 `[patch.crates-io]` 指向仓库内 `vendor/`,**无外部 git/网络依赖**,clean build 完全 hermetic(优于 git-fork 依赖)。
2. **一致版本**:整棵 tract 树统一走 vendored 0.23.1,无 onnx-crates.io 与 vendored 混版问题(已验)。
3. **回归可证明为零**(见上)。
4. **补丁最小且改善正确性**:两处共约 30 行,均可上游化。

## 进 main 的代价(需接受/缓解)

| 代价 | 说明 | 缓解 |
|---|---|---|
| **维护义务** | 日后 bump tract(求性能/其它修复)需重新 vendor 两个完整 crate + 重打补丁 | `PATCHES.md` 记每处 diff;补丁极小,re-apply 成本低 |
| 仓库体积 | vendored 源码入库(tract-hir ~0.5MB + tract-core ~数 MB) | 可接受;只是源码 |
| clean build 时间 | 从源码编 vendored tract-core/hir | tract 本就从源码编,增量可忽略 |
| 上游分叉 | 合并前与 upstream 分叉 | P7 立即提 PR;合并发版后回退版本依赖并删 vendor |

## 三条路对比与建议

| 路 | hermetic | 立即可推进 P6 | 维护成本 | 备注 |
|---|---|---|---|---|
| **A. vendor 进 main + 并行上游(推荐)** | ✅ 最高 | ✅ | 中(短期 fork) | 现状;回归已证零影响 |
| B. 先等上游合并再依赖 | ✅ | ❌ 阻塞数周 | 低 | 最干净但拖慢落地 |
| C. git fork 依赖 | ⚠️ 引入 git 依赖 | ✅ | 中 | 比 vendor 略不 hermetic |

**建议路 A**:现在用 vendored 补丁推进 P6,**同时**给 tract 提两个 PR(GatherNd 是明确 bug、TopK 该支持 TDim 输入),`PATCHES.md` 挂 PR 链接;上游合并发版后改回 `tract-* = "0.x"` 版本依赖并删 `vendor/`。风险已被算子级证明 + 全回归压到极低。

### 落地前置(进 main 的 checklist)
- [ ] `vendor/` 与 `[patch.crates-io]` 入库;`vendor/PATCHES.md` 完整(已就绪)。
- [ ] 确认 `vendor/tract-*/LICENSE*` 随源码保留(已在)。
- [ ] 记分牌 + 三件套回归归档(P7)。
- [ ] 提 2 个上游 PR,挂链接;设回退提醒(上游发版后)。
</content>
