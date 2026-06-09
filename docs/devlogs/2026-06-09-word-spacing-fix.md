# Devlog · 词距修复（参考 veraPDF，提 NID + 全局可读性）

> 日期：2026-06-09 · 类型：质量修复（承 N1 诊断）· 状态：✅ 完成
> 结果：NID 0.601→**0.625**；学术/CM 字体文档标题/作者/摘要不再粘连；零回归

---

## 1. 诊断（数据驱动）

N1 同台发现 NID 偏低。词级 vs 字级（去空格）相似度对比显示约 6 点来自**词距粘连**。在 2206.01062（DocLayNet）上 dump 原始 chunk 坐标，定位根因：

- 词间空格是**恰好 0.25em** 的间隙（`Birgit`→`Pfitzmann` = 3.0/12 = 0.25em）；词内间隙 ~0.01em。
- 我方阈值 `gap > 0.25em`（严格大于）= **恰在空格宽度上** → 0.25em 的空格 `3.0 > 3.0` 为假 → 不插空格 → `BirgitPfitzmann`。标题侥幸过（4.3/17=0.253）。

## 2. 参考 veraPDF 校正

读 `veraPDF-wcag-algs`：
- `TextChunkUtils.WHITE_SPACE_FACTOR = 0.25`（空格宽 ≈ 0.25em）；
- `TextChunkUtils.SPLIT_THRESHOLD_FACTOR = 0.21`（词切分阈值，`findWideWhiteSpaces`）。

关键：切分阈值 **0.21 < 空格宽 0.25**，留出余量，使恰好一个空格的间隙可靠触发切分。我方用 0.25（=空格宽）是错的。

**修复**：`layout::reconstruct_lines` 词距阈值 `0.25em → 0.2em`（`WORD_GAP_EM` 常量，注释引 veraPDF）。

## 3. 效果

- **文本**：`DocLayNet: A Large Human-Annotated Dataset for ... Birgit Pfitzmann Christoph Auer ... IBM Research ... Rueschlikon, Switzerland`——全部正确（原全粘连）。影响**所有**用这类字体的文档、**所有**输出格式（text/markdown/chunks）。
- **量化**：NID 0.601→**0.625**（10 份 LTR 平均），MHS 0.257→0.265。
- **零回归**：lorem/bialetti 不过切；三件套不变；IR chunk 数不变（仅输出层）；确定性 20/20；clippy 零 warning；单测 44。

## 4. 边界

- 0.2em < 0.21（veraPDF）略保守；词内字距异常（letter-spacing/tracking）的标题理论上可能过切，实测罕见、收益远大于风险。
- 剩余 NID 差距（~0.625 vs Docling）主要在**多栏阅读顺序**（如作者块跨栏横读，应按栏纵读）与内容取舍——见 N1 诊断，下一步候选。

## 5. 下一步

候选：① 多栏阅读顺序（XY-cut 列分割，作者块/双栏正文按栏读，参考 ODL/Docling 版面分析）；② 标题分级（提 MHS）；③ N4 续（列推断/合并单元格）。
