# 在 Agent 系统中使用 docparse-rs

> 面向把 docparse-rs 接入 **Agent / RAG / 自动化流水线** 的集成方。讲清楚：有哪些接入口、输入输出长什么样、坐标与引用怎么用、增强能力怎么按需开、典型接入模式。
>
> 设计身份（详见 [roadmap.md](roadmap.md)）：**纯 Rust 单二进制、零运行时依赖、确定性可复现、带坐标可引用**；模型（OCR/版面/表/公式/VLM）是**可选增强**，默认全关，数字文档不碰模型。
>
> 相关：系统架构 [architecture.md](architecture.md)；能力清单 [capabilities.md](capabilities.md)；**何时开哪种增强**（质量信号→决策矩阵）见 [agent-enhancement-decisions.md](agent-enhancement-decisions.md)。

## 1. 四种接入口（同一份解析、字节一致）

| 接入口 | 何时用 | 启动 |
|---|---|---|
| **CLI** | 批处理、脚本、子进程包装 | `docparse <file> -f <fmt>` |
| **库（Rust crate）** | 同进程嵌入 Rust 服务 | 依赖 `docparse-core` + 各后端 crate |
| **MCP（stdio）** | Agent 直接工具调用（Claude/兼容 MCP 的运行时） | `docparse mcp` |
| **REST（axum）** | 语言无关的服务化、内网/容器 | `docparse serve --port 8642`（绑 127.0.0.1） |
| **Agent Skill** | 给编码 agent（Claude Code/Cursor…）一份"怎么用 CLI"的结构化技能：格式选择、OCR/表/公式增强决策矩阵、`--quality` 自检循环 | 包在 [skills/docparse-document-intelligence/](../skills/docparse-document-intelligence/SKILL.md)（软链到 `.claude/skills/` 即生效） |

> **不变量**：同一输入 + 同一格式，CLI / MCP / REST 输出**逐字节一致**。任选其一不影响结果。
>
> Agent Skill 不是第五种解析路径——它只是**包装 CLI** 的使用说明（Bash 调 `docparse`），让 agent 自己按症状选增强档、用内置 `--quality`/`--profile`/`--route-plan` 自检并迭代。

## 2. 输出格式（五选一）

`json` | `markdown` | `text` | `chunks` | `outline`

- **json** — 完整 IR：页 → 元素（文本/表/图，带 bbox、字号、tag、span、source）。要最全的结构信息用它。
- **markdown** — 人读 / LLM 友好的线性化（标题层级、表格、列表、代码围栏、图片引用）。
- **text** — 纯文本，按阅读顺序。
- **chunks** — **RAG 首选**：检索切块，每块带来源页 + bbox + 标题面包屑 + `section_id`。
- **outline** — **文档结构树**：嵌套 section（`title`/`level`/`page`/`bbox`），供 agentic 导航长文档（翻目录 → 钻章节）。section id 与 chunks 的 `section_id` 对齐，可"先 outline 拿目录，再取某节的 chunks"。
- **okf** — **Open Knowledge Format bundle**（写**目录**，非 stdout）：结构树 → 一 section 一 Markdown+YAML-frontmatter "concept" 文件，目录嵌套镜像树，厂商中立、git 原生、每 concept `resource` 带 page+bbox 可溯源。`-o <dir>` 或自动派生 `<stem>-okf/`（`--force` 覆盖、`--okf-resource-base <uri>` 前缀）。RAG 知识库可直接 `git add`，OKF-aware agent 零适配 mount。确定性（同源 mtime → 字节一致）。

### chunk schema（`chunks` 格式 / `get_chunks` 工具）

```jsonc
{
  "id": 0,
  "kind": "paragraph",          // heading | paragraph | table | code | list_item | image
  "text": "……",
  "page": 1,                     // 1-based
  "bbox": { "x0": 72.0, "y0": 690.1, "x1": 523.4, "y1": 705.8 },
  "heading_path": ["3 Methods", "3.1 Setup"],  // 上级标题面包屑，做引用/层级过滤
  "section_id": 12,                             // 所属结构树 section（对齐 outline / `outline` 工具）
  "image": {                                    // 仅 kind=image：渲染/溯源载荷
    "file": "imgs/p2-1.jpg",                    //   --image-dir 导出路径（或 data_base64 + media_type）
    "caption": "Figure 2. Overview…",           //   图说（也已折进 text）
    "caption_source": "caption-line"            //   caption-line | layout-caption | alt | vlm:<model>
  }
}
```

