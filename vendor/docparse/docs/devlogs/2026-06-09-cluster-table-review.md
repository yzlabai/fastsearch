# Devlog · 聚类表格识别 P1a→P1b 阶段复盘

> 日期：2026-06-09 · 状态：P1a/P1b ✅ 已并 main（零回归）；P1c 待做
> 串起：[分析](../refer/opendataloader-verapdf-analysis.md) → [设计](../plans/cluster-table-recognizer-rust.md) → [P1a](2026-06-09-p1a-cluster-table-scaffold.md) → [P1b](2026-06-09-p1b-cluster-attraction.md)

## 一句话

参 veraPDF `ClusterTableConsumer` 独立重写了聚类表格识别器（`core::table_cluster`），**零回归**下找到 ruled/borderless 漏掉的真实宽数值表（2203：`Tags|Bbox`、`Model|mAP`、`Tabula` 等，表 3→4，MHS +0.014）。距 ODL 全覆盖（13）仍远，剩余 gap 留给 P1c 的结构校验。

## 方法（每步可证伪、数据驱动）

诊断（dump 真实 chunk）→ 参算法重写 → `compare_odl.py`/`compare_docling.py` 量化 → 提交。本轮关键是**用 harness 反复证伪**：naive 吸引级联曾把 NID/MHS 打崩（2305-pg9 0.99→0.55），靠逐文档 dump 定位误判类、逐一加门，直到零回归。

## 三个决定性认知（实现期纠正了设计）

1. **喂入序**：不能用 XY-cut（会把表竖切成列，header 凑不齐）；改**行扫描序**。
2. **按列喂入**（计划未预见）：双栏论文页宽扫描会把左栏表行与右栏散文交错 → header 永远不成。`split_columns`（sweep-line 找中央带最空 x 作栏间沟，容忍跨沟标题）逐栏喂，才是真正解锁——加这一条后 2203 立刻找到真实表。
3. **精度靠内容门是权宜**：吸引级联把每 token 都塞进列，方程/图注/CJK散文/页眉全成表。当前用数值/≥3列/逐列均长兜精度（保守、零回归），但**这不是 veraPDF 的精度来源**——它靠结构校验。退掉启发式、放开非数值/2列表是 P1c。

## 量化（compare_odl，15 份）

| | 基线 | P1b | 
|---|---|---|
| LTR NID / MHS | 0.651 / 0.583 | **0.651 / 0.584** |
| 含表 TEDS | 0.052 | **0.052** |
| 2203 表 / MHS | 3 / 0.694 | **4 / 0.708** |

聚合持平（零回归），真实信号 = 找到对的表、文本不损。打掉的误判类、逐项验证见 P1b devlog 表。

## 进度标注

- ✅ next-iteration.md N4：聚类表格 P1a/P1b 已勾。
- ✅ cluster-table 计划 §8：P1a-*/P1b-* 标 ✅，新增 P1c-1/2/3 + 偏离说明。
- ⬜ **P1c**（下一步）：gap 图 + `mergeClustersByMinGaps` 列碎片合并 + 真 `Table.validate`，用结构校验替代内容启发式 → 在保精度下逼近 ODL 覆盖。
