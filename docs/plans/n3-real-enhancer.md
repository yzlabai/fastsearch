# N3 · 真实 enhancer:**首选 P4 路线(ONNX 内嵌)**,tesseract CLI / HTTP 为备选(模块 8)

> 调研依据:[refer/n3-enhancer-odl-docling-research.md](../refer/n3-enhancer-odl-docling-research.md)(ODL hybrid 架构 + Docling OCR 引擎层 + 引擎质量评估)。
> 状态:✅ **P4 路线已落地**(2026-06-10,devlog:[n3-onnx-ocr](../devlogs/2026-06-10-n3-onnx-ocr.md)):`docparse-ocr` crate(tract 纯 Rust 推理)+ 解释器 ImageXObject 抽取 + CLI `--ocr`。chinese_scan 14/14 行全对、数字页零模型、确定性逐字节、双记分牌零回归。tesseract 备选(§1)未做、HTTP(§2)留远期。

## -1. 路线决策(2026-06-10 更新):P4 ONNX 内嵌为首选实现

用户判断 + 调研支持:**跳过 tesseract 作为主路线,直接按 P4(ONNX 内嵌 RapidOCR/PP-OCR 模型)实现 N3**。理由:

- **中文质量**:PP-OCRv4/v5 模型即中文事实标准,甩开 tesseract `chi_sim` 一个量级(refer §4);
- **部署形态**:进程内推理,无 Python/外部服务/子进程,与"单二进制易分发"同构;
- **模型小且许可干净**:PP-OCRv4 mobile det(~4.7MB)+ cls(~1.4MB)+ rec(~10MB)≈16MB,Apache-2.0;作**外部文件**按需加载(`--ocr-models <dir>`),二进制不变、OCR 仍是可选增强,差异化指标不破;
- roadmap P4 本就预留此路("ort/Candle 在 Rust worker 内推理")。

**前置决策点(实现启动时的第一件事):推理运行时选型**——身份约束"无 JVM/C++"与最成熟的 `ort`(onnxruntime,C++)冲突:

| 运行时 | 性质 | 风险/代价 |
|---|---|---|
| **`tract`**(首选,先 spike) | 纯 Rust ONNX 推理,身份零妥协 | rec(SVTR/CRNN)算子覆盖需验证;CPU 比 onnxruntime 慢 2–5x(按页触发,可接受) |
| `ort` | C++ 依赖,最成熟最快 | 破"无 C++";只可作 feature-gate 妥协 |
| `candle` | 纯 Rust | ONNX 支持弱,需转换链,不推荐 |

**Spike 判据**:tract 跑通 RapidOCR 三模型(det/cls/rec)→ 纯 Rust 路线成立;跑不通 → 在 `ort` feature-gate 与 N3b HTTP 之间二次决策。

**实现量级(P4 路线)**:比 tesseract CLI 大 3–5 倍——需自做推理前后处理:JPEG→像素张量(新增纯 Rust 解码依赖,如 `zune-jpeg`/`image`,**征询后引入**)、det(DBNet)二值化+轮廓后处理、rec 的 CTC 解码+字典映射。Enhancer 边界/按页触发/source 标签/坐标折回与下文 §1 完全复用。

下文 §1(tesseract CLI)**降级为备选**:仅当需要"最快证明边界端到端"或 tract spike 失败且不愿引 C++ 时采用;§2(HTTP)仍是接最强模型(VLM/版面)的远期通道。

## 0. 关键设计决策(调研结论)

**不渲染也能 OCR**:扫描件的页就是一张嵌入光栅图。抽 ImageXObject **原始字节**(DCTDecode→.jpg 直写;FlateDecode 灰度/RGB→PPM 头+裸字节,零依赖)喂引擎——"结构提取"而非"光栅渲染",身份约束(CLAUDE.md/roadmap §1)不破。

## 1. N3a · TesseractCliEnhancer(零 Rust 新依赖)——**备选**(见 §-1)

- **整页图抽取**(docparse-pdf):页内容流遇 `Do` + XObject `Subtype/Image` 时记录(放置 CTM、像素尺寸、滤镜);"扫描页"判据 = 单一大图覆盖页面大部 + 无文本(quality 已判 `scanned_no_text`)。MVP 只支持 DCTDecode(JPEG)与 FlateDecode 灰度/RGB(写 PPM);其余滤镜(JBIG2/CCITT/JPX)显式 TODO 报"不支持的扫描编码"。
- **enhancer 实现**(CLI 层,不进 core):实现 `core::enhance::Enhancer`,参考 Docling `TesseractOcrCliModel` 独立重写:
  - 子进程 `tesseract <img> stdout tsv -l <langs> [--psm N]`;TSV 逐词解析(text/bbox/conf),`conf/100` 归一;
  - tesseract 二进制**运行时可选**:`--version` 探测,缺失 → 维持现状(StubOcr 路径/路由报告"需 OCR"),不报错;
  - 像素 bbox 经图片放置 CTM 折回 PDF 用户空间;每词一个 `TextChunk`,confidence=conf/100。
- **元素级 `source` 标签**(M7 遗留,本里程碑一并做):IR 元素标 `source: "pdf" | "ocr:tesseract"`,chunk 信封透传——下游可审计哪段文本来自模型。
- **CLI**:`--ocr`(默认关,数字页永不触发)+ `--ocr-lang`;触发条件复用 M7 路由(仅 `scanned_no_text`/高乱码页)。
- **验收**(注意:tesseract 中文质量弱——见 refer §4——故验收只验**边界端到端正确**,不以中文字符准确率为硬门;中文质量正解在 N3b/P4):
  - `chinese_scan.pdf`:0 文本 → 大致可检索中文文本 + bbox 引用 + `source: ocr:tesseract` + 低 confidence;
  - 数字页(三件套/1901/2408)**零模型零子进程**(单测钉死 plan 为空);
  - tesseract 不在 PATH:行为与今日完全一致(优雅缺省);
  - 双记分牌零回归;clippy 零 warning。

## 2. N3b · HTTP enhancer(对齐 ODL hybrid,后置)

- docling-serve 兼容的 HTTP 后端调用(POST 页图/PDF → 结构化结果归一回 IR),后端可换(对齐 ODL `--hybrid-url` 思路)。
- **新依赖**(HTTP 客户端,如 `ureq`/`reqwest`)按 CLAUDE.md §4 先征询;触发与归并复用 N3a 的路由与 source 标签。
- 价值:接最强模型(VLM/TableFormer 级)补 CJK 复杂版面与无框表——聚合记分牌剩余 gap 的正解。

## 3. 边界

- OCR 引擎**永不进核心**:进程/HTTP 之外零耦合,主流程无之独立运行(M7 单测已钉)。
- 触发"按页",数字页零成本(roadmap §3 成本论点)。
- 图片抽取能力顺带对齐 ODL `--image-output`(带 bbox 导出),可作独立小特性先行。