- **坐标系**：PDF 用户空间——原点左下、y 向上、单位 pt。无真实坐标的格式（DOCX/HTML/MD…）用合成布局折算到同一约定。
- **引用**：`page` + `bbox` 可直接回指原文位置；`heading_path` 给检索块层级语境；`section_id` 把块挂回结构树（parent-document / auto-merging 检索）。`heading_path` 由真实标题层级（tagged H1–H6 / 字号档位）建树后导出，非字号近似。
- **图片块**（`kind=image`，PDF + DOCX + PPTX + HTML 的整版图）：`text` = 图说 ⊕ 周边上下文（可检索字段），`image` 载荷给渲染/引用。图说就近绑定文档内 "Figure N" 行（`caption-line`）、版面/tagged `Caption` 区域（`layout-caption`，需 `--layout`）或 HTML `alt`（`alt`）——均零模型；`--vlm-describe` 升级为神经描述（`vlm:<model>`）。要拿到 `image.file`/`data_base64` 需加 `--image-dir`/`--image-embed`。HTML 解析 `data:` URI 与文件相对路径 `<img>`，远程 URL 不抓取。
- json 格式里被模型替换的元素带 `source`（如 `table:unirec-0.1b`、`formula:unirec-0.1b`、`vlm:<model>`、`layout:<model>`）——溯源可见，确定性结果仍独立成立。

## 3. MCP 工具（`docparse mcp`）

stdio 上的 JSON-RPC 2.0（协议 `2025-06-18`），五个工具：

| 工具 | 作用 | 必填参数 |
|---|---|---|
| `parse_document` | 解析为 `json`/`markdown`/`text` | `path`（+ 可选 `format`、增强开关） |
| `get_chunks` | 解析为检索 chunks（带 page+bbox+heading_path+section_id） | `path`（+ 可选增强开关） |
| `outline` | **文档结构树**：导航长文档——列目录 + 钻取某节 | `path`（+ 可选 `id` 取子树、`max_depth` 限深、增强开关） |
| `export_okf` | **OKF bundle**：解析为结构树镜像的 concept 文件集，返回 `{okf_version, files:[{path,content}]}`（agent 直写/直读，git 原生交付） | `path`（+ 可选 `resource_base`、增强开关） |
| `locate` | **反向引用**：给页号 + 点 (x,y)，返回覆盖该点的 chunk（无则 null） | `path`、`page`、`x`、`y` |

增强开关（布尔，默认 false）：`ocr`、`layout`、`table_model`、`formula_model`、`vlm_describe`、`vlm_tables`。**它们需要服务端启动时配好对应模型**（见 §5），否则缺失即跳过、不报错。

启动示例（开放表/公式/版面增强）：

```bash
docparse mcp \
  --layout-model models/layout/doclayout_yolo.onnx \
  --unirec-models models/unirec
```

Claude Code / 兼容运行时把它登记为 stdio MCP server 即可调用上面五个工具。

**导航式检索示例**（agent 翻书）：`outline {path, max_depth:1}` 拿顶层目录 → `outline {path, id:12}` 钻取第 12 节子树 → `get_chunks` 后按 `section_id==12` 取该节切块。

**自描述（无需外部文档即可上手）**：
- **`outputSchema` + `structuredContent`**：`get_chunks`/`outline`/`export_okf`/`locate` 都声明输出 JSON Schema，调用结果除文本块外还带 `structuredContent`（与文本同源的结构化 JSON），client 可直接按 schema 校验/取类型。
- **`resources/*`**：列举并读取①六个输出 schema（`docparse://schema/<name>.json`）②两份使用指南（`docparse://guide/agent-integration.md`、`docparse://guide/enhancement-decisions.md`——后者是"看 `quality.flags` → 该开哪个增强"的决策矩阵）。
- **`prompts/*`**：两个现成模板 `parse-for-rag`（带质量自检循环）、`navigate-document`（按结构钻取），传 `path` 即用。

## 4. REST（`docparse serve`）

```bash
docparse serve --port 8642            # 绑 127.0.0.1
```

