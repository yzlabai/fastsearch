# 测试结果 · 与 Docling 同台（born-digital）

> 日期：2026-06-09 · 来源：`scripts/eval/compare_docling.py` · 数据：Docling 自带 born-digital 测试集（tmp/refer/docling/tests/data，其 groundtruth = Docling 自身输出）

> **测的是与 Docling 的一致度**，非人工真值准确率：高一致 = 我方 born-digital 结构接近 Docling；差异不必然 = 更差。NID=阅读顺序(词级 difflib ratio)，TEDS=表格结构代理，MHS=标题文本集 F1。

## 逐文档

| 文档 | NID | TEDS | MHS | 备注 |
|---|---|---|---|---|
| 2203.01017v2 | 0.467 | 0.043 | 0.160 | 表格 我方3/Docling6 |
| 2206.01062 | 0.489 | 0.092 | 0.000 | 表格 我方3/Docling5 |
| 2305.03393v1-pg9 | 0.768 | 0.000 | 0.000 | 表格 我方0/Docling1 |
| 2305.03393v1 | 0.788 | 0.000 | 0.000 | 表格 我方0/Docling2 |
| amt_handbook_sample | 0.526 | 1.000 | 0.000 |  |
| code_and_formula | 0.983 | 1.000 | 1.000 |  |
| multi_page | 0.593 | 1.000 | 0.000 |  |
| normal_4pages | 0.481 | 0.000 | 0.000 | 表格 我方0/Docling1 |
| picture_classification | 0.504 | 1.000 | 1.000 |  |
| redp5110_sampled | 0.651 | 0.034 | 0.491 | 表格 我方4/Docling6 |
| right_to_left_01 | 0.044 | 1.000 | 0.000 | RTL（我方 LTR，超范围） |
| right_to_left_02 | 0.000 | 0.000 | 1.000 | RTL（我方 LTR，超范围） |
| right_to_left_03 | 0.000 | 0.000 | 0.000 | RTL（我方 LTR，超范围） |

## 汇总（诚实分层）

| 切片 | 文档数 | NID | TEDS | MHS |
|---|---|---|---|---|
| **born-digital LTR**（去 RTL）| 10 | **0.625** | — | **0.265** |
| 含表格子集（TEDS 仅在有表文档有意义）| 6 | — | **0.028** | — |
| RTL（超范围，仅记录）| 3 | 0.015 | — | — |

- **表格检出召回**（Docling 有表的 LTR 文档中我方也检出 ≥1 表）：3/6。我方 M4 只做**有框**表格，学术论文多为**无框**（booktabs）→ 多数 TEDS=0，已知限制（→ N4 无框表格）。
- 解析：13/13 成功，0 panic。
- **诊断结论**：① 文本/阅读顺序 LTR 与 Docling 中等一致（受分块粒度与缺无框表影响）；② 表格主要差距在**无框表格**（N4 最高优先，本数据佐证）；③ 标题检测（字号众数）弱于 Docling 模型标注（N4 标题分级）；④ **RTL 未支持**（LTR XY-cut），非 born-digital-LTR 战场，记录在案。
- 参照：Docling 在 ODL benchmark 综合 0.882（不同数据/口径，**不可并列**，仅量级参照）。
