# Devlog · H2:方向分类 cls(旋转校正)— 滚动记录

> 2026-06-11 · 里程碑 H2(见 [hardening-iteration.md](../plans/hardening-iteration.md))· 状态:✅ 验收过

## 设计决策(开工前)

- **PP-OCR cls 是行级 0/180 二分类器**(3×48×192 输入),不是页级 0/90/180/270。
  页级四方向 = 两步:① det 框纵横比启发判 90/270(竖排行→先转正 90°);
  ② 转正后采样若干行裁剪过 cls 判 180。与 RapidOCR 的 use_angle_cls 思路同源,
  但我们的 det 后处理是轴对齐矩形(无旋转矩形裁剪),所以采用"旋转整页重跑 det"
  而非"逐框旋转裁剪"。
- **bbox 必须逆映射**:OCR 在旋转后的图上出像素 bbox,要经逆旋转映射回原图
  像素坐标,再走既有的 placement bbox → PDF 用户空间换算,引用定位才不破。
- 模型来源优先级:HuggingFace SWHL/RapidOCR(det/rec 同源)→ ModelScope →
  官方 paddle2onnx 自转;拿不到则按计划文档化跳过。
- 验收口径:rotated 三件与 ocr_test.pdf 文本编辑距离相似度 ≥0.95
  (review 修订,逐字一致对重采样像素差过严)。

## 中途发现(改变问题定义)

检查 docling 的 `ocr_test_rotated_{90,180,270}.pdf` 发现:**图像本体是正的**
(三份与 ocr_test.pdf 是同一张 4960×7016 位图),旋转只是页面字典 `/Rotate`
属性。实测我们当前对 rotated_90 的 OCR 文本**已经逐字正确**——因为 det/rec
看到的就是正图;缺的是 `/Rotate` 坐标口径:页尺寸没互换、bbox 留在未旋转
用户空间,与查看器/真值的"视觉空间"不符,引用定位在带 /Rotate 的 PDF 上会指错。

H2 据此拆为两个子项:

- **H2a · /Rotate 尊重(确定性,无模型)**:PDF 后端读 /Rotate(沿 Pages 树
  继承,同 H7 的 MediaBox 继承)→ 输出坐标变换到视觉空间 + 90/270 页尺寸互换。
  影响所有带 /Rotate 的 PDF(含 born-digital),不只 OCR。docling rotated
  三件套由它收口。
- **H2b · cls(像素级真旋转扫描)**:原计划的模型路——图真转了 det/rec 才乱。
  测试材料需自造(抽 ocr_test 嵌入图、像素旋转后重嵌)。

## 进展记录

- [x] H1 收官,H2 开工;计划文档已标注进行中。
- [x] 排查 rotated 测试件 → 发现是 /Rotate 属性而非像素旋转;H2 拆 H2a/H2b。
- [x] H2a:/Rotate 沿 Pages 树继承(`inherited_attr`,顺带清掉 H7 的 MediaBox
  继承 TODO),**烘进解释器基 CTM**(Path C,渲染器设备变换同法)——坐标原生
  视觉空间、90/270 页尺寸互换;`ImageChunk.turns` 新字段记录放置旋转
  (CTM 线性部分捕捉到最近四分之一转,镜像/剪切不跟踪)。
- [x] H2b:cls 模型从 HF `SWHL/RapidOCR` 的 `PP-OCRv1/ch_ppocr_mobile_v2.0_cls_infer.onnx`
  取得(~0.6MB,比计划预估 1.4MB 还小);可选加载(`*cls*.onnx`),缺失则跳过
  180° 检测。`orient()`:det 框纵横比投票判 90/270(竖条行 ≥2 且多于横条 →
  转正 90° 重 det),cls 多数票判 180(行裁剪 ≥2、单票阈值 0.6)。
- [x] e2e:docling rotated 三件 + 自造像素旋转四向(无 /Rotate)全过。

## 中途翻车与决策:viewer-faithful vs 可读读序