- `GET /healthz` — 存活探针（回 `name`/`version`/`schema_version`）。
- `GET /openapi.json` — OpenAPI 3.1 自描述文档，`components.schemas` 内嵌全部输出 schema；外部项目据此 codegen 客户端。
- `GET /schema/{name}` — 单个输出 JSON Schema（`document`/`chunk`/`outline`/`quality`/`profile`/`okf-bundle`），draft 2020-12。
- `POST /parse?format=json|markdown|text|chunks|outline|okf` — **multipart** 上传文件字段，返回对应格式（`outline` = 文档结构树，section id 对齐 chunks 的 `section_id`；`okf` = 确定性 OKF tar bundle，`application/x-tar`，可加 `?resource_base=<uri>`）。
  增强用查询参数：`?ocr=true&layout=true&table_model=true&formula_model=true&vlm_describe=true&vlm_tables=true`（同样需启动时配模型，见 §5）。
- `format=chunks` 可加 `?envelope=true`：把裸 chunk 数组包成 `{provenance, quality, profile, chunks}`（同 MCP `get_chunks`）。RAG 消费方可据 `quality.flags`（`ScannedNoText` / `HighGarble` 等）和 `profile` 自行决定要不要对该文档开 OCR/layout，**省一次往返**。默认（不加）仍是裸数组，与 CLI 逐字节一致。
- `format=chunks` 可加 `?table_format=markdown`：表格 chunk 文本出 GitHub 管道表（默认 `tab`=制表符/换行）。CLI 同名 `--table-format markdown`、MCP `get_chunks` 同名 `table_format` 参数 —— 三面同默认、同输出（不变量保持）。

```bash
curl -s -F "file=@paper.pdf" \
  "http://127.0.0.1:8642/parse?format=chunks" | jq '.[0]'
# 带质量信封（决定是否开 OCR）：
curl -s -F "file=@scan.pdf" \
  "http://127.0.0.1:8642/parse?format=chunks&envelope=true" | jq '.quality.flags'
```

> OCR 等模型是**首请求懒加载**：只服务数字文档时进程零模型、冷启动 <100ms。

## 5. 增强能力（opt-in，默认全关）

数字文档走确定性快路径，**不碰任何模型**。难例按需开下列增强（CLI flag / MCP 参数 / REST 查询参数同名）；每项需对应模型文件或服务：

| 能力 | 开关 | 需要 | 说明 |
|---|---|---|---|
| 扫描件 OCR | `--ocr` | `models/ppocr-v6`（PP-OCRv6 tiny，~7MB，缺时 TTY 下确认自动下载；`--ocr-models models/ppocr` 回退 v4） | 数字页零模型；扫描图解码覆盖 DCT-JPEG、JPX(JPEG 2000)、CCITT G3/G4、JBIG2、16/8/4/2-bit 灰度与 1-bit(含 2-entry 调色板);四方向旋转校正。CMYK/带 alpha 等色彩歧义图仍只记位置 |
| 版面重排 | `--layout`（默认 YOLO）；`--layout-model …PP-DoclayoutV2_simp.onnx` 切 PP-DocLayoutV2 | DocLayout-YOLO / PP-DocLayoutV2 ONNX（按输入数自动识别） | 难版面（设计稿/CJK）按版面模型重排读序；PPV2 类别更丰富 + 原生阅读顺序，**杂版面端到端表识别 ≈3× YOLO** |
| 表结构重抽 | `--table-model DIR` | UniRec-0.1B（`models/unirec`） | 多级表头/合并单元格，进程内、无需服务 |
| 公式→LaTeX | `--formula-model DIR` | UniRec + 版面模型 | display 公式转 LaTeX |
| 整页转写 | `--transcribe-model DIR` | UniRec | 设计/CJK 版面整页重识别（中英域内强；行级定位降为区域级） |
| 图片描述 | `--vlm-describe` | `--vlm-url --vlm-model` | OpenAI 兼容服务（vLLM/LM Studio/云） |
| 表→VLM 重抽 | `--vlm-tables` | `--vlm-url --vlm-model` | 难表交 VLM；失败保底确定性网格 |

> 选型与边界：扫描中英用 `--ocr`（轻量、带行级 bbox）或 `--transcribe-model`（质量更高、区域级定位）；难表先试内嵌 `--table-model`，更难或多语种再走 `--vlm-tables` 接服务。**学术难表是当前模型天花板**（见 [status.md](status.md)）。

## 6. 典型接入模式

### A. RAG 摄取（最常见）
`get_chunks` / `-f chunks` → 每块 `text` 送嵌入，`page`/`bbox`/`heading_path`/`kind` 存为 metadata。检索命中后用 `page+bbox` 渲染高亮引用，用 `heading_path` 做章节过滤。数字文档零模型、确定可复现。

