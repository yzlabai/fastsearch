# 测试结果 · 与 OpenDataLoader (ODL) 同台（born-digital）

> 日期：2026-06-10（TEDS 按内容配对纠偏） · 来源：`scripts/eval/compare_odl.py` · 参照：ODL JSON 输出（/tmp/odl_out）

> ODL 是**确定性**结构抽取器（Java/veraPDF wcag-algs），与本项目同类——其水平**确定性可达**。NID=阅读顺序(词级)，TEDS=表格结构代理，MHS=标题文本集 F1。

| 文档 | NID | TEDS | MHS | 备注 |
|---|---|---|---|---|
| 2203.01017v2 | 0.936 | 0.056 | 0.652 | 表 我方4/ODL13 |
| 2206.01062 | 0.748 | 0.056 | 0.293 | 表 我方6/ODL10 |
| 2305.03393v1-pg9 | 0.990 | 0.060 | 1.000 | 表 我方1/ODL1 |
| 2305.03393v1 | 0.921 | 0.044 | 0.488 | 表 我方2/ODL12 |
| amt_handbook_sample | 0.660 | 0.000 | 0.000 | 表 我方0/ODL4 |
| code_and_formula | 0.999 | 1.000 | 1.000 |  |
| multi_page | 0.984 | 1.000 | 1.000 |  |
| normal_4pages | 0.580 | 0.000 | 0.545 | 表 我方0/ODL1 |
| picture_classification | 0.998 | 1.000 | 1.000 |  |
| redp5110_sampled | 0.973 | 0.432 | 0.410 | 表 我方5/ODL4 |
| right_to_left_01 | 0.971 | 1.000 | 1.000 | RTL |
| right_to_left_02 | 0.000 | 0.000 | 0.000 | RTL |
| right_to_left_03 | 0.000 | 0.002 | 0.000 | RTL |
| skipped_1page | 0.222 | 1.000 | 0.667 |  |
| skipped_2pages | 0.123 | 1.000 | 0.312 |  |

## 汇总（去 RTL）

- LTR 12 份：NID **0.761**、MHS **0.614**
- 含表 7 份：TEDS **0.093**
- 评测 15 份（有 ODL 输出者）。
