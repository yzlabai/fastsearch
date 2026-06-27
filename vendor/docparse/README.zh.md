<div align="center">

# 📄 docparse-rs

**快、纯 Rust、面向 Agent 与 RAG 的多格式文档解析器。**

从 12+ 种格式抽取带位置的结构化内容——每个切块都带 page + bbox，可引用、可反查。

[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
![Rust](https://img.shields.io/badge/built%20with-pure%20Rust-orange?logo=rust)
![Single binary](https://img.shields.io/badge/deploy-single%20binary%20~29MB-brightgreen)
![Platforms](https://img.shields.io/badge/platforms-macOS%20%7C%20Linux%20%7C%20Windows-lightgrey)
![Tests](https://img.shields.io/badge/tests-142%20passing-success)

[English](README.md) | 中文

</div>

---

docparse-rs 把 **PDF · DOCX · HTML · XLSX · PPTX · Markdown · CSV · SRT/VTT · LaTeX · EML · PNG/JPEG · AsciiDoc** 解析为统一中间表示（IR），再输出 **JSON / Markdown / Text / RAG 切块**。它走"结构提取"快路径——解析 PDF 内容流拿坐标，而非把页面渲染成像素——所以暖解析 **<10ms（~700 页/s）**，同输入逐字节确定。单个 ~29 MB 二进制，无 JVM / C++ / Python，零运行时依赖。

## 🎬 演示

<video src="https://github.com/yzlabai/docparse-rs/raw/main/docs/assets/fastdemo.mp4" controls width="100%"></video>

> ▶️ 播放器若不加载，[观看 / 下载 `fastdemo.mp4`](docs/assets/fastdemo.mp4)。

## ✨ 特性

- 🦀 **单个纯 Rust 二进制** —— ~29 MB，零运行时依赖，暖解析 <10ms（~700 页/s）
- 🔌 **四接口一份输出** —— CLI / 库 / MCP（stdio）/ REST，**跨接口逐字节一致**
- 📍 **RAG 原生引用** —— 每个切块带 page + bbox + 标题面包屑；`locate(x, y)` 坐标反查，定位率 100%
- 🔍 **进程内 OCR** —— `--ocr` 走 `tract` ONNX（PP-OCRv4）；数字页零模型零成本；覆盖 CCITT G3/G4 传真 + JBIG2 扫描
- 🧠 **内嵌模型，opt-in** —— 合并格表结构、公式→LaTeX、整页转写（UniRec-0.1B），外加 PP-DocLayoutV2 / DocLayout-YOLO 版面
- 🛡️ **安全预检** —— 隐藏文本过滤（标注可审计，绝不静默删除）、zip-bomb / 页数守卫、页级复杂度画像
- 🧩 **可插拔 AI 边界** —— 确定性核心独立成立；模型只在难页触发，产出带 `source` 标签与降级置信度

## 📥 安装

**预编译二进制** —— 免工具链，macOS · Linux · Windows（见 [Releases](https://github.com/yzlabai/docparse-rs/releases)）：

```bash
# macOS / Linux
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/yzlabai/docparse-rs/releases/latest/download/docparse-cli-installer.sh | sh
```

```powershell
# Windows (PowerShell)
powershell -c "irm https://github.com/yzlabai/docparse-rs/releases/latest/download/docparse-cli-installer.ps1 | iex"
```

**用 Cargo**（从源码构建，自动应用 vendored `tract` 补丁）：

```bash
cargo install --git https://github.com/yzlabai/docparse-rs docparse-cli
```

**本地构建**（开发）：

```bash
git clone https://github.com/yzlabai/docparse-rs && cd docparse-rs
cargo build --release   # → ./target/release/docparse
```

三种方式都得到 `docparse` 二进制。核心不需要任何模型，可选档按需下载（见 [可选模型](#-可选模型)）。

## 🚀 快速开始

```bash
docparse input.pdf -f json       # 完整 IR：provenance + 坐标
docparse input.pdf -f markdown   # Markdown
docparse input.pdf -f chunks     # RAG 切块（page + bbox + 面包屑）
docparse scan.pdf  --ocr         # 扫描件 OCR（数字页零成本；首次用会提示拉取 models/ppocr-v6）
```

<details>
<summary><b>更多命令 —— 版面 · 表格 · 公式 · VLM</b></summary>

```bash
docparse hard.pdf --layout                                   # 版面模型重排读序（DocLayout-YOLO；需 models/layout）
docparse hard.pdf --layout --layout-model models/layout-ppv2/PP-DoclayoutV2_simp.onnx   # PP-DocLayoutV2 后端（杂版面表检测 ≈3× YOLO）
docparse doc.pdf  --table-model models/unirec                # 合并格表结构（进程内，无服务）
docparse doc.pdf  --formula-model models/unirec              # 公式 → LaTeX
docparse doc.pdf  --transcribe-model models/unirec           # 整页转写（中英难版面 / 扫描件）
docparse doc.pdf  --vlm-describe --vlm-url URL --vlm-model M # 经 OpenAI 兼容 VLM 做图片描述
docparse doc.pdf  --vlm-tables   --vlm-url URL --vlm-model M # VLM 重抽表结构（失败保底确定性网格）
docparse doc.pdf  --image-dir imgs/                          # 导出嵌入图片（JSON "file" / Markdown ![]()）
docparse input.pdf --quality --profile --route-plan          # 质量分 / 页级画像 / 路由计划（stderr JSON）
```
</details>

### 接入 Agent

```bash
claude mcp add docparse -- docparse mcp     # MCP 工具：parse_document / get_chunks / locate
docparse serve --port 8642                                  # REST：POST /parse（multipart）+ GET /healthz
curl -F "file=@doc.pdf" "http://127.0.0.1:8642/parse?format=chunks&ocr=true"
```

```python
# Python / LangChain（clients/python —— 零依赖薄客户端）
from docparse_client.langchain import DocparseLoader
docs = DocparseLoader("paper.pdf").load()   # 每 chunk 一个 Document，metadata 带 page + bbox
```

**Agent Skill** —— 一个 [SKILL.md](skills/docparse-document-intelligence/SKILL.md) 技能包，教编码 agent（Claude Code / Cursor）按症状驱动 `docparse` CLI：格式选择、OCR/表/公式决策矩阵、以及"解析 → 自检（`--quality`/`--profile`）→ 迭代"循环。软链到 agent 查找技能的目录即可：

```bash
mkdir -p .claude/skills
ln -s "$(pwd)/skills/docparse-document-intelligence" .claude/skills/   # 或 ~/.claude/skills（全局）、~/.cursor/skills（Cursor）
```

## 📊 质量

在 **[OmniDocBench](https://github.com/opendatalab/OmniDocBench)**（CVPR 2025）上对**人工真值**打分，使用内嵌 UniRec 模型：

| 维度 | 路径 | 分数 |
|---|---|---|
| 文本识别 | `--transcribe-model`，论文 | **0.872** |
| 公式 → LaTeX | `--formula-model`，论文 | **0.874** |
| 表结构 | `--table-model`，clean 表 | **0.810**（median 0.895） |

**文本与公式已接近论文级（~0.87）。** 剩下的缺口是难学术表（多级表头 + 密集数字 + 含 LaTeX）。代理口径的 "Overall" ≈ 75，落在管线工具档（Marker 78、Docling ~80–85；专用 VLM 90+）——[记分牌与方法 →](docs/status.md)。

## 🆚 与同类对比

| | **docparse-rs** | [liteparse](https://github.com/run-llama/liteparse) | Docling | OpenDataLoader | MarkItDown |
|---|---|---|---|---|---|
| 部署 | **纯 Rust ~29 MB 二进制，零运行时依赖** | Rust + PDFium/Tesseract（C++）；非 PDF 走 LibreOffice/ImageMagick | Python + 模型（GB 级环境） | Java / JVM | Python |
| PDF 引擎 | **自研内容流解释器** | 包 PDFium | 自研 | veraPDF | （委托） |
| 确定性 | **默认路径逐字节确定** | 确定 | 非严格 | 确定 | 确定 |
| 引用 | **page+bbox 双向，100%** | 每文本元素带 bbox | 元素级 | 坐标 | 无 |
| 输出 | JSON / **Markdown** / text / **RAG chunks** | JSON / text / PNG | Markdown / JSON | JSON / Markdown | Markdown |
| 格式数 | **12，全部进程内** | PDF 原生；其余靠外部转换 | 15+ | PDF 专注 | 20+ |
| 难页 | 可选内嵌模型（表/公式/CJK） | 无（设计如此） | 神经版面 | 规则 | 无 |
| 速度（born-digital） | **<10ms / ~700 页/s** | 快 | 秒级/页 | 快 | 快 |

**最近的同类——liteparse**（run-llama，同为 Rust + 确定性 + bbox 优先）：设计哲学高度重合，取舍不同。liteparse 用 **PDFium** 抽 PDF 文本、内置 **Tesseract**、并经 **LibreOffice + ImageMagick** 转换 DOCX/XLSX/PPTX/图片——因此带原生 C++ 依赖与外部工具；docparse-rs 则是单个零依赖二进制，自研 PDF 解释器 + 12 种格式全进程内解析。**liteparse 占优在"触达面"**：WASM/浏览器构建、一流的 Node/Python 绑定、`npm`/`pip`/`cargo install`、以及 Tesseract 开箱即用的多语种 OCR（docparse-rs 的 OCR 聚焦中英）。**docparse-rs 多出**：带标题面包屑的 Markdown + RAG chunks 输出与双向 `locate()`，以及 liteparse 为保持轻量而刻意不做的可选内嵌模型（合并格表、公式→LaTeX、CJK/整页转写）。

对方仍占优处：Docling 的神经版面在最难版面上质量上限更高、生态更成熟；MarkItDown 长尾格式更多；我方不做 GPU 管线，非中英 OCR（RTL / 韩文…）暂未覆盖。[详细对比 →](docs/refer/docling-objective-comparison.md)

## 🏗️ 架构

Cargo workspace，**17 个 crate**。核心不变量：**`core` 不依赖任何 PDF 库**——阅读顺序与输出对所有格式通用，所以新增格式只需实现 `DocumentParser` trait 并在注册表加一行。

项目的核心是自研的 **PDF 内容流解释器**（图形/文本矩阵状态机，发射带坐标的切块——ODL 委托给 veraPDF 的那一层）与**字体层**（ToUnicode CMap / AFM / Encoding，参考 veraPDF *算法*独立实现）。模型永不进核心——经 `Enhancer` 边界按页外接，且只有被路由到模型的难页才会按需渲染（纯 Rust）。详见 [crates](crates/) 与 [roadmap →](docs/roadmap.md)。

## 📦 可选模型

全部 Apache-2.0，从各自原始仓库拉取为外部文件，不进二进制。核心**一个都不需要**：数字版 PDF 与其他所有格式零下载即可解析。按功能档位下载：

```bash
./scripts/fetch-models.sh ocr        # --ocr               (~16 MB)
./scripts/fetch-models.sh layout     # --layout（默认）     (~75 MB)
./scripts/fetch-models.sh unirec     # --table/formula/transcribe-model (~700 MB)
./scripts/fetch-models.sh ppv2       # --layout-model ppv2 (~210 MB + 一步本地预处理)
./scripts/fetch-models.sh all
```

需 HuggingFace CLI（`pip install -U huggingface_hub`）；`ppv2` 另需 `onnx`+`onnxsim` 把图静态化给 `tract`（脚本会打印该命令）。

| 档位 | 模型（来源） | 驱动功能 |
|---|---|---|
| `ocr` → `models/ppocr/`（~16 MB） | PP-OCRv4 det+rec+cls（`SWHL/RapidOCR`） | `--ocr` 扫描件文字、自动转正 |
| `layout` → `models/layout/`（~75 MB） | DocLayout-YOLO（`wybxc/DocLayout-YOLO-DocStructBench-onnx`） | `--layout` 版面区域（默认）、公式检出 |
| `ppv2` → `models/layout-ppv2/`（~210 MB） | PP-DocLayoutV2（`topdu/PP_DoclayoutV2_onnx`） | 更丰富版面 + 原生读序（杂版面表 ≈3× YOLO） |
| `unirec` → `models/unirec/`（~700 MB） | UniRec-0.1B（`topdu/unirec_0_1b_onnx`） | `--table-model` / `--formula-model` / `--transcribe-model` |

> UniRec 与 PP-DocLayoutV2 是 [OpenOCR](https://github.com/Topdu/OpenOCR) **OpenDoc-0.1B** 的两半；我们用纯 Rust `tract` 运行其官方 ONNX，再用自己的确定性核心拼接。[选型理由 →](docs/refer/openocr-0.1b-evaluation.md)

## 📄 许可

**Apache-2.0** —— 独立实现，不含 veraPDF 代码（veraPDF 为 GPLv3+/MPLv2，仅参考其算法并在源码注明出处）。外部模型文件均为 Apache-2.0。构建携带两处最小、已注明出处的 [tract 补丁](vendor/PATCHES.md)（[按决定长期 vendored 留 main](vendor/README.md)）以在 `tract` 上运行 PP-DocLayoutV2。