### B. 引用回链
答案里带页码坐标 → `locate(path, page, x, y)` 反查 chunk，校验"这句话确实出自此处"。

### C. Agent 工具直连
MCP 登记 `docparse mcp`；agent 按文档难度决定是否在调用里加 `table_model:true` 等开关。简单文档默认快路径，难页才升级。

### D. 流水线 / 批处理
CLI 子进程或 REST 批量；`--profile`（页级复杂度画像）/ `--report`（覆盖率/乱码/flags，输出到 stderr）可用于路由判断"这份要不要开增强"。

### E. Python 集成
`clients/python/`（docparse-client，零依赖）：子进程包 CLI、urllib 包 REST，两传输同形输出；自带 **LangChain `DocumentLoader`** 与 **LlamaIndex `Reader`** 适配（每 chunk 一个带 `page`/`bbox`/`heading_path`/`kind` metadata 的 Document）。

```python
from docparse_client import DocparseClient
docs = DocparseClient().chunks("paper.pdf")   # [{id,kind,text,page,bbox,heading_path}, ...]
```

### F. TypeScript / Node 集成
`clients/typescript/`（docparse-client，零运行时依赖）：子进程包 CLI、`fetch`+`FormData` 包 REST，两传输同形输出；自带 **LangChain.js `DocumentLoader`**（`docparse-client/langchain`，每 chunk 一个带 `page`/`bbox`/`heading_path`/`section_id`/`kind` metadata 的 Document）与 **Vercel AI SDK 工具**（`docparse-client/ai`，`docparseTools()` → `get_chunks`/`outline`/`parse_markdown`）。

```ts
import { DocparseClient } from 'docparse-client';
const chunks = await new DocparseClient().chunks('paper.pdf');   // [{id,kind,text,page,bbox,heading_path,section_id}, ...]
```

## 6b. 机器可读契约（schema / OpenAPI）

不用读本文档手抄字段——契约是机器可读的，且**由代码生成**（schemars 从产出 JSON 的同一批 serde 类型派生），所以永不漂移。

- **离线拿 schema**：`docparse schema` 打印全部输出 schema（JSON 对象，按名字索引）；`docparse schema --name chunk` 取单个；`docparse schema --write` 刷新仓库内的 `schemas/*.json`（已入库，可直接 `curl` raw 文件）。
- **入库的 schema 文件**：[`schemas/`](../schemas/) —— `document` / `chunk` / `outline` / `quality` / `profile` / `okf-bundle`，draft 2020-12，可喂 `datamodel-codegen`(Python)、`quicktype`(TS/Go…) 生成类型，或在 agent 侧做响应校验。
- **REST 自描述**：`GET /openapi.json`（OpenAPI 3.1，component 即上述 schema）+ `GET /schema/{name}`。
- **MCP 自描述**：每个结构化工具的 `outputSchema` + `resources/*` 暴露的 `docparse://schema/<name>.json`——三面同源同名。
- **防漂移**：golden 测试断言 `schemas/*.json` 等于代码当前生成结果；改了输出契约就跑 `docparse schema --write` 重生并提交。

```bash
docparse schema --name chunk > chunk.schema.json
datamodel-codegen --input chunk.schema.json --output models.py   # Python 类型
# 或服务化：
curl -s localhost:8642/openapi.json | jq '.components.schemas | keys'
```

## 7. 支持格式

PDF / DOCX / HTML / XLSX / PPTX / Markdown / CSV / EML / SRT・VTT / 图片(PNG・JPEG) / AsciiDoc / LaTeX 源。按扩展名自动选后端；新格式只需实现 `DocumentParser` 并注册一行（见 [iteration-guide.md](iteration-guide.md)）。

## 8. 运维要点

- **零运行时依赖**：单二进制，无 JVM/Python/GPU；模型是外部文件、可选。
- **确定性**：同文件多次运行字节一致——适合做缓存键、可复现引用。
- **安全**：隐藏文本过滤（防 prompt injection 的不可见/离页/微小文本）、zip-bomb 预检、页数早停；解析失败的页返回空页而非 panic。
- **资源**：逐页 rayon 并行；冷启动 <100ms（无模型）；难页增强才按需渲染该页（纯 Rust，~2.4s/页量级）。