第一版把 OCR bbox 逆映射回视觉空间(viewer-faithful):JSON 坐标正确(竖排
chunk),但 ① text 渲染层不认竖排行(rotated_90/270 输出为空)② 180° 页行序
按视觉空间倒排。本质冲突:**对"显示就是转着的"扫描件,viewer-faithful 坐标
和可读文本流不可兼得**。

**决策**:纯扫描页(无可见确定性文本)检测出旋转 → 整页**归一化到内容正立
坐标系**(Docling 同口径):页尺寸 90/270 互换、既有元素 bbox 经 PDF 空间
旋转变换、OCR bbox 直接在正立帧线性映射。理由:旋转扫描是损坏输入,消费者
(人/RAG UI)终归按正立消费;读序、行重建、引用都顺了。混排页保持
viewer-faithful(确定性文本锚定视觉帧)。为此 `Enhancer::enhance_page`
签名从 `Option<Vec<Element>>` 改为 `Option<Page>`(enhancer 获得改页几何
的能力,trait 实现仅 2 处)。

**已知边界**:混排+旋转页(罕见)OCR 补的竖排 chunk 在 text/markdown 渲染
不可见(JSON/chunks 仍在)——竖排行重建是 H3/H4 层的事,记录不追。

## 验收数据

| 项 | 结果 |
|---|---|
| `ocr_test_rotated_{90,180,270}` vs `ocr_test` 文本相似度 | **1.000 / 1.000 / 1.000**(门 ≥0.95) |
| 归一化页尺寸(rotated_90) | 842×595 → **595×842** ✓ |
| 自造像素旋转 pixrot_{0,90,180,270}(Flate Gray8,无 /Rotate) | 四向全部逐字正确 |
| /Rotate 解释器单测 | 尺寸互换 + 坐标 (100,700)→(700,512) ✓ |
| 回归 | 三件套 ✓;双记分牌(ODL 0.792/0.685/0.419、Docling 0.822/0.643/0.474)与基线一致;H1 CCITT/JBIG2 样例 ✓;clippy 0;全单测 135 绿 |

## 经验

- **先看测试材料的真实构造再写代码**:docling 的 rotated 三件是 /Rotate 属性
  +正图,与"像素旋转"是两个问题;不拆开会做错机制、测错对象。
- **/Rotate 烘进基 CTM 优于事后变换**:born-digital 横排页文本原生水平,
  行重建/表检测全部免费正确;事后旋转 bbox 则要每层补丁。
- **旋转扫描的归一化语义要显式定**(viewer-faithful vs 内容正立),否则
  bbox 对了、读序反了,两边各对一半。参照 Docling 选内容正立。
- det(DBNet+连通域)对 90° 竖排行的框是可用的(纵横比信号清晰),不必为
  90/270 单独引模型;cls 只管 180。

## 2026-06-11 提交前自审(7 角度 finder)修复清单

正确性修复(H1/H2 范围):截断补白先于 /Decode 反转(黑带)→ 按反转极性补;
det_boxes 四边全 clamp + orient 投票 saturating(usize underflow / 误转 90°);
**90° 修正门控在 cls 存在上**(无 cls 时 90 vs 270 是掷硬币,转错比不转糟,
语义改为"无 cls 即禁用旋转校正",README/模块文档同步);镜像放置 (a<0,d>0)
不再误记 180(turns=2 需 a、d 双负);/Rotate Real 型兼容;JBIG2Globals 带
Filter 解码失败不再回退原始字节;1-bit Indexed 调色板位置占位(防负片);
ImageMask 收紧为仅 CCITT/JBIG2 滤镜(防水印 stencil 被 OCR 注入文本);
旋转页基 CTM 折入 MediaBox 原点(非零原点 + /Rotate 全坐标偏移并误触
off-page 隐藏判定)。清理:Gray8Sink 合一(双 trait impl)、删 ocr_rgb 死包装、
推理失败上报去重、cls 投票多数即早退、预旋转 buffer 及时 drop、packed 1-bit
逐字节展开。遗留项(组合增强器错帧/导出忽略 turns/转写无方向校正/走查器
统一)记入计划 H7 自审遗留段。回归:旋转全家桶 7×1.000、CCITT/JBIG2、
三件套、双记分牌全部与基线一致;clippy 0;135 测试绿。
