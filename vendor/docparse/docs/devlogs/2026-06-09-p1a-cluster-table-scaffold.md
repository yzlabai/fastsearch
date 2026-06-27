# Devlog · P1a 聚类表格识别（脚手架 + 状态机，参 veraPDF 独立重写）

> 日期：2026-06-09 · 状态：✅ 机制就绪、零回归、零误判；**真实语料净增 0 表**（符合 P1a 设计边界）
> 计划：[plans/cluster-table-recognizer-rust.md](../plans/cluster-table-recognizer-rust.md) · 分析：[refer/opendataloader-verapdf-analysis.md](../refer/opendataloader-verapdf-analysis.md)

## 做了什么

新增 `core::table_cluster`（[crates/docparse-core/src/table_cluster.rs](../../crates/docparse-core/src/table_cluster.rs)），按计划附录逐行重写 veraPDF `ClusterTableConsumer` 路径的 **P1a 子集**：

- **几何谓词 + 概率原语**：`is_containing`、`uniform_prob`（平顶梯形）、`line_merge_prob`（字距×基线/字号相似）。
- **`RecognitionArea` 流式状态机**：`belongs_to_headers_area` / `expand_headers` / `expand_header`（含自适应行距学习）/ `check_headers`（≥2 列一致性，阈 0.75）/ `add_cluster`（gap/溢出关闭区域）。
- **`recognize`**：`setup_row_numbers`（基线分桶）+ 列号（header 左→右）+ 单 header 包含归列 + 建网格（header 行 + body 行）+ `validate`（行分离分，阈 0.75）+ 每 body 行 ≥2 实格。
- **驱动**：`scan_order` 喂入 + 区域关闭即识别 + 断点 token 回喂；接入 interpreter（bordered→ruled→**cluster**→borderless）。
- 5 个单测（含合成净表检出、散文不误判、排除区跳过）。clippy 零 warning，全测 36+ 绿。

## 关键修正：喂入顺序不能用 XY-cut

初版设计写"用 `reading_order()`(XY-cut)喂"。**错**：XY-cut 遇表格强列间隙会先竖切成列，把整列连续喂入（Method→alpha→beta…），header 行永远凑不齐 → 检不出。合成单测复现后改用 `scan_order()`（top→bottom 分带、带内 left→right，≈绘制序）。已回写计划 §11.2。

## 诚实的量化：P1a 在真实语料净增 0 表

`compare_odl.py`（15 份 born-digital）：LTR NID **0.650**（基线 0.651）、MHS **0.583**（不变）、含表 TEDS **0.052**（不变）；表格计数逐文档与基线**完全一致**（2203 我方3/ODL13、2305 全 2/12、amt 0/4…）。纯文档（`code_and_formula` 0.999、`picture` 0.998）不变 → **零误判**。

**为什么净增 0**：P1a 的"单 header 包含"门要求每个 body cell 被恰一个 header 列 x-包含；真实学术表格**单元格不规则**（比表头宽、右对齐、空格），几乎都触发 `return None` bail。这正是计划写明的 P1a 边界——**真正的召回提升要靠 P1b 的两个吸引阶段**（`merge_weak_clusters` 级联 + `merge_clusters_by_min_gaps`），把无 header 的弱 cluster 归列、放宽包含 bail。

## 结论与下一步

- **价值**：整套状态机 + recognizer + validate 脚手架就位、测过、接入、零回归零误判。P1b 只需"填两个桩 + 放宽 bail"。
- **下一步 P1b**：实现 gap 图（`min_left/right_gap`）、`is_weak_cluster` 链式判定、`merge_weak_clusters` 吸引级联（0.0001/0.001/0.01/0.1/1.0 + `−EPSILON` 怪癖）、`merge_clusters_by_min_gaps` 不动点合并；放宽 recognize 的包含 bail。目标：2203/2305 表格召回向 ODL 13/12 逼近。
