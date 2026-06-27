# 调研 · PaddleOCR 模型选型:v5 是否更好、如何接入(2026-06-10,带实测)

> 触发:用户问"是否用 PaddleOCR 模型更好"。**澄清前提:现役模型就是 PaddleOCR 系**(PP-OCRv4 mobile 的 ONNX 导出,RapidOCR 仓库)。本文回答的是"Paddle 生态内有没有更好的型号/接法"。

## 1. 候选与事实

| 型号 | 体积 | 能力 | ONNX 可得性 | tract 兼容(实测) |
|---|---|---|---|---|
| PP-OCRv4 mobile(现役) | ~16MB | 简中+EN | ✅(SWHL/RapidOCR) | ✅ 2.0s/页(chinese_scan 端到端) |
| **PP-OCRv5 server** | **172MB** | 简/繁/拼音/EN/JP 一个模型;手写/古籍/复杂场景大幅提升(官方宣称端到端 +13pp) | ✅(monkt/paddleocr-onnx,另有韩/阿等语言包) | ✅ **6.0s/页**;实测修正 v4-mobile 在样例上的两处错字(在/、) |
| PP-OCRv5 mobile | ~20MB 级 | 同上精度略低 | ❌ 各源未见现成,需自己 paddle2onnx 转(待办) | 未测(同架构,预期可行) |
| PaddleOCR-VL-1.5 | 0.9B 参数 | 整页文档解析(版面+表格+公式一体),文档解析 94.5% | 不适用 ONNX/tract | **走 G8b 服务路线**:vLLM 部署后经我们的 OpenAI 兼容客户端调用 |

字典差异:v5 用 18383 字大字典(覆盖繁体/拼音/日文)vs v4 的 6623——我们的 CTC 解码本就按字典长度泛化,无须改。

## 2. 接入(已完成,本 commit)

`docparse-ocr` 两处泛化后,**任何 PP-OCR 系模型目录直接 `--ocr-models <dir>` 即用**:

1. **维度消毒器**支持双前缀(`p2o.DynamicDimension.` 与裸 `DynamicDimension.`,v5 导出用后者);
2. **模型文件自动发现**:v4 精确名优先,否则按 `*det*.onnx` / `*rec*.onnx` / `*dict*.txt` 匹配——v5 目录免改名。

```bash
# v5 server(质量优先):
docparse scan.pdf --ocr --ocr-models models/ppocr-v5
```

## 3. 结论与建议

- **默认保持 v4 mobile**:16MB/2 秒页,与"速度快"定位一致;样例上 v5 的优势是个别字粒度;
- **v5 server 作质量档**(已可用):手写/繁体/日文/复杂场景需求时切换,172MB/6 秒页;
- **最优解是 v5 mobile**(v5 精度 + mobile 体积):待办=用 paddle2onnx 自转(需 Python 环境,一次性);
- **PaddleOCR-VL 不走本地推理**:它是 VLM,正确接法是 vLLM 起服务 + 我们 G8b 客户端——整页转写任务的首选后端;
- 多语种(G4 余项)顺带解锁:monkt 仓库有韩/阿拉伯/印地等 rec 语言包,`--ocr-models` 指向即可(每语言一目录)。
