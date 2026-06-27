# Devlog · G4 落地:Form XObject 流解释 + 混合页区域级 OCR

> 日期:2026-06-10 · plan:[closing-docling-gaps.md §G4](../plans/closing-docling-gaps.md) · 零新依赖

## 一、Form XObject 流解释(修正确性债)

解释器此前不执行 Form 内容流——藏在 Form 里的文本/图全漏。现在:

- `images.rs` 解析 `FormX`(自带 content/Matrix/资源,字体与图各自解析,嵌套深度上限 4 防循环);`font.rs` 拆出 `build_fonts_from_resources` 供页面与 form 共用;
- 解释器重构:操作符循环抽成**可递归的 `exec_content`**(每内容流独立 CTM 栈/文本态/路径态),`Do` 遇 Form 即以 `Matrix×CTM` 递归执行;
- 合成验收:form 内文本被抽出且坐标精确(`2 0 0 2 100 600 cm` 变换全对:位置 (120,700)、字高 24=2×12pt)。

**记分牌实效(双刃,如实记录)**:
- 大赢:`right_to_left_02` **0 → 0.972**(整文档内容全在 form 里!)、`skipped_2pages` +0.14、`2206` +0.04;表格检出 2203 4→7、2206 6→9,**TEDS vs ODL 0.098→0.128、vs Docling 0.110→0.157**;vs ODL NID **0.764→0.780**;
- 副作用与修复:form 内图示标签("Transformer"/"Softmax"…)曾被判标题(2203 标题 36 个)——form 文本标 `source:"form"`、标题分类排除之(36→24,MHS 回血);
- 残余分歧:vs Docling NID 0.833→0.818——**form 解出的内容真实存在**,但 Docling 参照不含图内文字;一致度受罚而正确性提升,保留不回退(一致度≠准确率的又一例)。

`source` 字段语义扩展:`None`=页面内容流 / `"form"`=Form XObject / `"ocr:*","vlm:*"`=模型。

## 二、混合页区域级 OCR

数字文本 + 插入扫描图(图章/扫描片段)的页面此前不路由 OCR。现在:

- `quality` 新 flag `MixedTextAndScan`(有文本层 + 带像素载荷的大图)→ 路由至 OCR enhancer;
- OCR 结果与既有可见文本**空间去重**(>50% 面积重叠即弃,数字层永远赢),纯扫描页路径不变;
- 合成验收:数字标题 + 嵌入中文扫描图的混合页——不开 `--ocr` 只有数字文本,开后两者齐全、路由 flag 正确、无重复。

## 终态

93 单测(+1)、clippy 零 warning、三件套零回归。记分牌(Form 流后):vs ODL NID **0.780**/MHS 0.597/TEDS **0.128**;vs Docling NID 0.818/MHS 0.634/TEDS **0.157**。
