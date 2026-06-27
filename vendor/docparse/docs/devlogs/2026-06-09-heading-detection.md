# Devlog · 标题检测增强（编号/全大写）— 提 MHS

> 日期：2026-06-09 · 类型：质量改进（承 N1 诊断）· 参考 veraPDF · 状态：✅ 完成
> 结果：MHS 0.265→**0.412**；NID 不变（0.626）；零正文回归

---

## 1. 诊断

N1 同台 MHS 最弱（多数文档 0.000）。对 2206 比对：我方只检出**标题/作者**（字号 >1.25×body），漏掉 Docling 标注的 section header——`ABSTRACT`/`1 INTRODUCTION`/`2 RELATED WORK`/`REFERENCES` 等。这些在双栏论文里是**字重粗、全大写或带编号，但字号≈正文**，故字号规则漏判。

## 2. 修复（参考 veraPDF + 学术排版惯例）

`layout::is_heading_text`：单行短块（2–55 字符）满足其一即判标题——
- **编号节**：首词为节号（`1`/`5.1`/`A.1`，纯数字+点且含数字）后接大写词；
- **全大写**：字母 ≥2 且全大写（`ABSTRACT`/`REFERENCES`）。

并入 `make_block`：`heading = 单行 && (字号>1.25×body || is_heading_text)`。（veraPDF `NodeUtils.HEADING_SIZE_FACTOR=1.5` 是字号路径的同类思路；我方再加文本形状路径补字号漏判。）

**零 NID 风险**：标题块文本仍进 reading_order，故只影响 MHS 与 markdown `##`，不动 NID。

## 3. 效果（compare_docling.py）

| 文档 | MHS 前 | MHS 后 |
|---|---|---|
| 2203.01017v2 | 0.160 | **0.627** |
| 2206.01062 | 0.000 | **0.387** |
| 2305.03393v1 | 0.000 | **0.500** |
| 2305...pg9 | 0.000 | 0.333 |
| redp5110_sampled | 0.491 | 0.277 ↓ |
| **平均（10 LTR）** | **0.265** | **0.412** |

- 检出真实节标题（ABSTRACT/3 THE DOCLAYNET DATASET/4 ANNOTATION CAMPAIGN/5 EXPERIMENTS/6 CONCLUSION/REFERENCES）。
- 少量假阳性（`PLN DB DLN`/`68 Text` 等表内/图注全大写片段）；redp5110 因此略降——净收益显著为正。
- NID 0.626 不变；零正文回归；确定性 15/15；clippy 零 warning；单测 **45**（+1 标题检测）。

## 4. 已知限制

- **title-case 子标题**（`Baselines for Object Detection`、`Learning Curve`）仍漏——非编号非全大写，需**字重**信号；当前 IR `TextChunk` 不带 weight。后续可从字体名含 `Bold` 推断并入 IR。
- 假阳性：表内/图注的全大写短行被误判标题（amt/multi_page/normal_4pages 仍 0，含此类噪声）。

## 5. 本轮三连（参考 veraPDF/ODL + harness 数据驱动）小结

| 改动 | 指标 |
|---|---|
| 无框表格检测（N4）| 表格召回 1/6→3/6 |
| 词距阈值 0.25→0.15em（参 veraPDF）| NID 0.601→0.626 |
| 标题检测 编号/全大写（参 veraPDF）| MHS 0.265→0.412 |

下一步候选：字重入 IR（补 title-case 标题、减表内假阳性）；列推断提 TEDS；真实 enhancer（N3）补无框表/扫描。
