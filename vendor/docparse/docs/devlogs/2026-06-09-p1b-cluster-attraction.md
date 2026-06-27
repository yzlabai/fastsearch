# Devlog · P1b 聚类表格：吸引级联 + 按列喂入 + 精度门（参 veraPDF 独立重写）

> 日期：2026-06-09 · 状态：✅ 净正、**零回归**；找到 ruled/borderless 漏掉的真实宽数值表
> 承接 [p1a-cluster-table-scaffold](2026-06-09-p1a-cluster-table-scaffold.md) · 计划 [plans/cluster-table-recognizer-rust.md](../plans/cluster-table-recognizer-rust.md)

## 做了什么

在 P1a 脚手架上补足让识别器真正产出表的三件事（[crates/docparse-core/src/table_cluster.rs](../../crates/docparse-core/src/table_cluster.rs)）：

1. **吸引级联**（替换 P1a 的严格单 header 包含 bail）：`attract_to_header` 按 veraPDF `mergeWeakClusters` 的 factor 级联——中心重叠(0.01) < 重叠(0.1) < 仅按中心距(1.0)——把不规则 cell（比表头宽、右对齐）也归到最近列，不再整表丢弃。新增谓词 `are_overlapping`/`are_center_overlapping`。
2. **按列喂入**（`split_columns`）：关键修复。页宽行扫描会把双栏论文里左栏表格的行与右栏散文交错，header 行永远凑不齐。用 sweep-line 求"中央带内被最少 chunk 跨越的 x"作栏间沟（容忍跨沟的整宽标题/节标题——只是少数 chunk），按栏切分后逐栏喂状态机。栏切只在 chunk 数 ≥60（页级）时启用，避免把孤立表格按自身列缝切碎。
3. **精度门**（`passes_content_gates`）：吸引级联会把每个 token 都塞进某列，没有门控就把方程块/图注/CJK 散文/页眉当表。门：≥2 body 行、≥3 列、每列有内容且**逐列**均长 ≤25 字（全局均长会被"节号5.1+整段"骗过）、密度 ≥⅓、**数值证据 ≥25%**。

## 量化（`compare_odl.py`，15 份 born-digital）

| 指标 | 基线(P1a) | P1b | 评 |
|---|---|---|---|
| LTR NID | 0.651 | **0.651** | 持平（零回归）|
| LTR MHS | 0.583 | **0.584** | 持平 |
| 含表 TEDS | 0.052 | **0.052** | 持平 |
| 2203 表格 | 3（ruled/borderless）| **4** | +1 **真实**结果表 |
| 2203 MHS | 0.694 | **0.708** | +0.014 |

**2203 新增的聚类表是真的**：`Tags\|Bbox\|Size\|Format`、`Model\|Dataset\|mAP\|mAP(PP)`、`Tabula\|78.0\|57.8\|67.9`、TEDS 结果表——正是 ODL/Docling 检出、而我方 ruled/borderless 漏掉的宽数值表。Docling 同台：2203 我方 3→4（趋近 Docling 6）。

**零回归确认**：所有纯文档/clean 文档不变（`code_and_formula` 0.999、`picture` 0.998、`skipped_*`/`normal_4pages` TEDS 1.0）；曾经的最佳表格文档 `2305-pg9` 全程保持 1/1、NID 0.990、MHS 1.0。

## 迭代中打掉的误判类（每类都验证过）

| 误判 | 触发 | 门 |
|---|---|---|
| 双栏论文左栏表被栏交错冲散 | 页宽扫描 | 按列喂入 `split_columns` |
| 孤立表被自身列缝切碎 | sweep 无旁文支撑 | chunk<60 不切栏 |
| 2 行类别标签块 | 几何像表 | ≥2 body 行 |
| 节标题"5.1 + 整段"、图注"Table 1. …" | 短首 token + 长散文列 | **逐列**均长 ≤25 |
| CJK 编号散文、方程块、页眉 | 短 cell、伪列 | 数值证据 ≥25% + ≥3 列 |

## 诚实的边界

- 这是**保守版 P1b**：数值证据 + ≥3 列两道门为保精度而牺牲召回——**非数值表、2 列表**仍走不到（被 ruled/borderless 兜底）。所以离 ODL 的 13/12 还远。
- 根因：我方略过了 veraPDF 的**结构校验**（gap 图 + `mergeClustersByMinGaps` 列碎片合并 + 真 `Table.validate`），只能用内容启发式兜精度。**下一步 P1c**：实现 gap 图与列碎片合并，用结构校验替代数值/列数启发式，才能在保精度下放开非数值/2 列表，向 ODL 覆盖逼近。
- TEDS 是结构代理 + 对 ODL 一致度（非人工真值），±0.005 属噪声；本次真实信号是"找到对的表 + NID/MHS 不掉"。
