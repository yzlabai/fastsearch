# docparse-rs

**中文** | [English](README.en.md)

高效、纯 Rust 的**多格式文档解析系统**：从 PDF/DOCX/HTML 抽取**带位置的结构化内容**（文本/版面/阅读顺序/表格 → 统一 IR → JSON / Markdown / Text / RAG chunks），走"结构提取"而非"光栅渲染"的快路径。面向 Agent / RAG：结果**确定、可复现、可引用**（每个 chunk 带 page+bbox 双向溯源）。

> 设计动机来自对 [opendataloader-pdf](https://github.com/opendataloader-project/opendataloader-pdf) 的架构分析：它快，是因为默认从不把页面渲染成像素，只解析内容流拿坐标，再逐页并行做版面分析。docparse-rs 用纯 Rust 复刻并延伸这条快路径——无 JVM、无 C++、无 Python，单二进制。

## 亮点

- **单二进制 19.1MB、运行时依赖 0**：预热解析 <10ms、700 页/s、同输入逐字节确定;
- **四接口一份输出**：CLI / 库 / MCP（stdio，agent 直连）/ REST，跨接口**逐字节一致**（含 OCR 路径）;
- **RAG 一等公民**：结构化切块带 page+bbox+标题面包屑，`locate(x,y)` 坐标反查 chunk，引用可定位率 100%;
- **安全预检内置**：隐藏文本过滤（防 prompt injection，标注可审计而非静默删除）、zip-bomb/页数资源守卫、页级复杂度画像;
- **扫描件 OCR 不破纯 Rust 身份**：`--ocr` 走进程内 `tract` ONNX 推理（PP-OCRv4，中文事实标准模型，~16MB 外部模型文件），抽嵌入图原字节而非渲染;按页路由,数字页**零模型零成本**;
- **可插拔 AI 边界**：确定性主流程独立成立,模型只在质量评分判定难例时按页触发,产出带 `source` 标签与降级置信度。

## 当前状态与记分牌

十大功能模块全部闭合（IR/PDF/版面/语义/多格式/输出 RAG/质量路由/AI 外接/安全/服务化）。

**质量记分牌**（2026-06-10，born-digital LTR，与参照系统的**一致度**，非人工真值）：

| 同台 | NID 阅读顺序 | MHS 标题 | TEDS 表格 |
|---|---|---|---|
| vs OpenDataLoader（确定性同类，15 份） | **0.780** | **0.680** | 0.128 |
| vs Docling（神经管线，13 份） | **0.818** | **0.634** | 0.157 |

clean 文档 0.94–1.00（与两者结构同构）；聚合被 CJK 复杂版面与表结构精度（神经域）拖低——逐轴对比、口径与边界详见 [综合测评](docs/testresults/2026-06-10-benchmark-roundup.md)。

## 用法

```bash
cargo build --release
./target/release/docparse input.pdf -f json        # 完整 IR（带 provenance/坐标）
./target/release/docparse input.pdf -f markdown    # Markdown
./target/release/docparse input.pdf -f text -o out.txt
./target/release/docparse input.pdf -f chunks      # RAG 切块（page+bbox+标题面包屑）
./target/release/docparse scan.pdf --ocr           # 扫描件 OCR（需 models/ppocr，数字页零成本）
./target/release/docparse hard.pdf --layout        # 版面模型重排宏观读序（需 models/layout，opt-in）
./target/release/docparse doc.pdf --vlm-describe --vlm-url http://127.0.0.1:11434 --vlm-model qwen2.5vl   # VLM 图片描述
./target/release/docparse input.pdf --quality --profile --route-plan   # 质量分/页级画像/路由计划（stderr JSON）

./target/release/docparse mcp                      # MCP stdio server（agent 直连）
./target/release/docparse serve --port 8642        # REST：POST /parse（multipart）+ GET /healthz
```

```bash
# Claude Code 接入：
claude mcp add docparse -- /path/to/docparse mcp
# 工具面：parse_document(path, format, ocr) / get_chunks(path, ocr) / locate(path, page, x, y)

# REST：
curl -F "file=@doc.pdf" "http://127.0.0.1:8642/parse?format=chunks&ocr=true"
```

OCR 模型（可选，三个文件 ~16MB，Apache-2.0）放 `models/ppocr/`：`ch_PP-OCRv4_det_infer.onnx` + `ch_PP-OCRv4_rec_infer.onnx`（HuggingFace `SWHL/RapidOCR`）+ `ppocr_keys_v1.txt`（PaddleOCR 仓库）。

```bash
cargo test          # 82 单测（CMap/矩阵/XY-cut/表格/切块/MCP/限额/OCR 解码…）
```

## 架构

Cargo workspace，八个 crate：

| crate | 职责 | 关键依赖 |
|---|---|---|
| [`docparse-core`](crates/docparse-core) | 格式无关核心：版本化 IR + provenance、`DocumentParser` trait、XY-cut 阅读顺序、版面/段落/页眉页脚、表格四检测器、RAG 切块与 `locate` 反查、质量评分/画像与 `Enhancer` 外接边界、资源守卫、JSON/MD/Text 输出 | serde |
| [`docparse-pdf`](crates/docparse-pdf) | 纯 Rust PDF 后端：lopdf 解析 + **自研内容流解释器**（矩阵栈 + 操作符状态机 + 隐藏文本检测 + 图像 XObject 抽取）+ **字体层**（ToUnicode CMap/AFM/Encoding，参考 veraPDF 独立实现）+ rayon 逐页并行 | lopdf, rayon |
| [`docparse-docx`](crates/docparse-docx) | DOCX 后端：docx-rs 结构 → 合成坐标汇入同一 IR；含 zip-bomb 预检 | docx-rs |
| [`docparse-html`](crates/docparse-html) | HTML 后端：DOM 前序遍历 → 标题/段落/列表/表格 | scraper |
| [`docparse-ocr`](crates/docparse-ocr) | ONNX 内嵌 enhancer：OCR（PP-OCRv4 det+rec，DBNet 后处理/CTC 解码自研）+ 版面（DocLayout-YOLO 区域→阅读组），均经 `tract` 纯 Rust 推理 | tract-onnx, zune-jpeg |
| [`docparse-raster`](crates/docparse-raster) | 难页按需渲染（纯 Rust `hayro`，~100ms/页）——主流程永不渲染；仅 enhancer 路由页 opt-in，含坏渲染守卫 | hayro |
| [`docparse-vlm`](crates/docparse-vlm) | VLM enhancer：OpenAI 兼容服务（vLLM/Ollama/LM Studio）图片描述等任务，自带最小 PNG 编码器，服务失败优雅降级 | ureq, base64 |
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
- [ ] **Phase 4（其余）**：补齐 Docling 占优轴——版面/表结构 ONNX enhancer（难页路由）、格式平齐（XLSX/PPTX/邮件/字幕/图片即文档/LaTeX 等）、区域级 OCR/Form 流、语义增强面（代码块/公式/图片/图表/整页 VLM——VLM 经 OpenAI 兼容服务接入：vLLM/Ollama 等）、LangChain/LlamaIndex 接入、千份语料压测。见 [迭代计划](docs/plans/closing-docling-gaps.md)。

## 许可

Apache-2.0。本项目为独立实现，不包含 veraPDF 代码（veraPDF 为 GPLv3+/MPLv2，仅参考其算法并在源码注明出处）。OCR 模型（PP-OCR）为 Apache-2.0，作为外部文件分发。
