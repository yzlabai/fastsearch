# 调研 · N3 真实 enhancer:ODL 与 Docling 的 OCR/模型接入架构

> 日期:2026-06-10 · 源码:`../opendataloader-pdf`(hybrid 设计)与 `tmp/refer/docling`(OCR 引擎层)
> 结论已细化为执行计划:[plans/n3-real-enhancer.md](../plans/n3-real-enhancer.md)。**用户决策:暂只落档,不实现。**

## 1. ODL:核心零模型,难例 HTTP 外接(与我方哲学同构)

- **本体(Java/veraPDF)没有任何内置 OCR/神经模型**;扫描页默认输出空文本,无警告。
- **Hybrid 模式**是它的答案:TriageProcessor(保守策略,宁多送)把复杂/扫描页经 **HTTP 发给 docling-serve 后端**,Result Merger 把结果归并回主流程、保读序。后端可换(`docling-fast` 现有;hancom-ai/Azure/Google 规划中)。见 `docs/hybrid/hybrid-mode-design.md`。
- OCR 引擎选择直接**委托 docling 的 `get_ocr_factory`**,不自研引擎抽象。
- 前置能力:**图片抽取**——`--image-output {off,embedded,external}`,每图带 `[l,b,r,t]` bbox + 页码,供外部 OCR/VLM 消费。
- 可借鉴:外接边界放在**进程/HTTP**粒度;triage 宁可多送;合并时保 provenance。

## 2. Docling:工厂 + 判别式 options 的引擎插件层

7 个 OCR 引擎(auto/easyocr/rapidocr/tesserocr/**tesseract-cli**/ocrmac/kserve 远程)注册于 `docling/models/plugins/defaults.py`,经 pluggy 工厂(`models/factories/`)按 `options.kind` 判别式选择。TableFormer/公式/图片分类 enrichment 走同一模式。

**TesseractOcrCliModel(我方最该抄的)**,`models/stages/ocr/tesseract_ocr_cli_model.py`:

```python
cmd = [tesseract_cmd, ..., ifilename, "stdout", "tsv"]
output = subprocess.run(cmd, stdout=PIPE, stderr=DEVNULL, check=True)
df = pd.read_csv(StringIO(output.stdout.decode()), sep="\t", quoting=QUOTE_NONE)
# 每词: text + bbox(像素) + conf;置信度 conf/100 归一
```

- 纯子进程边界、无链接依赖;版本探测 `tesseract --version`;OSD(方向)失败与 OCR 失败分别捕获、可降级继续。
- **触发策略**(`base_ocr_model.py::get_ocr_rects`):页面位图区域(backend 给 bitmap rects)做 20×20 膨胀合并 → 覆盖率 > `bitmap_area_threshold`(默认 0.05)做**区域 OCR**;> max(0.75, 阈值) 或 `force_full_page_ocr` 做**整页**;否则不 OCR。
- **去重**:OCR cell 与程序化文本 cell 经 R-tree 空间索引过滤重叠。
- **provenance**:cell 带 `confidence`(各引擎归一到 0–1)+ `from_ocr: bool`。
- **渲染**:统一 `scale=3`(72→216 DPI)页面图;坐标按 `coord/scale + region_offset` 折回。

## 3. 对 docparse-rs 的设计推论

**解开"不光栅化"死结**:OCR 要图,但扫描件的页**本来就是一张嵌入光栅图**(整页 DCTDecode JPEG XObject)。抽嵌入图**原始字节**(JPEG 直接落盘;Flate 灰度/RGB 写 PPM 头+裸字节,零依赖)是"结构提取"不是"渲染"——身份约束不破。我们不渲染矢量内容,只取出 PDF 里已有的位图。

| Docling/ODL 的做法 | docparse-rs 落点 |
|---|---|
| tesseract CLI 子进程 + TSV + conf/100 | `TesseractCliEnhancer`(零 Rust 新依赖,tesseract 为**运行时可选**二进制,缺失→现路由报"需 OCR") |
| 页面渲染 3x 喂引擎 | **不渲染**:抽整页 ImageXObject 原字节(JPEG/PPM) |
| 像素坐标 /scale + offset 折回 | 像素坐标经**图片放置 CTM** 折回 PDF 用户空间(解释器已有 CTM 跟踪) |
| `from_ocr` + confidence | M7 已有 chunk confidence;补**元素级 `source` 标签**(M7 遗留) |
| bitmap 覆盖率触发 | `quality::assess_page`(scanned_no_text)已是现成触发器 |
| ODL hybrid HTTP 后端 | N3b:docling-serve 兼容 HTTP enhancer(需 HTTP 客户端依赖,届时征询) |

**验收样例就用现成的 `chinese_scan.pdf`**:从 0 文本到可检索带 bbox 引用;数字页(1901/2408)仍零模型零外呼。

## 4. 引擎质量补充(2026-06-10 调研):tesseract 中文是短板,预期要设对

- **tesseract 5.x(LSTM)**:干净印刷体拉丁文 300DPI 可达 95–99%,CPU、~10MB、快;但对版面复杂度敏感(复杂版面 30–60%)。
- **中文(`chi_sim`)明显弱**:官方模型在真实文档表现差(社区报告复杂场景极低、复合字常整字错),LSTM 有"字符间插空格"已知 bug 需后处理;有社区重训包(gumblex/tessdata_chi)替代官方。调优后(tessdata_best+300DPI+psm)清晰简体约 80–90% 字符级,与中文事实标准差距明显。
- **中文事实最优是 PaddleOCR**(PP-OCRv5 2025-05;PaddleOCR-VL-1.5 2026-01 文档解析 94.5%),代价 Python/Paddle 运行时;**RapidOCR**(PP-OCR 的 ONNX 移植,onnxruntime,无 Python)与我方 **P4(`ort` 内嵌)天然契合**——"纯 Rust 部署 + 中文质量"的潜在两全。
- Docling/ODL 默认引擎也是 EasyOCR 而非 tesseract;tesseract 是它们的零依赖兜底——与 N3a 给它的定位一致。
- **对计划的修正**:N3a 验收定为"边界端到端正确"(0 文本→大致可检索+bbox+provenance),**不以中文字符准确率为硬门**;中文质量正解在 N3b(HTTP 外接 Paddle 服务)或 P4(RapidOCR ONNX 经 ort 内嵌)。
