# Devlog · G2 门控 spike:DocLayout-YOLO × tract —— 通过,但 born-digital 输入需要光栅决策

> 日期:2026-06-10 · plan:[closing-docling-gaps.md §G2](../plans/closing-docling-gaps.md) · spike 代码 `tmp/layout-spike`(不合入)

## 结论一:模型可用,效果好(✅ 门控通过)

- 模型:**DocLayout-YOLO**(YOLOv10 系,DocStructBench 训练含中文,75MB ONNX,`wybxc/DocLayout-YOLO-DocStructBench-onnx`);RapidLayout 的 PicoDet CDLA 小模型未在 HF 取到(后续可再找,75MB 作外部文件可接受)。
- `tract 0.21` 直接跑通,**无需维度消毒**;输出 `(1,300,6)` 已解码框(x1y1x2y2+score+class),无需 NMS。
- 在 `normal_4pages`(韩文信息图,我方 NID 0.58 的难页)的真实渲染上:**正确识别双栏结构**(左栏/右栏 plain_text 各 3-4 块)、title、页眉页脚(abandon 类正好可喂 N5a/页眉页脚过滤)。推理 2.37s/页(1024²,CPU)——只在难页触发,可接受。
- 10 类:title/plain_text/abandon/figure/figure_caption/table/table_caption/table_footnote/isolate_formula/formula_caption——**table 与 formula 区域同时拿到**,G3/G8c 的区域定位可共用这一个模型。

## 结论二:"文本框草图"替代渲染——否决

尝试用 IR 文本框画草图喂模型(白底+词级灰块+行间隙,两种参数),模型一律把整页判成一张 `figure`(0.81/0.84)——框图在训练分布外。**born-digital 页要用版面模型,必须有真实页面光栅。**

## 由此暴露的根本决策(G2/G3/G8c/G8d 共用)

所有"吃区域图/页面图"的神经增强,对 born-digital 页都需要光栅来源。三条路:

| 选项 | 形态 | 身份影响 |
|---|---|---|
| A. 外部光栅进程(推荐) | 运行时可选 `pdftoppm`/`sips`/`mutool` 子进程,仅 enhancer 路由的难页触发(同 tesseract-CLI 模式) | 二进制零新依赖;"不光栅化"改述为"主流程不光栅化,enhancer 可经外部工具按页光栅(opt-in)" |
| B. 纯 Rust 文本渲染器 | ttf-parser + tiny-skia 自研 text-only 渲染(字形轮廓+定位已有) | 身份最纯但是大工程(数周级),且只覆盖文本(图/矢量缺) |
| C. 仅扫描页 | born-digital 不接版面模型,CJK gap 留给 VLM(但 VLM 同样要页面图,问题不消失) | 不解决记分牌剩余 gap |

spike 用 macOS `sips` 渲染验证(仅测试用途)。**待用户拍板**后 G2 落地。
