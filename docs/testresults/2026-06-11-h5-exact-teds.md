# 测试结果 · H5:精确 TEDS(Zhang-Shasha 树编辑距离)上线

> 2026-06-11 · 里程碑 H5(见 [hardening-iteration.md](../plans/hardening-iteration.md))
> 脚本:`scripts/eval/score.py`(新增 `teds_x` 列)+ 三个 extractor 的 span 透传

## 做了什么

旧 `TEDS` 是结构代理(网格形状 + 行对齐 DP),两次掩盖真实变化、且 span 入 IR
后无法奖励 span 结构。新增 **`TEDS_X`**:把表构造成 `<table><tr><td:rs×cs>` 树,
用 **Zhang-Shasha 精确树编辑距离**(PubTabNet 代价模型:增删=1,同 span 的 td
改名=归一化文本差,异 span=1)计分,纯标准库 ~80 行、评测侧零依赖。三个
extractor(我方 `-f json`、docling GT、ODL)都改为透传 span 锚点
(`tables_cells`,只记 anchor、不重复被覆盖位)。proxy 列**保留对照**,composite
仍走 proxy 以保持历史记分牌可比。

## 双记分牌(proxy TEDS vs 精确 TEDS_X)

含表子集聚合:

| 参照 | 文档数 | proxy TEDS | 精确 TEDS_X |
|---|---|---|---|
| vs ODL(确定性同类) | 7 | 0.419 | **0.477** |
| vs Docling(神经管线) | 6 | 0.474 | **0.526** |

逐文档分歧最大的几例(换尺有真实信号差,不是单调偏移):

| 文档 | 参照 | proxy | exact | 差异来源(逐个核对) |
|---|---|---|---|---|
| normal_4pages | ODL | 0.400 | 0.723 | 行数差(我方 3 行 / GT 5 行,漏表头 2 行);proxy 的行对齐 DP 对漏行惩罚过重,exact 按树编辑只算增删两行 |
| 2206.01062 | ODL | 0.421 | 0.512 | 同上类,多表配对后 exact 更宽容缺行 |
| redp5110_sampled | ODL | 0.859 | 0.935 | 结构基本对,exact 不因列内容轻微错位连锁清零 |
| 2305.03393v1-pg9 | ODL | 0.804 | 0.745 | **反向**:proxy 因压扁网格"碰巧"对齐给高分;exact 看出我方少 1 行表头 span,扣得更准 |

## span 输出在精确尺下的验证(2305.03393v1-pg9,GT 有 6 个 span 锚点)

| 配置 | span 锚点数 | proxy TEDS | 精确 TEDS_X |
|---|---|---|---|
| 默认(确定性,压扁网格) | 0 | 0.819 | 0.765 |
| `--table-model`(UniRec,真 span) | 14 | 0.390 | 0.408 |

**结论(诚实)**:`--table-model` 确实产出 span(6 表头 + 8 数据行 span 锚点),
精确尺也确实**奖励**了 span 结构(它不再像 proxy 那样把 span 压扁口径反噬)——
但本例 `--table-model` 把表识别成 10 行(GT/默认是 6/5 行),**多检了 4 行**,
行数错误的扣分盖过了 span 收益,所以两个尺子下都比默认低。换言之:H5 把"尺子
不奖励 span"的问题修好了(同样行结构下 span 对齐现在有分),但 `--table-model`
当前在本文档上的**行切分**有 bug(把每个物理行拆成两行),这是 G3-R 模型侧
的已知噪声,不属 H5 范围 —— 记入按需池 G3b/table-model 行切分。

## 验收

- `score.py --selftest`:新增 4 条 span 断言(identical span=1.0、压扁失分、
  grid fallback=1×1、对结构+1 错格 > 压扁)全过;
- proxy 列、composite、历史聚合数字**未变**(0.419/0.474 原样),纯增量上线;
- 边界:`tables_cells` 缺失的源回退 1×1 网格(等价表、不给 span 加分),向后兼容。
