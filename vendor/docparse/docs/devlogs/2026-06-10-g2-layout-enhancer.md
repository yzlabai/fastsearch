# Devlog · G2 落地 + 诚实负结果:版面 enhancer 基建就绪,但"CJK gap = 宏观区域序"假设被否决

> 日期:2026-06-10 · plan:[closing-docling-gaps.md §G2](../plans/closing-docling-gaps.md) · 前置:[光栅决策](../refer/rasterization-options-analysis.md)(定位修订:主流程不渲染,难页 opt-in)

## 交付(全部就绪,默认关闭,零回归)

| 层 | 内容 |
|---|---|
| **docparse-raster**(新 crate) | hayro 纯 Rust 渲染封装:`Rasterizer::new(bytes)` + `render_rgb(page, scale)`;失败=该页跳过增强,绝不影响解析 |
| IR 0.5.0 | `TextChunk.group: Option<u32>` **阅读组**:layout 重建先按组排序、组内仍跑 XY-cut——模型管宏观序,确定性管微观序 |
| docparse-ocr::layout | DocLayout-YOLO 推理(letterbox 1024/已解码框/无 NMS)→ 区域映射回 PDF 坐标 → **区域序用 core XY-cut 求**(合成 chunk)→ chunk 按中心+重叠归组;`DOCPARSE_LAYOUT_DEBUG` 可观测 |
| CLI | `--layout --layout-model <path>`(PDF,opt-in,~2.4s/页) |

84 单测(+2)、clippy 零 warning、三件套零回归;不开 `--layout` 一切路径不变。二进制 23.5MB(hayro+vello +4.4MB)。

## 负结果一:CJK 信息图的 gap 不是宏观区域序

`normal_4pages`/`skipped_*` 开 `--layout` 后 NID **一字不动**(0.580/0.222/0.123)。根因:页 1 的 524/622 个 chunk 不落在任何检出区域(信息图的彩色 label-value 块不是版面模型眼中的 text region),且检出区域的宏观序与 XY-cut 本就一致。**与 ODL 的分歧在区域内部的微观序(label-value 行列序)——区域级版面模型在原理上救不了**。此假设(plan §G2 的预期收益)否决;CJK gap 重新归类:G8b 整页 VLM 或接受参照分歧。

## 负结果二:hayro 对部分真实 PDF 渲染损坏

`2206`(DocLayNet 论文)渲染成整页黑底、内容缩在角落(透明组/页框类 bug,上游自述 WIP 区);`normal_4pages` 则渲染完美。**外部工具兜底链(分析文档的 A 选项)从"按需后补"提级为 G2 后续必做**;另需加"坏渲染检测"(大面积黑/空白启发式)防止坏图喂模型。

## 为什么基建保留(不回退)

- 零回归、默认关、opt-in;
- raster + 区域检测 + 阅读组是 **G3(表结构)/G8(公式/图片/VLM)的共用底座**——G2 的"版面修序"只是它的第一个(被否决的)用例;DocLayout-YOLO 的 table/figure/formula 区域定位在 G3/G8c 直接复用;
- 阅读组机制对未来任何"宏观序提供者"(VLM、规则)通用。

## 教训

假设要带验收数字上场:plan 写了"`skipped_*` ≥0.5"的验收,落地当天即证伪——好过带着错误预期继续投入。下一个能动 CJK gap 的候选只剩 G8b 整页 VLM(它读内容而非框几何)。
