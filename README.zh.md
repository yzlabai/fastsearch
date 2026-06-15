# docparse-rs

[English](README.md) | **中文**

高效、纯 Rust 的**多格式文档解析系统**：从 PDF/DOCX/HTML/XLSX/PPTX/Markdown/CSV/SRT·VTT/LaTeX/EML/PNG·JPEG/AsciiDoc 抽取**带位置的结构化内容**（文本/版面/阅读顺序/表格 → 统一 IR → JSON / Markdown / Text / RAG chunks），走"结构提取"而非"光栅渲染"的快路径。面向 Agent / RAG：结果**确定、可复现、可引用**（每个 chunk 带 page+bbox 双向溯源）。

> 设计动机来自对 [opendataloader-pdf](https://github.com/opendataloader-project/opendataloader-pdf) 的架构分析：它快，是因为默认从不把页面渲染成像素，只解析内容流拿坐标，再逐页并行做版面分析。docparse-rs 用纯 Rust 复刻并延伸这条快路径——无 JVM、无 C++、无 Python，单二进制。

## 亮点

- **单二进制 26.5MB、运行时依赖 0**（含两套纯 Rust 推理栈+按需渲染器）：预热解析 <10ms、700 页/s、同输入逐字节确定;
- **四接口一份输出**：CLI / 库 / MCP（stdio，agent 直连）/ REST，跨接口**逐字节一致**（含 OCR 路径）;
- **RAG 一等公民**：结构化切块带 page+bbox+标题面包屑，`locate(x,y)` 坐标反查 chunk，引用可定位率 100%;
- **安全预检内置**：隐藏文本过滤（防 prompt injection，标注可审计而非静默删除）、zip-bomb/页数资源守卫、页级复杂度画像;
- **扫描件 OCR 不破纯 Rust 身份**：`--ocr` 走进程内 `tract` ONNX 推理（PP-OCRv4，中文事实标准模型，~16MB 外部模型文件），抽嵌入图原字节而非渲染;扫描编码覆盖 JPEG/Flate/**CCITT G3·G4 传真压缩/JBIG2**（JPX 暂只记位置）;按页路由,数字页**零模型零成本**;
- **内嵌语义模型（opt-in，无服务依赖）**：表结构（合并格/多级表头 → rowspan/colspan 入 IR）、公式→LaTeX、整页转写，UniRec-0.1B 经 `tract` 进程内推理（~700MB 外部模型文件）;
- **可插拔 AI 边界**：确定性主流程独立成立,模型只在质量评分判定难例时按页触发,产出带 `source` 标签与降级置信度（进程内 tract 或 OpenAI 兼容服务外接均可）。

## 当前状态与记分牌

十大功能模块全部闭合（IR/PDF/版面/语义/多格式/输出 RAG/质量路由/AI 外接/安全/服务化）。

### OmniDocBench 测评（人工真值）

[OmniDocBench](https://github.com/opendatalab/OmniDocBench)（CVPR 2025，opendatalab）对**人工标注**真值打分——表格为 HTML/LaTeX 含真实合并格 span、公式为 LaTeX、文本与阅读顺序。它正是 **OpenDoc-0.1B / UniRec（我们内嵌的模型）自报 90.57% 的 benchmark**。我们用自有指标在数据子集上评分（表用精确树编辑距离 TEDS_X、公式用 LaTeX 字符相似度、文本用字符级相似度——中文无词间空格）：

| 维度 | 路径 | 分数 |
|---|---|---|
| **文本识别** | `--transcribe-model`（UniRec），学术论文 | **0.872** |
| **公式 → LaTeX** | UniRec，学术论文（全集 0.708） | **0.874** |
| **表结构** | UniRec 单模块，80 表 | **0.810**（median **0.895**） |
| 表，端到端 | 检测 + 识别，单表页 | 0.827 |
| 表，端到端 | 学术论文（最难类） | 0.517 |
| 文本，轻量档 | `--ocr` PP-OCRv4 mobile（16MB） | 0.42–0.44 |

**直白读法：**
- **强项** —— 用内嵌 UniRec 模型，**文本与公式都达 ~0.87（接近论文级）**；clean 表区 **0.810，一半的表近乎完美（median 0.895）**。
- **弱项** —— **学术难表是最大缺口**（端到端 0.52：多级表头 + 密集数字 + 含 LaTeX）。轻量 `--ocr` 档（mobile，16MB）只有 ~0.44——它定位"数字页零成本、扫描补充"，非重型图像 OCR；要质量请用 `--transcribe-model`。

**整体落位。** 把论文子集分数套进 OmniDocBench 官方 Overall 公式 `((1−text_edit)·100 + table_TEDS + formula_CDM)/3` 得 **≈75**——这是粗略代理（自有指标、子集、表/公式为单模块），**不可与下表官方分逐位比**：

| 系统 | 类型 | Overall |
|---|---|---|
| MinerU2.5-Pro / GLM-OCR | 专用 VLM | ~95 |
| **OpenDoc-0.1B**（我们内嵌的 UniRec） | 专用 VLM | **90.67** |
| GPT-4o / GOT-OCR | 通用 / 专家 VLM | ~86 |
| Docling | 管线工具 | ~80–85 |
| Marker | 管线工具 | 78.44 |
| **docparse-rs**（论文子集，代理） | 管线工具 | **~75** |

诚实定位：**文本与公式已不是短板（各 ~0.87）；与 leaderboard 顶部（90+）的差距主要在难学术表 + 端到端管线打磨。** 我们把 OpenDoc-0.1B 的两半都内嵌为可选 `tract` 后端——UniRec（识别）+ 现在的 **PP-DocLayoutV2**（版面，与默认 DocLayout-YOLO 并存、opt-in）——但用自己的确定性核心拼接，而非跑 OpenDoc 的端到端管线;且 **born-digital 优先**——图像文档是补充域。（PP-DocLayoutV2 把杂版面端到端表识别提升约 3×,见 [版面后端 A/B](docs/testresults/2026-06-15-ppv2-vs-yolo-omnidocbench.md)。）完整方法与边界：[OmniDocBench 测评](docs/testresults/2026-06-12-omnidocbench.md)。

### 一致度记分牌（vs ODL / Docling，born-digital）

第二个互补参照：与同类系统输出在 born-digital 文档上的**一致度**——非人工真值。它跟踪确定性快路径，但与**压扁口径**表真值比，会反噬真实合并格结构（同一个 UniRec 表在这里读 0.526，在 OmniDocBench 的 span 真值下读 0.810）：

| 同台 | NID 阅读顺序 | MHS 标题 | TEDS_X |
|---|---|---|---|
| vs OpenDataLoader（15 份） | 0.792 | 0.687 | 0.477 |
| vs Docling（13 份） | 0.822 | 0.645 | 0.526 |

详见 [综合测评](docs/testresults/2026-06-10-benchmark-roundup.md)。

## 与同类产品对比

> 诚实口径：各家定位不同，下表按"agent/RAG 消费文档"的视角对齐维度；对方占优处照写。详细分析见 [docs/refer/docling-objective-comparison.md](docs/refer/docling-objective-comparison.md)。

| 维度 | **docparse-rs** | Docling | OpenDataLoader-PDF | MarkItDown |
|---|---|---|---|---|
| 实现/部署 | **纯 Rust 单二进制 ~26.5MB，零运行时依赖** | Python + 神经模型（GB 级环境，冷启动） | Java/JVM（veraPDF 系） | Python，轻量 |
| 确定性/可复现 | **默认路径同输入逐字节确定** | 神经管线非严格确定 | 确定 | 确定 |
| 引用定位 | **page+bbox 双向（chunk↔坐标 `locate`），引用率 100%** | 元素级 provenance | 元素坐标 | 无坐标（markdown-first） |
| 格式数 | 12 | **15+** | PDF 专注 | **20+** |
| 表结构（合并格） | 确定性四检测器 + **内嵌 0.1B 模型**（rowspan/colspan 入 IR，opt-in） | TableFormer（神经，内置） | 确定性（平铺网格） | 基础 |
| 公式→LaTeX | `--formula-model`（内嵌） | 有（模型） | — | — |
| OCR | 进程内 `tract`（PP-OCR），**数字页零模型零成本**；整页转写高质量档 | 多引擎集成（全页跑模型） | hybrid 模式外接 | 插件 |
| VLM/LLM 增强 | OpenAI 兼容外接（vLLM 等），任务级 opt-in | 内置 + serve 生态 | hybrid（docling 后端） | LLM 图片描述可选 |
| Agent 接口 | **CLI/库/MCP/REST 四面字节一致** + Python 客户端 + LangChain/LlamaIndex loader | SDK + 生态成熟 | CLI/Java/Python 包 | CLI/库 |
| born-digital 速度 | **<10ms 暖解析，~700 页/s** | 秒级/页 | 快 | 快 |
| 许可 | Apache-2.0（含模型文件） | MIT（个别模型许可另议） | Apache-2.0 | MIT |

**对方仍占优、我们不回避的**：Docling 的神经版面在最难版面上质量上限更高、格式广度与社区生态更成熟；MarkItDown 的长尾格式数更多；我方显式不做 GPU 管线，RTL 与韩文等多语种暂未覆盖（评测里如实计 0）。上表的"一致度记分牌"测的是与参照系统的一致度而非人工真值——口径与边界见[综合测评](docs/testresults/2026-06-10-benchmark-roundup.md)。

## 用法

```bash
cargo build --release
./target/release/docparse input.pdf -f json        # 完整 IR（带 provenance/坐标）
./target/release/docparse input.pdf -f markdown    # Markdown
./target/release/docparse input.pdf -f text -o out.txt
./target/release/docparse input.pdf -f chunks      # RAG 切块（page+bbox+标题面包屑）
./target/release/docparse scan.pdf --ocr           # 扫描件 OCR（需 models/ppocr，数字页零成本）
./target/release/docparse hard.pdf --layout        # 版面模型重排宏观读序（需 models/layout，opt-in；默认 DocLayout-YOLO）
./target/release/docparse hard.pdf --layout --layout-model models/layout-ppv2/PP-DoclayoutV2_simp.onnx   # PP-DocLayoutV2 后端（自动识别；杂版面表检测 ≈3× YOLO）
./target/release/docparse doc.pdf --vlm-describe --vlm-url http://127.0.0.1:8000 --vlm-model <vision-model>   # VLM 图片描述
./target/release/docparse doc.pdf --vlm-tables --vlm-url http://127.0.0.1:8000 --vlm-model <vision-model>     # VLM 重抽表结构（合并格/多级表头），失败保底确定性网格
./target/release/docparse doc.pdf --table-model models/unirec   # 内嵌 UniRec-0.1B 重抽表结构（合并格/多级表头），进程内无服务
./target/release/docparse doc.pdf --formula-model models/unirec # 公式→LaTeX（YOLO 找公式区 + UniRec 识别，需 models/layout）
./target/release/docparse doc.pdf --transcribe-model models/unirec # 整页转写（中英难版面/扫描件高质量档，区域级定位）
./target/release/docparse doc.pdf --image-dir imgs/   # 导出嵌入图片（JPEG/PNG），JSON 带 file、Markdown 带 ![]() 引用
./target/release/docparse doc.pdf --image-embed       # 图片以 base64 内嵌进 JSON（data_base64 + data_media_type）
./target/release/docparse input.pdf --quality --profile --route-plan   # 质量分/页级画像/路由计划（stderr JSON）

./target/release/docparse mcp                      # MCP stdio server（agent 直连）
./target/release/docparse serve --port 8642        # REST：POST /parse（multipart）+ GET /healthz
```

```bash
# Claude Code 接入：
claude mcp add docparse -- /path/to/docparse mcp
# 工具面：parse_document / get_chunks / locate——参数 ocr/layout/table_model/formula_model/vlm_*
#（服务启动时配模型：docparse mcp --unirec-models models/unirec [--vlm-url ...]）

# REST：
curl -F "file=@doc.pdf" "http://127.0.0.1:8642/parse?format=chunks&ocr=true&table_model=true"

# Python / LangChain（clients/python，零依赖薄客户端）：
#   from docparse_client.langchain import DocparseLoader
#   docs = DocparseLoader("paper.pdf").load()   # 每 chunk 一个 Document，metadata 带 page+bbox
```

可选模型文件（全部 Apache-2.0，外部分发，不进二进制）：

| 目录 | 模型 | 来源 | 驱动的功能 |
|---|---|---|---|
| `models/ppocr/`（~16MB） | PP-OCRv4 det+rec + 字典；可选 cls 方向分类（~0.6MB，缺失则禁用旋转校正） | PaddleOCR（HuggingFace `SWHL/RapidOCR` 转换件；cls 在其 `PP-OCRv1/ch_ppocr_mobile_v2.0_cls_infer.onnx`） | `--ocr` 扫描件文字 + 旋转扫描自动转正（0/90/180/270） |
| `models/layout/`（~75MB） | DocLayout-YOLO | [opendatalab/DocLayout-YOLO](https://github.com/opendatalab/DocLayout-YOLO)（DocStructBench） | `--layout` 版面区域（默认后端）、公式区检出 |
| `models/layout-ppv2/`（~210MB） | **PP-DocLayoutV2**（RT-DETR，25 类 + 原生阅读顺序） | [PaddleOCR](https://www.paddleocr.ai/) PP-DocLayoutV2；ONNX 经 [OpenOCR](https://github.com/Topdu/OpenOCR)（`topdu/PP_DoclayoutV2_onnx`）——**OpenDoc-0.1B** 的版面那半 | `--layout --layout-model …PP-DoclayoutV2_simp.onnx`：类别更丰富 + 原生读序，**杂版面端到端表检测 ≈3× YOLO**（自动识别）。一次性静态化：`scripts/spike/ppv2/prepare.py` |
| `models/unirec/`（~700MB） | **UniRec-0.1B**（统一文本/公式/表格识别） | [OpenOCR](https://github.com/Topdu/OpenOCR)（FVL Lab；[论文 arXiv 2512.21095](https://arxiv.org/abs/2512.21095)）——其 **OpenDoc-0.1B** 文档解析系统的识别器，官方 ONNX：`huggingface-cli download topdu/unirec_0_1b_onnx --local-dir models/unirec` | `--table-model` 合并格表结构 / `--formula-model` 公式→LaTeX / `--transcribe-model` 整页转写（中英） |

> UniRec 接入方式：我们用 `tract` 纯 Rust 运行其官方 encoder/decoder ONNX，自回归循环与 KV-cache 在 Rust 宿主侧驱动——OpenOCR 的 OpenDoc 管线本身是 Python/ONNX Runtime,我们复用其模型与 tokenizer 映射、独立实现推理与 HTML/LaTeX 结果解析（选型与 spike 实测见 [docs/refer/openocr-0.1b-evaluation.md](docs/refer/openocr-0.1b-evaluation.md)）。

```bash
cargo test          # 142 单测（CMap/矩阵/XY-cut/表格/切块/MCP/限额/OCR 解码/1-bit 与旋转/各格式后端…）
```

## 架构

Cargo workspace，十七个 crate：

| crate | 职责 | 关键依赖 |
|---|---|---|
| [`docparse-core`](crates/docparse-core) | 格式无关核心：版本化 IR + provenance、`DocumentParser` trait、XY-cut 阅读顺序、版面/段落/页眉页脚、表格四检测器、RAG 切块与 `locate` 反查、质量评分/画像与 `Enhancer` 外接边界、资源守卫、JSON/MD/Text 输出 | serde |
| [`docparse-pdf`](crates/docparse-pdf) | 纯 Rust PDF 后端：lopdf 解析 + **自研内容流解释器**（矩阵栈 + 操作符状态机 + 隐藏文本检测 + 图像 XObject 抽取）+ **字体层**（ToUnicode CMap/AFM/Encoding，参考 veraPDF 独立实现）+ rayon 逐页并行 | lopdf, rayon |
| [`docparse-docx`](crates/docparse-docx) | DOCX 后端：docx-rs 结构 → 合成坐标汇入同一 IR；含 zip-bomb 预检 | docx-rs |
| [`docparse-html`](crates/docparse-html) | HTML 后端：DOM 前序遍历 → 标题/段落/列表/表格 | scraper |
| `docparse-{xlsx,pptx,md,csv,srt,tex}` | 薄后端：XLSX（calamine）/ PPTX（每 slide 一页）/ Markdown / CSV（手写 RFC-4180 子集）/ SRT·WebVTT 字幕（每 cue 一段带时间戳）/ LaTeX 源码子集（章节/列表/tabular→表）/ EML 邮件（头部/正文/附件列举）/ PNG·JPEG 图片即文档（走 OCR 路由）/ AsciiDoc 子集——同一合成布局汇入 IR | calamine, quick-xml, pulldown-cmark, mail-parser, zune-png |
| [`docparse-ocr`](crates/docparse-ocr) | ONNX 内嵌 enhancer：OCR（PP-OCRv4 det+rec，DBNet 后处理/CTC 解码自研）+ 版面（DocLayout-YOLO 或 PP-DocLayoutV2 区域→阅读组，统一 `RegionKind`），均经 `tract` 纯 Rust 推理 | tract-onnx, zune-jpeg |
| [`docparse-raster`](crates/docparse-raster) | 难页按需渲染（纯 Rust `hayro`，~100ms/页）——主流程永不渲染；仅 enhancer 路由页 opt-in，含坏渲染守卫 | hayro |
| [`docparse-vlm`](crates/docparse-vlm) | VLM enhancer：OpenAI 兼容服务（vLLM/LM Studio 等）图片描述等任务，自带最小 PNG 编码器，服务失败优雅降级 | ureq, base64 |
| [`docparse-cli`](crates/docparse-cli) | `docparse` 命令行 + **MCP stdio server**（手写 JSON-RPC，零 SDK 依赖）+ **REST**（axum） | clap, axum, tokio |

**为什么这样分层**：`core` 不依赖任何 PDF 库——阅读顺序和输出对所有格式通用。新增格式只需实现 `DocumentParser` trait 并在 CLI 注册表里加一行；模型永不进核心，经 `Enhancer` 边界按页外接。

### 内容流解释器（项目的核心）

这是 opendataloader-pdf 委托给 veraPDF 的那一层，这里用 Rust 自己实现：lopdf 给出已解析的操作符列表，[`interpreter.rs`](crates/docparse-pdf/src/interpreter.rs) 维护图形/文本矩阵栈，走文本显示操作符发射带坐标的 chunk。**主流程不光栅化**（速度的来源）——OCR 只抽扫描页里**已有的**嵌入位图原字节；唯有难页请神经 enhancer 帮忙时，才用纯 Rust 渲染器按需画那一页（opt-in，默认关闭）。

已处理操作符：`q Q cm` · `BT ET` · `Tf TL Tc Tw Tz Tr Td TD Tm T*` · `Tj ' TJ` · 路径 `m l re c v y h S f B n`（表格线抽取）· `Do`（图像 XObject）。

### 字体层（参考 veraPDF 独立实现）

嵌入子集 CID 字体的 show 字符串是字形索引，不靠字体信息读不出文字。参考 veraPDF 独立实现：ToUnicode CMap（`bfchar`/`bfrange`、codespace 变长码切分）、标准 14 字体 AFM 度量、简单字体 Encoding/Differences + AGL、字形宽度（`Widths`/`W`/`DW`）。真实字形宽度让 x 坐标精确，输出层据此按几何间距还原单词边界。

## 文档地图

- [docs/roadmap.md](docs/roadmap.md) —— 战略：愿景、四条身份约束、十大模块、四大战场对标 Docling;
- [docs/plans/next-iteration.md](docs/plans/next-iteration.md) —— 近期里程碑 N1–N6（全部完成）与验收记录;
- [docs/testresults/](docs/testresults/) —— 记分牌与测评（[综合测评](docs/testresults/2026-06-10-benchmark-roundup.md) 入口）;
- [docs/devlogs/](docs/devlogs/) —— 每个里程碑的过程、决策与踩坑记录。

## 进度

- [x] **M1–M7**：文本保真（AFM/Encoding/CMap/字距）、IR 脊梁（版本化+provenance+质量分）、版面可读、有框表格、DOCX/HTML、RAG 切块+引用、质量路由+外接边界。
- [x] **N1 评测**：NID/TEDS/MHS 与 ODL/Docling 同台（上表）；差异化指标自动化（`scripts/metrics.sh`）。
- [x] **N2 服务化**：MCP stdio + REST，四接口逐字节一致。
- [x] **N3 真实 enhancer**：ONNX 内嵌 OCR（PP-OCRv4 × `tract` 纯 Rust）——`chinese_scan` 0 文本→14/14 行全对带 bbox 引用；MCP/REST 透传；数字页零模型。
- [x] **N4 大部**：表格四检测器（bordered→ruled→cluster→borderless）、标题分级、词距。
- [x] **N5 安全预检与画像**：隐藏文本过滤（Tr 3/7/页外/微字 → 标注+排除+可审计）、zip-bomb/页数资源守卫、页级复杂度画像（`--profile`）。
- [x] **Phase 4 · G2 基建**：版面 enhancer 全链路（按需渲染/区域检测/阅读组）落地 opt-in；其"修 CJK 序"假设实测否决（gap 在区域内微观序），CJK 改由 VLM 路线攻——诚实记录见 [devlog](docs/devlogs/2026-06-10-g2-layout-enhancer.md)。
- [x] **Phase 4 主体**（2026-06-11）：格式平齐 3→12（XLSX/PPTX/MD/CSV/SRT·VTT/LaTeX/EML/PNG·JPEG 图片即文档/AsciiDoc）、G9 结构层全部（Tagged PDF/列表/标题分级/表结构重建，TEDS 验收门过）、**内嵌表结构/公式模型**（`--table-model`/`--formula-model`，UniRec-0.1B×tract，进程内合并格语义与公式→LaTeX）、VLM 服务接入（`--vlm-describe/--vlm-tables`，OpenAI 兼容，可不接）、图片导出/内嵌（`--image-dir`/`--image-embed`）、MCP/REST 全增强透传、Python 客户端 + LangChain/LlamaIndex loader（五行验收实测）、压测+fuzz（1847 输入 + ~1020 万次执行零 panic）、IR 0.7.0（Cell span 语义）。见 [迭代计划](docs/plans/closing-docling-gaps.md)。
- [x] **Phase 5（健壮性纵深，2026-06-12）**：CCITT G3/G4 + JBIG2 扫描解码（传真类扫描件可 OCR）、旋转校正（`/Rotate` 烘入基 CTM + PP-OCR cls，0/90/180/270 自动转正）、双栏左列重排（列感知 `fill_x`，单栏字节级零回归）、精确树编辑距离 TEDS、HTML `<meta charset>` + CMYK-JPEG 处理。**两个负结论，均做到能下定论**：确定性 `--layout` 自动路由关闭（第 4 个几何判据仍不可分）、同色隐藏文本检测确证为**架构边界**——建了完整背景模型仍无法在不渲染下零误杀，与"不光栅化"身份冲突。见 [hardening-iteration.md](docs/plans/hardening-iteration.md)。

## 许可

Apache-2.0。本项目为独立实现，不包含 veraPDF 代码（veraPDF 为 GPLv3+/MPLv2，仅参考其算法并在源码注明出处）。外部模型文件均为 Apache-2.0：PP-OCR（PaddleOCR）、DocLayout-YOLO（opendatalab）、PP-DocLayoutV2（PaddleOCR；ONNX 经 OpenOCR）、UniRec-0.1B（[OpenOCR](https://github.com/Topdu/OpenOCR)/FVL Lab——感谢其开源 OpenDoc-0.1B 文档解析系统与官方 ONNX 导出）。构建携带两处最小、已注明出处的 [tract 补丁](vendor/PATCHES.md)（按决定长期 vendored 留 main，见 [vendor/README.md](vendor/README.md)）以在 `tract` 上运行 PP-DocLayoutV2（RT-DETR）。
