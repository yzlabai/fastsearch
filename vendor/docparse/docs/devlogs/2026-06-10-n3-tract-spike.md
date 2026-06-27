# Devlog · N3 门控 spike:tract(纯 Rust)跑通 PP-OCRv4 —— **通过,P4 路线成立**

> 日期:2026-06-10 · plan:[n3-real-enhancer.md §-1](../plans/n3-real-enhancer.md) · spike 代码在 `tmp/tract-spike`(不合入)

## 结论

**纯 Rust 路线成立,身份零妥协**。`tract-onnx 0.21` 完整跑通 RapidOCR 的 PP-OCRv4 mobile det(DBNet)与 rec(SVTR-LCNet)模型,对 `chinese_scan.pdf` 第 1 页(2480×3508 RGB,ASCII85+Flate,lopdf 直接解出裸像素)识别质量惊艳:

```
中国公司年度报告
第一章：公司概况
本公司成立于2020年，是一家专注于人工智能
技术研发的高科技企业。公司总部位于北京，
庄上海丶深圳设有分支机构。   ← 仅 2 错字(在→庄、、→丶),系 spike 的最近邻缩放+粗糙行框所致
```

耗时:det(960×704)+ 5 行 rec **共 1.06s**(含 rec 模型每行重复加载的浪费;真实现一次加载)。对照 refer §4:这个质量已远超 tesseract `chi_sim` 的预期水平。

## 关键发现(实现时要带走的)

1. **paddle2onnx 维度名要消毒**:模型的 dim_param 叫 `p2o.DynamicDimension.0`,tract 的 TDim 解析器吃不下点号。**等长字节替换 `.`→`_`**(protobuf 字符串定长,文件不破)即可。实现时在模型加载处做内存内替换(`model_for_read`),不要求用户改文件。
2. **动态形状要钉死**:`with_input_fact` 给具体 NCHW(det:max-side 960 且 32 对齐;rec:高 48、宽随框缩放)。每种输入尺寸一次 `into_optimized`——实现时按宽度桶(如 320/640/1024)缓存 runnable 模型,避免每框重优化。
3. **归一化**:det 用 ImageNet mean/std;rec 用 `(v-0.5)/0.5`。CTC 贪心解码:idx 0 = blank、1..=6623 → 字典行、6624 → 空格。
4. cls(方向分类)模型在 SWHL/RapidOCR HF 仓库未取到——MVP 跳过(扫描件多正向),显式 TODO。
5. 模型获取:HF `SWHL/RapidOCR` 的 `PP-OCRv4/ch_PP-OCRv4_det_infer.onnx`(4.7MB)+ `ch_PP-OCRv4_rec_infer.onnx`(10.9MB);字典 PaddleOCR `ppocr_keys_v1.txt`(6623 行)。本地放 `models/ppocr/`(已 gitignore,作外部文件分发)。

## 下一步(按 plan §-1)

新 crate `docparse-ocr`:`tract-onnx` 依赖、det 后处理(阈值+连通域成框)、双线性缩放、CTC 解码、`impl core::enhance::Enhancer`;解释器补 `Do`/ImageXObject 抽取入 IR;CLI `--ocr --ocr-models <dir>`。
