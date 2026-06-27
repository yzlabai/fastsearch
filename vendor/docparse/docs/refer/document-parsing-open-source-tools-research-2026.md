# 面向 RAG/LLM 的开源文档解析工具调研报告

> 调研日期：2026-06-09  
> 调研范围：PDF、Office 文档、图片、扫描件中的文本、表格、公式、图像与版面结构解析  
> 主要用途：企业知识库、RAG、文档检索、结构化抽取和离线批处理

## 1. 执行摘要

文档解析不是单一 OCR 问题，而是由文件识别、原生文本提取、版面分析、阅读顺序恢复、OCR、表格/公式识别、图像理解、结构归一化和质量检测组成的一条流水线。不同项目优化的环节不同，因此不存在对所有格式和场景都最优的单一工具。

本次调研的核心结论如下：

1. **通用多格式主解析器首选 Docling。** 它原生覆盖 PDF、DOCX、PPTX、XLSX、HTML、图片等格式，提供统一的 `DoclingDocument` 中间表示、无损 JSON 和成熟的 RAG 集成，许可证也相对友好。
2. **OpenDataLoader PDF 是值得重点 PoC 的 PDF 专用解析器。** 它的纯 Java 快速路径速度高、输出包含元素级坐标，并具有 Tagged PDF、隐藏文本过滤和 PDF 可访问性能力。其 Hybrid 模式会把复杂页面路由到 Docling，因此更适合被理解为“高性能 PDF 前端与调度层”，而不是完全独立于 Docling 的 AI 解析栈。
3. **中文、扫描件和复杂科技文档优先补测 MinerU 与 PaddleOCR。** MinerU 的文档级输出、公式、跨页表格和可视化调试能力较完整；PaddleOCR 的 OCR、表格单元格坐标、印章、图表和中文生态更强。
4. **MarkItDown 和 Apache Tika 更适合轻量提取与长尾格式兜底。** 它们不应承担复杂 PDF 的高保真结构恢复，但非常适合低成本处理普通 Office 文档、邮件、压缩包、旧格式和元数据。
5. **Marker、Dolphin 的许可证风险必须单独处理。** Marker 代码为 GPL-3.0，默认模型权重还包含营收、融资和竞品限制；Dolphin 仓库当前采用仅限非商业用途的 Qwen Research License。二者不能简单按“公开 GitHub 仓库”理解为可自由商用的开源组件。
6. **生产方案应采用文件路由和质量回退。** 推荐以 Docling 作为统一模型，OpenDataLoader PDF 处理普通数字 PDF，MinerU/PaddleOCR 处理复杂或中文扫描文档，Tika/MarkItDown 处理长尾格式，并建立统一的规范化输出和质量评分。

## 2. 调研方法

### 2.1 评估维度

本报告按以下维度评估：

- 输入格式：PDF、图片、DOCX、PPTX、XLSX、旧版 Office、HTML、邮件等。
- 内容能力：文本、标题层级、列表、表格、公式、图片、图表、印章、手写内容。
- 版面能力：多栏阅读顺序、页眉页脚、脚注、跨页内容、元素坐标。
- 输出能力：Markdown、HTML、JSON、元素树、坐标、图片资源和调试文件。
- OCR：扫描件检测、语言覆盖、按页启用、引擎可替换性。
- 工程能力：CPU/GPU、批处理、服务化、容器化、失败回退、生态集成。
- 许可证：代码许可证、模型许可证、商业使用限制和归因义务。
- 活跃度：近期发布、文档完整度、Issue/PR 活跃情况和社区规模。

### 2.2 资料可信度

资料优先级如下：

1. 项目许可证原文、官方仓库和官方文档。
2. 项目论文、官方 benchmark 代码和数据集。
3. 基金会或行业组织发布的材料。
4. Reddit 等社区讨论，仅作为使用体验线索，不作为性能结论。

所有性能数字都应视为特定版本、数据集和硬件下的结果。最终选型必须使用本公司的真实文档集复测。

### 2.3 “开源”口径

本报告会区分三类状态：

- **标准开源许可证**：如 Apache-2.0、MIT、GPL-3.0。
- **带附加条件的源码可用许可证**：代码或权重可下载，但存在规模、归因、用途或商业限制。
- **仅研究可用**：不允许直接商业使用。

因此，报告保留 Marker、MinerU 和 Dolphin 作为技术候选进行比较，但不会因为仓库公开就把它们一概视为“可无条件商用的开源软件”。

## 3. 工具全景对比

### 3.1 核心能力矩阵

| 工具 | 定位 | PDF | Office | 扫描/OCR | 表格/公式 | 结构化坐标 | 主要输出 | 资源 |
|---|---|---:|---:|---:|---:|---:|---|---|
| OpenDataLoader PDF | 高速 PDF 解析与可访问性 | 强 | 不支持 | Hybrid 支持 | Hybrid 较强 | 强 | MD/JSON/HTML/Tagged PDF | CPU；Hybrid 约 2-4 GB RAM |
| Docling | 多格式统一文档解析 | 强 | DOCX/PPTX/XLSX | 支持多 OCR 引擎 | 强 | 强 | MD/HTML/JSON/Text/DocTags | CPU 可用，GPU 可加速 |
| MinerU | 复杂文档与科技文档解析 | 强 | DOCX/PPTX/XLSX | 109 种语言 | 强 | 强 | MD/JSON/中间结果/调试 PDF | CPU 可用，GPU/NPU/MPS 可加速 |
| PaddleOCR PP-StructureV3 | OCR 与文档视觉结构分析 | 强 | 非主要目标 | 很强 | 很强 | 很强 | MD/JSON/DOCX/可视化 | CPU/GPU，多模型流水线 |
| Marker | PDF 转 Markdown/JSON | 强 | 需额外依赖 | Surya OCR | 强 | 强 | MD/JSON/HTML/Chunks | PyTorch；约 3.5 GB VRAM/worker |
| Unstructured | 多格式 ETL 与元素切分 | 中强 | 广 | Tesseract | PDF 表格依赖 hi_res | 支持元素元数据 | Element JSON/Text | 依赖较多，可容器化 |
| MarkItDown | 轻量多格式转 Markdown | 中弱 | 强 | 插件/LLM Vision | 基础 | 弱 | Markdown | 轻量；高级 OCR 可能依赖 API |
| Apache Tika | 超多格式文本/元数据提取 | 基础 | 很广，含旧格式 | 可接 Tesseract | 弱 | 弱 | Text/XHTML/Metadata | Java，适合服务化 |
| olmOCR | VLM 文档线性化 | 强 | 不支持 | 强 | 强 | 非主要输出 | Markdown/Dolma | 7B VLM，至少约 12 GB VRAM |
| Dolphin | 研究型端到端文档视觉模型 | 强 | 需先渲染 | 强 | 强 | 有布局输出 | JSON/Markdown | GPU 模型推理 |

### 3.2 许可证矩阵

| 工具 | 代码/仓库许可证 | 模型或附加条款 | 商业集成判断 |
|---|---|---|---|
| OpenDataLoader PDF 2.x | Apache-2.0 | Hybrid 依赖的 OCR/Docling 模型需逐项检查 | 友好 |
| Docling | MIT | 使用的独立模型各自适用原许可证 | 友好，但需生成模型清单 |
| MinerU | Apache-2.0 衍生自定义许可证 | 超过 1 亿 MAU 或月收入 2000 万美元需另行许可；在线服务需显著标识 MinerU | 可商用但不是无附加条件 |
| PaddleOCR | Apache-2.0 | 具体模型、字体和推理依赖仍需生成 SBOM | 较友好 |
| Marker | GPL-3.0 | 默认模型权重限制年营收/融资超过 200 万美元及竞品用途 | 高风险 |
| Unstructured | Apache-2.0 | 开源库与商业平台能力边界需确认 | 较友好 |
| MarkItDown | MIT | OCR 插件若调用外部模型/API，受对应服务条款约束 | 友好 |
| Apache Tika | Apache-2.0 | 外部解析器/OCR 工具各自适用许可证 | 友好 |
| olmOCR | Apache-2.0 | 官方仓库声明模型与工具采用 Apache-2.0 | 较友好 |
| Dolphin | Qwen Research License | 仅限非商业用途，商用需单独申请 | 不适合直接商用 |

> 许可证判断仅用于技术选型预警，不构成法律意见。生产引入前仍需法务审核完整依赖树和模型权重。

## 4. 重点工具分析

## 4.1 OpenDataLoader PDF

### 定位与架构

OpenDataLoader PDF 是 Hancom 主导的 Java PDF 解析器，提供 Python、Node.js 和 Java 接口。它有两条路径：

- **Fast/Java 路径**：使用确定性规则、PDF 原生对象和 XY-Cut++ 阅读顺序算法解析数字 PDF，不需要 GPU。
- **Hybrid 路径**：先判断页面复杂度，把表格、OCR 或不确定页面发送到本地 AI 后端。目前官方后端名为 `docling-fast`，安装时会带入 Docling。

因此，Hybrid 的价值主要是：

- 简单页保留 Java 路径的低延迟。
- 复杂页使用 Docling 的布局、表格和 OCR 能力。
- 最后按页合并为统一输出。

### 主要能力

- 输出 Markdown、HTML、层次化 JSON。
- JSON 为每个元素保留页码和 bounding box。
- 识别标题、段落、列表、页眉页脚、图片和表格。
- 支持读取 PDF 自带的结构标签。
- 过滤透明文字、零字号文字、页面外文字等隐藏内容，降低 PDF prompt injection 风险。
- 支持把无标签 PDF 自动转换为 Tagged PDF。
- Hybrid 模式支持 OCR、复杂表格、LaTeX 公式和图像/图表描述。
- 提供 LangChain Loader。

### 工程特征

- Java 快速路径适合 CPU 高吞吐服务。
- Python 的单次 `convert()` 会启动 JVM，官方建议批量提交文件，避免反复启动。
- Hybrid 后端约需 2-4 GB 内存和 1-2 GB 模型缓存，GPU 可选。
- Fast 与 Hybrid 可以拆成独立服务，便于设置不同队列和资源池。

### 优势

- 在 PDF 原生文本、坐标、Tagged PDF 和可访问性方向有明显差异化。
- Apache-2.0 对商业产品较友好。
- 确定性路径便于回归测试、结果缓存和问题复现。
- 元素级坐标适合 RAG 引用高亮、原文定位和审计。

### 局限与风险

- 只处理 PDF，不是多格式入口。
- 无边框、嵌套表格和扫描件需要 Hybrid；此时准确率和资源成本很大程度取决于 Docling。
- 图表描述采用轻量视觉模型，官方也提示其不适合精确恢复复杂图表数值。
- 官方 benchmark 由项目方发布，PDF Association 的转载明确标注结果为 Hancom 自测。
- benchmark 中部分竞品版本和许可证信息已经过时，例如 MinerU 已在 2026 年变更许可证，说明表格不能视为永久、同版本的公平比较。

### 适用场景

- 大量数字 PDF 的低延迟批处理。
- 需要文本块坐标和引用高亮的 RAG。
- 需要过滤隐藏文字和异常 PDF 内容的安全敏感系统。
- 需要 Tagged PDF 或文档可访问性改造的业务。

## 4.2 Docling

### 定位与架构

Docling 最初由 IBM Research 团队开发，目前托管于 LF AI & Data Foundation。它把不同输入统一转换为 `DoclingDocument`，再导出 Markdown、HTML、Text、DocTags 或无损 JSON。

PDF 有两类主要流水线：

- **Standard Pipeline**：PDF 原生解析 + 可选 OCR + 神经网络布局分析 + 表格结构识别。
- **VLM Pipeline**：将页面图像交给视觉语言模型，适合复杂版面、公式、图像文字和手写内容。

### 主要能力

- 输入支持 PDF、DOCX、PPTX、XLSX、HTML、CSV、Markdown、LaTeX、图片、音视频、WebVTT 及部分专业 XML。
- PDF 支持阅读顺序、表格、公式、代码、图片分类和图表理解。
- OCR 可替换为 EasyOCR、Tesseract、RapidOCR、macOS Vision 等引擎。
- 支持本地和隔离网络部署。
- 提供层次切块与混合切块，以及 LangChain、LlamaIndex、Haystack 等集成。

### 优势

- 多格式覆盖、统一数据模型和 RAG 生态之间的平衡最好。
- MIT 许可证降低了嵌入产品的阻力。
- 既能运行确定性/传统模型流水线，也能切换 VLM。
- 输出模型比单纯 Markdown 更适合后续结构化处理。
- 社区与发布节奏活跃，文档和示例完善。

### 局限与风险

- 完整安装和模型依赖较重，冷启动与模型下载需要规划。
- 开启 OCR、表格、公式、图片描述会显著增加耗时，不能全部无条件打开。
- Office 文件虽然原生支持，但复杂版式和嵌入对象仍应单独验证。
- 不支持旧版二进制 `.doc`，需要 Tika、LibreOffice 或格式转换兜底。
- 代码是 MIT 不代表全部模型都是 MIT，部署时应锁定模型版本并记录许可证。

### 适用场景

- 需要 PDF 与 Office 统一处理的企业知识库。
- 希望内部只维护一种规范化文档模型。
- 需要灵活切换 CPU、GPU、传统流水线和 VLM 的平台。

## 4.3 MinerU

### 定位与架构

MinerU 面向复杂文档、科学文献和多模态内容解析，支持 pipeline 与 VLM 后端。3.x 开始采用 `mineru` 客户端、`mineru-api` 服务和 `mineru-router` 的分层方式，便于多服务和多 GPU 部署。

### 主要能力

- 输入支持 PDF、图片、DOCX、PPTX、XLSX。
- 自动检测扫描或乱码 PDF 并启用 OCR。
- OCR 官方声明支持 109 种语言。
- 支持标题、段落、列表、图片、图注、表格、表注、公式、代码、算法、页眉页脚和脚注。
- 表格可输出 HTML，公式可输出 LaTeX。
- 输出 Markdown、按阅读顺序排列的 JSON、`middle.json`、模型结果和布局可视化 PDF。
- 新版能力包括跨页表格合并、图表解析及表格内图片识别。

### 优势

- 对中文、科技论文、公式和复杂页面有较强针对性。
- 调试文件丰富，方便对解析错误做可视化定位。
- CPU、CUDA、NPU 和 Apple MPS 都有部署路径。
- 支持 CLI、FastAPI、WebUI 和路由服务，生产形态比较完整。

### 局限与风险

- 结构化输出曾随 VLM 后端版本发生不兼容变化，二次开发必须固定版本和 schema。
- 依赖与模型较多，生产部署、模型缓存和启动时间需要实测。
- 使用自定义许可证：大规模商业主体有阈值限制，面向第三方的在线服务还有显著归因义务。
- 旧 benchmark 常基于更早版本，不能直接推断当前版本排名。

### 适用场景

- 中文财报、论文、技术手册、复杂表格和公式文档。
- 需要布局可视化和精细问题排查的解析平台。
- 有 GPU/NPU 资源并接受相对复杂部署的场景。

## 4.4 PaddleOCR 与 PP-StructureV3

### 定位与架构

PaddleOCR 是 OCR 与文档视觉理解工具箱。面向 RAG 的重点不是基础文字识别接口，而是 PP-StructureV3 和 PaddleOCR-VL 文档解析流水线。

PP-StructureV3 由多个可独立替换和训练的模块组成：

- 版面检测。
- 通用 OCR。
- 方向检测与文档图像矫正。
- 表格识别。
- 印章识别。
- 公式识别。
- 图表解析。

### 主要能力

- 支持图片和 PDF。
- 恢复多栏阅读顺序。
- 识别标题、正文、页眉页脚、脚注、公式、表格、图片、图表、印章和侧栏等类型。
- 表格结果包含 HTML、单元格坐标和 OCR 置信度。
- 输出 JSON、Markdown、DOCX 和多种可视化图片。
- 支持本地推理、服务化、多 GPU、多进程、ONNX、OpenVINO 和 TensorRT。

### 优势

- 中文 OCR 和多语言识别生态成熟。
- 表格单元格级坐标、置信度和可训练模块适合需要深度定制的团队。
- 对旋转、畸变、印章和扫描图像的处理能力比普通文件转换器更完整。
- Apache-2.0，部署方式丰富。

### 局限与风险

- 它主要处理页面图像，不负责完整的 Office 原生对象语义。
- 多模块流水线参数多，默认模型不一定适合每种语言；官方文档明确建议纯英文场景替换英文识别模型。
- PDF 多页结果需要调用方合并，跨页结构和文档级层次仍需额外处理。
- PaddlePaddle、PaddleX、模型和推理后端的版本兼容性需要严格管理。

### 适用场景

- 中文扫描 PDF、图片、票据、印章和畸变文档。
- 需要单元格坐标、OCR 置信度或自训练模型的业务。
- 作为 Docling/MinerU 的 OCR 或特殊页面回退引擎。

## 4.5 Marker

### 定位与架构

Marker 使用 Surya 等模型完成文本提取、OCR、布局检测、阅读顺序、公式和表格处理，输出 Markdown、JSON、HTML 或 chunks。可选择 LLM 对结果做二次修正。

### 优势

- PDF、公式和表格能力完整。
- JSON 输出包含块结构，可单独提取表格或只运行 OCR。
- 提供批处理、多 GPU 和调试可视化能力。

### 局限与风险

- 官方文档给出的单 worker 平均显存约 3.5 GB、峰值约 5 GB，横向扩展成本不低。
- 复杂嵌套表格和表单仍是已知限制。
- 代码为 GPL-3.0；如果分发或嵌入产品，需要评估 GPL 传染性和源码义务。
- 默认模型权重采用修改版 AI Pubs OpenRAIL-M：
  - 前一年营收超过 200 万美元，不能用于一般商业目的。
  - 累计融资超过 200 万美元，不能用于一般商业目的。
  - 不能用于与许可方产品竞争的产品或服务。
  - 模型输出和衍生物也包含额外的 share-alike/归因条件。

### 结论

Marker 可以作为实验室 benchmark 候选，但不建议在未取得商业许可和法务确认前进入企业默认生产栈。

## 4.6 Unstructured

### 定位与架构

Unstructured 是文档 ETL 和元素化切分框架。它使用 `partition()` 自动识别文件类型，并路由到相应的 partition 函数，输出 `Title`、`NarrativeText`、`ListItem`、`Table` 等元素。

### 主要能力

- 支持 DOC/DOCX、PPT/PPTX、XLSX、PDF、图片、邮件、RTF、EPUB、HTML、XML、CSV 和文本等。
- 通常提供 `fast` 与 `hi_res` 策略。
- PDF 表格抽取依赖 `hi_res`。
- 提供元素、切块、连接器和大量 LLM 生态适配。

### 优势

- 输入格式、数据源连接器和 ETL 概念成熟。
- 元素类型适合直接进入 chunking 和 embedding 流程。
- Apache-2.0，Docker 镜像和社区经验较多。

### 局限与风险

- 完整安装会依赖 `libmagic`、Poppler、Tesseract、LibreOffice、Pandoc 等系统组件。
- PDF 高精度策略的速度和资源成本明显高于 fast。
- 开源库和商业 Platform 同时发展，部分生产增强能力只存在于商业产品，需要逐项确认边界。
- 复杂 PDF 的结构精度在多个公开 benchmark 中通常不及新一代专用解析器。

### 适用场景

- 已经使用 Unstructured 连接器和元素模型的 ETL 系统。
- 格式多、数据源多，而 PDF 极致精度不是唯一目标。

## 4.7 MarkItDown

### 定位与架构

MarkItDown 是 Microsoft 提供的轻量 Python 文件转 Markdown 工具。它更接近“面向 LLM 的格式转换器”，而不是高级文档视觉解析引擎。

### 主要能力

- 支持 PDF、Word、PowerPoint、Excel、图片、音频、HTML、CSV、JSON、XML、ZIP、EPUB 和部分 URL。
- 可按格式安装可选依赖。
- 支持第三方插件。
- 官方 OCR 插件通过 OpenAI-compatible 视觉模型处理 PDF、DOCX、PPTX 和 XLSX 中的图片及扫描页。

### 优势

- API 简单、依赖可裁剪、Markdown 输出直接。
- 普通 Office 文档和文本型文件处理成本低。
- MIT 许可证，适合作为工具链中的轻量入口。
- 插件机制可以把复杂 PDF 路由给更专业的解析器。

### 局限与风险

- 官方定位明确说明它不追求高保真文档转换。
- 内置 PDF 解析不适合复杂表格、多栏和扫描件。
- OCR 插件不是传统本地 OCR，而是依赖外部或自建的兼容视觉模型；这会引入数据传输、费用和模型条款。
- 输出以 Markdown 为中心，缺乏统一、丰富的元素坐标模型。

### 适用场景

- 普通 Office、HTML、CSV、JSON 等文档快速进入 LLM。
- 命令行工具、个人知识库和轻量原型。
- 作为文件类型路由层，而不是复杂 PDF 最终解析器。

## 4.8 Apache Tika

### 定位与架构

Apache Tika 是成熟的内容检测、文本提取和元数据框架。Tika 3.3.1 是截至调研日的稳定版本，4.0.0-alpha-1 已发布但包含重大重构，不建议立即用于保守生产环境。

### 主要能力

- 使用统一接口检测和提取上千种文件类型。
- 覆盖新旧 Microsoft Office、OpenDocument、iWork、WordPerfect、PDF、邮件、压缩包、音视频、CAD、数据库和科学格式。
- PDF 基于 PDFBox，Office 基于 Apache POI。
- 可接入 Tesseract OCR、Grobid 和其他外部解析器。
- 提供可直接运行的 `tika-server`。

### 优势

- 长尾格式、旧文件和元数据提取能力最强。
- Java 服务成熟、稳定、易于做统一文件检测。
- Apache-2.0，企业使用历史长。

### 局限与风险

- 默认目标是文本和元数据，不是高保真版面理解。
- 表格、标题层级、阅读顺序和坐标能力弱于现代文档 AI 工具。
- 外部 OCR/NLP 解析器会增加部署复杂度。

### 适用场景

- 未知格式检测和长尾格式兜底。
- 旧版 Office、邮件、压缩包、嵌套附件和元数据提取。
- 作为解析路由器的第一层，而不是所有 PDF 的最终处理器。

## 4.9 olmOCR

### 定位与架构

olmOCR 是 Allen Institute for AI 发布的 VLM 文档线性化工具，主要用于把大规模 PDF 或页面图片转换成自然阅读顺序的 Markdown/Dolma 数据。

### 主要能力

- PDF、PNG、JPEG 转 Markdown。
- 支持公式、表格、手写、多栏、插图和复杂阅读顺序。
- 自动去除页眉页脚。
- 支持本地 vLLM、远程 OpenAI-compatible 推理服务和 Docker。
- 官方 benchmark 包含 1,400 份文档、7,000 多个测试用例。

### 优势

- 对极难 OCR、手写和视觉布局有较强能力。
- Apache-2.0。
- 批量管线、重试、失败率和远程推理设计适合大规模语料处理。

### 局限与风险

- 基于 7B VLM，本地官方建议至少约 12 GB 显存和约 30 GB 磁盘。
- 输出重点是线性化文本，而不是精细的元素坐标和 Office 原生结构。
- 对只包含正常文本层的简单 PDF，成本明显高于确定性解析器。

### 适用场景

- 传统 OCR/布局模型持续失败的复杂文档。
- 手写、历史资料和大规模训练语料构建。
- 作为高成本最终回退，而不是默认首路。

## 4.10 Dolphin

### 定位与架构

Dolphin 是 ByteDance 发布的研究型文档图像解析模型，采用两阶段方法：

1. 文档类型判断、布局分析和阅读顺序预测。
2. 对拍摄文档进行整体解析，对数字文档中的元素进行并行解析。

支持页面级 JSON/Markdown，以及文本、表格、公式、代码等元素级解析。

### 优势

- 模型架构专门面向复杂文档图像。
- 同时提供页面级、布局级和元素级示例。
- 社区讨论中对其速度和效果有积极反馈。

### 局限与风险

- 更接近模型和研究代码，不是完整的多格式 ETL 平台。
- 工程接口、生产服务化、错误回退和 schema 稳定性弱于 Docling/MinerU。
- 仓库 LICENSE 当前为 Qwen Research License，仅允许非商业研究或评估；商用必须另行申请。

### 结论

Dolphin 适合作为研究比较对象或非商业 PoC，不应列入默认商业生产候选。

## 5. Benchmark 解读

### 5.1 OpenDataLoader benchmark

OpenDataLoader 公布的 200 份真实 PDF benchmark 使用：

- 阅读顺序指标 NID。
- 表格结构指标 TEDS。
- 标题层级指标 MHS。
- 三项归一化后的综合分数。

其公开结果中：

- OpenDataLoader Hybrid 综合分数为 0.907。
- Docling 为 0.882。
- Marker 为 0.861。
- Unstructured hi_res 为 0.841。
- OpenDataLoader Java-only 为 0.831。

这些结果有参考价值，但必须同时注意：

- benchmark 由 OpenDataLoader 项目方设计和发布。
- PDF Association 的新闻稿明确标记为 Hancom 自测。
- Hybrid 模式依赖 Docling，不能把 0.907 理解为完全独立模型对 Docling 的胜出。
- 项目版本变化快，表格中的 MinerU 许可证仍是旧 AGPL，说明竞品版本和元数据可能不是最新状态。
- 200 份 PDF 未必覆盖企业内部中文扫描件、合同骑缝章、合并单元格、工程图纸和超长表格。

### 5.2 不同 benchmark 不能直接横比

Marker、MinerU、PaddleOCR、olmOCR 都发布了自己的 benchmark，但任务定义不同：

- 有的评估页面 Markdown 编辑距离。
- 有的评估 OCR 字符准确率。
- 有的评估表格树编辑距离。
- 有的评估阅读顺序或人工/LLM 打分。

因此不能把来自不同项目的单一总分拼成排行榜。合理做法是先按功能和许可证筛选，再在同一文档集、同一输出规范和同一硬件上运行。

## 6. 推荐技术架构

### 6.1 分层解析架构

```text
文件接入
  |
  v
文件检测与安全检查
  |-- MIME / 扩展名 / 压缩包展开：Apache Tika
  |-- 文件大小、页数、加密、恶意对象、超时限制
  |
  v
格式路由
  |-- 普通 DOCX/PPTX/XLSX/HTML：Docling
  |-- 轻量 Office/文本格式：MarkItDown
  |-- 普通数字 PDF：OpenDataLoader Fast
  |-- 复杂数字 PDF：OpenDataLoader Hybrid 或 Docling
  |-- 中文扫描/印章/畸变：PaddleOCR 或 MinerU
  |-- 公式/科技文档：MinerU 或 Docling VLM
  |-- 极难页面/手写：olmOCR
  |-- 旧格式和长尾格式：Apache Tika
  |
  v
统一文档模型
  |-- document / page / section / block / span
  |-- type / text / bbox / page / confidence
  |-- table cells / formula / image / caption
  |-- source parser / parser version / model version
  |
  v
质量检测与回退
  |-- 文本覆盖率、乱码率、重复率
  |-- 阅读顺序异常、空页、表格闭合性
  |-- OCR 置信度、标题层级和跨页连续性
  |
  v
结构化切块、索引与引用定位
```

### 6.2 推荐组合

#### 组合 A：企业通用知识库

- 主模型：Docling。
- PDF 快速入口：OpenDataLoader Fast。
- 复杂 PDF：OpenDataLoader Hybrid/Docling Standard。
- 中文扫描回退：PaddleOCR。
- 长尾格式：Tika。

优点是许可证相对清晰、格式覆盖广、坐标信息完整，并能控制 GPU 使用范围。

#### 组合 B：中文财报、论文和技术手册

- 主解析：MinerU。
- OCR/印章与表格单元格增强：PaddleOCR。
- 普通 Office：Docling。
- 普通数字 PDF 快速通道：OpenDataLoader。

需要特别落实 MinerU 在线服务归因和大规模商业阈值评估。

#### 组合 C：低资源快速上线

- Office/HTML/文本：MarkItDown。
- PDF：OpenDataLoader Fast。
- 扫描件：仅检测失败后调用 PaddleOCR。
- 其他格式：Tika。

该组合部署轻，但复杂布局精度会低于完整模型流水线。

## 7. OCR 与解析路由策略

不建议对所有 PDF 强制 OCR。合理流程如下：

1. 检查每页是否存在可用文本对象。
2. 计算字符数、可打印字符比例、乱码率和文本覆盖区域。
3. 数字文本正常时直接提取，避免 OCR 引入错字和重复层。
4. 没有文本层、文本层乱码或页面主要由大图组成时启用 OCR。
5. 混合 PDF 应按页路由，不应整本强制 OCR。
6. OCR 后检测原生文本与 OCR 文本是否重复。
7. 对低置信度页或关键表格页使用第二解析器复核。

OpenDataLoader Hybrid 的页面分流思路值得借鉴：简单页面走低成本路径，表格页和不确定页面走模型路径。

## 8. PoC 测试建议

### 8.1 测试集

建议准备至少 200-500 份真实文档，并按业务占比分层：

- 普通数字 PDF。
- 双栏/多栏 PDF。
- 扫描 PDF、手机拍照和低分辨率文档。
- 中文、英文、中英混排。
- 有框表格、无框表格、合并单元格、跨页表格。
- 公式、代码、脚注、参考文献。
- 图片、流程图、折线图和柱状图。
- DOCX、PPTX、XLSX。
- 旧版 DOC/PPT/XLS、邮件和压缩附件。
- 加密、损坏、超长和异常 PDF。

### 8.2 评价指标

不能只比较 Markdown 肉眼观感，至少记录：

- 文本 CER/WER 或归一化编辑距离。
- 阅读顺序准确率。
- 标题层级准确率。
- 表格结构 TEDS 和单元格文本准确率。
- 公式 LaTeX 编辑距离。
- 图片/图表召回率。
- 页码和 bounding box 定位误差。
- 文档成功率、页面失败率和超时率。
- 每页延迟、吞吐、CPU、内存、GPU 显存。
- 冷启动、模型下载和容器大小。
- 解析结果对实际 RAG 问答正确率的影响。

### 8.3 建议入围工具

第一轮建议测试：

- OpenDataLoader Fast 与 Hybrid。
- Docling Standard。
- MinerU 当前稳定版。
- PaddleOCR PP-StructureV3。
- MarkItDown，用于普通 Office 基线。
- Apache Tika，用于长尾格式基线。

第二轮仅对困难样本测试：

- Docling VLM。
- olmOCR。
- Marker，仅在许可证允许的隔离评估环境。
- Dolphin，仅用于非商业研究比较。

### 8.4 通过标准

建议将生产准入设置为：

- 数字 PDF 解析成功率不低于 99.5%。
- 扫描文档页面成功率不低于 98%。
- 普通文本页 P95 延迟满足业务 SLA。
- 所有失败均产生可追踪错误码，不能静默输出空文档。
- 输出包含解析器、模型和 schema 版本。
- 相同输入和版本能够稳定复现。
- 高风险许可证组件不进入默认生产镜像。

## 9. 最终选型建议

### 9.1 首选方案

建议采用以下技术栈：

- **统一文档模型与多格式入口：Docling**
- **普通 PDF 高速解析与坐标/安全能力：OpenDataLoader PDF**
- **中文扫描、印章和表格增强：PaddleOCR**
- **复杂科技文档备选：MinerU**
- **长尾格式检测和旧格式：Apache Tika**
- **普通 Office 轻量路径：MarkItDown**
- **极难视觉文档最终回退：olmOCR**

### 9.2 暂不建议作为默认生产依赖

- **Marker**：代码 GPL-3.0，模型权重商业限制较强。
- **Dolphin**：当前许可证仅允许非商业用途。
- **Unstructured**：不是不能使用，而是在新建解析平台时，其复杂 PDF 精度和系统依赖相对不占优；如果现有系统已经深度使用其连接器和 Element 模型，则迁移收益需要单独计算。

### 9.3 最关键的工程决策

最终效果更取决于以下工程设计，而不是排行榜第一名：

- 是否按格式和页面复杂度路由。
- 是否保留页码、坐标、元素类型和解析来源。
- 是否有 OCR 重复文本清理和质量评分。
- 是否能对失败页自动切换第二解析器。
- 是否使用真实业务问答验证解析结果，而非只看 Markdown。
- 是否锁定代码、模型、OCR 引擎和 schema 版本。

## 10. 使用 Rust 构建文档解析系统

### 10.1 产品定位

如果使用 Rust 开发，建议把系统定位为：

> 一个安全、高吞吐、可扩展的文档解析平台内核，通过 Rust 原生快速解析器处理常规文档，并通过标准插件协议调用 Docling、MinerU、PaddleOCR 等模型服务处理复杂页面。

第一阶段不建议追求“所有能力纯 Rust”。原因是文档解析中的模型、训练代码和最新研究成果仍主要在 Python 生态。Rust 更适合负责：

- 文件接入与安全预检。
- 原生文本和 Office 结构的快速解析。
- 页面级分类与解析器路由。
- 任务调度、背压、超时和资源配额。
- 多解析器结果归一化。
- 质量检测、重试与回退。
- 输出、切块、缓存、索引和可观测性。

复杂 OCR、布局模型、表格模型和 VLM 可先作为外部 worker；成熟、稳定的小模型再逐步用 ONNX Runtime 或 Candle 迁入 Rust 进程。

### 10.2 系统需要具备的核心功能

#### 1. 文件接入

- 文件上传、对象存储 URI、HTTP URL 和批量目录导入。
- 流式读取，避免大文件一次性加载到内存。
- MIME magic 检测，不能只信任扩展名和客户端 `Content-Type`。
- SHA-256 内容寻址、幂等任务和重复文件检测。
- 文件大小、页数、解压后大小和嵌套深度限制。
- 加密文档识别及密码注入机制。
- ZIP、邮件和容器文件的受限展开。

#### 2. 安全预检

- 拒绝路径穿越、ZIP bomb、超高压缩比和递归归档。
- PDF JavaScript、嵌入文件、外部链接、启动动作和隐藏文字检测。
- 宏、OLE 对象、外部关系和远程模板检测。
- 解析 worker 使用独立进程或容器，配置 CPU、内存、文件和运行时间上限。
- 临时目录隔离、只读根文件系统和禁止默认网络访问。
- 对原始文件、派生图片和模型服务传输进行权限控制和审计。

Rust 的内存安全能减少解析器自身的内存破坏风险，但不能替代进程隔离、资源限制和恶意文档检测。

#### 3. 文档类型与复杂度分析

系统需要在真正解析前生成 `DocumentProfile`：

- 文件类型、文件版本、页数和语言。
- 数字文本、扫描图像或混合 PDF。
- 每页文本密度、图片覆盖率和乱码率。
- 表格、公式、多栏、旋转页、印章和手写内容概率。
- 加密、损坏、字体映射异常和异常对象。
- 推荐解析器、预计资源等级和是否需要 GPU。

路由粒度应同时支持文档级和页面级。一本 PDF 可以让普通页走 Rust 快速路径，仅把表格页或扫描页发送给 AI worker。

#### 4. Rust 原生解析器

建议优先实现以下确定性能力：

- PDF 对象、页面树、文本绘制命令、字体映射、图片和书签解析。
- 字符级位置、字号、字体、颜色和变换矩阵恢复。
- 基于 XY 坐标的行、段落、多栏和阅读顺序重建。
- 页眉页脚、页码、水印和重复内容识别。
- PDF 自带结构树和 Tagged PDF 读取。
- DOCX 段落、样式、标题、列表、表格、脚注、批注和媒体关系解析。
- PPTX 幻灯片、文本框、层级、表格、备注和图片解析。
- XLSX/ODS 单元格、公式、合并区域、隐藏行列和工作表元数据解析。
- HTML、Markdown、CSV、JSON、XML 和纯文本解析。

可评估的 Rust 组件包括：

- `lopdf`：PDF 对象读取、修改和解密。
- `pdf-rs`/`pdf_oxide`：PDF 解析、渲染或文本提取候选。
- `calamine`：纯 Rust XLS/XLSX/XLSB/ODS 读取。
- `rs-docx`：DOCX 读取和写入候选。
- `zip`、`quick-xml`：直接解析 OOXML 包。
- `html5ever`、`scraper`：HTML DOM 解析。

这些库可以降低起步成本，但 PDF 阅读顺序、复杂表格和 OOXML 完整语义仍需要系统自行补齐。尤其不能直接把某个 crate 的项目方 benchmark 当作生产结论。

#### 5. OCR 与文档 AI

OCR 层应设计成可插拔能力，而不是写死某个引擎：

```rust
trait OcrEngine {
    async fn recognize(&self, request: OcrRequest) -> Result<OcrPage>;
}

trait LayoutEngine {
    async fn analyze(&self, request: LayoutRequest) -> Result<LayoutPage>;
}

trait VisionEngine {
    async fn describe(&self, request: VisionRequest) -> Result<VisionResult>;
}
```

需要支持：

- Tesseract、本地 ONNX OCR、PaddleOCR、MinerU、Docling 和兼容 VLM 服务。
- 按页选择 OCR，避免整本重复识别。
- 图像矫正、方向检测、去噪、二值化和分辨率控制。
- OCR 文本与原生 PDF 文本去重。
- 文本、表格、公式、图表和印章的独立置信度。
- worker 能力发现、模型版本注册和灰度切换。

Rust 内嵌模型可优先使用：

- `ort` 调用 ONNX Runtime，复用成熟的硬件执行后端。
- Candle 运行适合 Rust 的轻量模型，支持 CPU、CUDA 和 Metal。
- `tokenizers` 处理 Transformer tokenizer。

大型 VLM 和更新频繁的研究模型仍建议独立部署，以免 Rust 主服务与 CUDA/PyTorch 依赖耦合。

#### 6. 统一文档中间表示

这是系统最重要的长期接口。建议定义版本化的 `Document IR`：

```rust
struct Document {
    schema_version: String,
    id: DocumentId,
    metadata: Metadata,
    pages: Vec<Page>,
    assets: Vec<Asset>,
    provenance: Provenance,
}

struct Block {
    id: BlockId,
    kind: BlockKind,
    page: u32,
    bbox: BoundingBox,
    reading_order: u32,
    confidence: Option<f32>,
    content: BlockContent,
    source: ParseSource,
}

enum BlockContent {
    Text(Vec<Span>),
    Table(Table),
    Formula(Formula),
    Image(AssetRef),
    Group(Vec<BlockId>),
}
```

IR 至少应保存：

- 文档、页、章节、块、行、span 的层级。
- 原始坐标、归一化坐标、旋转角和坐标系。
- 标题、段落、列表、代码、引用、页眉页脚等语义类型。
- 表格行列、跨行跨列、单元格坐标和单元格内容。
- 公式的原图、文本和 LaTeX。
- 图片资源、图注及页面引用。
- 原始文本、规范化文本和语言。
- 解析器、代码版本、模型版本、参数、耗时和置信度。
- 原始元素到导出 Markdown/HTML/chunk 的可追踪映射。

建议用 Protobuf 或版本化 JSON Schema 作为跨语言协议，同时在 Rust 内部使用强类型结构。Schema 必须允许添加字段，并提供迁移与兼容性测试。

#### 7. 结果融合

同一页面可能同时有原生文本、OCR、布局模型和表格模型结果。系统需要：

- 坐标归一化和页面旋转校正。
- 基于 bbox 重叠、文本相似度和来源优先级的元素对齐。
- 原生文本与 OCR 文本去重。
- 多引擎候选结果打分。
- 标题、图注、表注和脚注的关联。
- 跨页段落、列表和表格合并。
- 冲突保留和人工调试视图，不能只输出最终黑盒结果。

#### 8. 质量检测与自动回退

每份文档和每一页都应产生质量分：

- 字符覆盖率、乱码率、重复率和异常 Unicode 比例。
- 页面空白率与文本/图片覆盖率。
- 阅读顺序跳跃和多栏交叉。
- 标题层级断裂。
- 表格网格闭合、单元格数量和内容覆盖。
- OCR 平均置信度与低置信度区域。
- 公式、图片、图注和表注匹配情况。
- 页面数量与输出页面数量一致性。

回退策略示例：

```text
Rust PDF fast path
  -> 质量合格：直接输出
  -> 仅少数页失败：失败页调用 Docling/PaddleOCR
  -> 表格失败：只调用表格模型
  -> 全文扫描或严重错序：整份调用 MinerU/Docling
  -> 仍失败：VLM/人工审核队列
```

#### 9. 输出与 RAG 能力

- JSON/Protobuf 无损输出。
- Markdown、HTML、纯文本和 CSV 派生输出。
- 原图、页面图、表格图片和公式图片导出。
- 基于章节、token 数和语义边界的结构化切块。
- chunk 到原始页码和 bbox 的双向映射。
- 引用高亮、原文预览和证据定位。
- 可选全文索引、向量索引和混合检索。
- 增量重解析：解析器升级时只重跑受影响的阶段。

若需要单机全文检索，可使用 Tantivy；向量数据库最好保持外部接口，避免解析内核绑定具体存储。

#### 10. 平台与运维能力

- REST/gRPC API、CLI 和 Rust SDK。
- 同步小文件接口与异步任务接口。
- 优先级队列、租户配额、背压和取消。
- CPU、OCR、GPU/VLM 使用独立队列。
- 阶段级缓存和内容寻址缓存。
- 幂等重试、死信队列和人工重放。
- OpenTelemetry trace、指标和结构化日志。
- 每个阶段的耗时、内存、GPU、页数和失败原因统计。
- 模型与解析器版本注册、灰度发布和回滚。
- 多租户数据隔离、保留策略和删除审计。

### 10.3 推荐架构

```text
                       +----------------------+
Upload / S3 / HTTP --->| Rust API Gateway     |
                       | auth, quota, hashing |
                       +----------+-----------+
                                  |
                       +----------v-----------+
                       | Security & Profiler  |
                       | MIME, bomb, PDF scan |
                       +----------+-----------+
                                  |
                       +----------v-----------+
                       | Rust Router/Scheduler|
                       | page-level DAG       |
                       +---+----------+-------+
                           |          |
             +-------------v--+    +--v------------------+
             | Native Workers |    | AI Plugin Workers  |
             | PDF/OOXML/HTML |    | Docling/MinerU/OCR |
             +-------------+--+    +--+------------------+
                           |          |
                       +---v----------v-------+
                       | IR Normalizer/Merger |
                       +----------+-----------+
                                  |
                       +----------v-----------+
                       | Quality & Fallback   |
                       +----------+-----------+
                                  |
                  +---------------v----------------+
                  | Export/Chunk/Index/Provenance  |
                  +--------------------------------+
```

主进程应保持轻量，不直接加载所有 GPU 模型。插件 worker 可通过 gRPC 或基于对象存储的任务协议连接；对高吞吐本机插件，可进一步使用 Unix domain socket 或共享内存减少页面图像复制。

### 10.4 Rust 带来的优势

#### 性能和成本

- 无 GC 暂停，长时间批处理的延迟更稳定。
- Tokio 异步 I/O 适合大量并发上传、对象存储和 worker 调用。
- Rayon 适合页面解压、图像预处理和确定性 CPU 算法。
- 强类型 IR 减少多阶段 JSON 字段漂移和运行时错误。
- 可精确控制内存、复制次数和并发度。
- 单一静态二进制和较低空闲内存有利于边缘部署和高密度容器。

真正的成本优势主要来自路由：让大多数普通页走 Rust 快速路径，只让困难页消耗 Python/GPU 模型资源。

#### 安全与可靠性

- 相比 C/C++ PDF 解析器，安全 Rust 能显著降低 use-after-free、越界访问和数据竞争风险。
- `Result`、所有权和类型系统适合表达可恢复错误、阶段状态和资源生命周期。
- 编译期并发安全有利于实现取消、背压和任务状态机。
- 容易构建可复现的 CLI 和服务端共用核心库。

但 `unsafe` FFI、PDF 渲染器、ONNX Runtime、OCR 原生库仍可能带来 C/C++ 风险，必须通过进程隔离和依赖审计控制。

#### 可嵌入性

- 同一核心可以导出 CLI、动态库、Python binding、Node N-API 和 WebAssembly。
- 适合在桌面客户端、边缘设备、浏览器或企业内网以本地优先方式部署。
- 比完整 Python 环境更容易分发给不能联网的客户。

#### 平台化

- trait 与强类型协议适合建立稳定插件边界。
- 可以把解析 DAG、资源配额、缓存和 provenance 做成通用底座。
- 多语言模型团队只需遵守协议，不需要进入 Rust 主仓库开发。

### 10.5 Rust 方案的局限

- Rust 原生 PDF/Office 生态的成熟度和格式覆盖仍不及 PDFBox、Apache POI、LibreOffice、PyMuPDF 等多年积累的项目。
- 复杂阅读顺序、表格、公式和图表理解不是换语言就能解决，需要数据集、模型和长期算法投入。
- Python 文档 AI 项目更新很快，直接重写会持续产生追赶成本。
- GPU、CUDA、Metal、ONNX 和图像库的跨平台构建仍然复杂。
- FFI 与外部命令会削弱“单二进制”和纯 Rust 的优势。
- 招聘和调试门槛通常高于 Python，产品迭代早期可能更慢。

因此，Rust 的商业价值应来自稳定平台能力和低成本 fast path，而不是“纯 Rust”本身。

### 10.6 建议的模块划分

```text
crates/
  document-ir          # 统一数据模型和 schema
  document-api         # REST/gRPC 类型
  document-ingest      # 上传、存储、hash、容器展开
  document-security    # MIME、PDF/OOXML 安全检查
  document-profile     # 文档和页面复杂度分类
  parser-core          # Parser/OCR/Layout trait
  parser-pdf           # Rust PDF fast path
  parser-ooxml         # DOCX/PPTX/XLSX
  parser-web           # HTML/Markdown/Text
  parser-plugins       # gRPC/进程插件适配
  document-merge       # 坐标对齐、去重、跨页合并
  document-quality     # 质量评分和回退决策
  document-export      # JSON/MD/HTML/Text
  document-chunk       # RAG 结构化切块
  document-runtime     # DAG、队列、配额、缓存
  document-server      # 服务端
  document-cli         # 命令行
```

插件能力不要直接写成 `if parser == "docling"`。应定义版本化 capability：

- 输入格式。
- 支持的元素类型。
- 是否返回 bbox/confidence。
- 支持的语言和设备。
- 最大页数/图像尺寸。
- 模型与 schema 版本。
- 健康状态和当前负载。

### 10.7 分阶段实施路线

#### 阶段 1：平台 MVP

- Rust API、任务状态机、对象存储、hash 和幂等。
- 统一 Document IR。
- PDF/Office 类型识别与基础文本提取。
- Docling/PaddleOCR gRPC 或 HTTP 插件。
- JSON/Markdown 输出。
- 页级质量指标、超时和回退。

目标不是超过现有解析器，而是证明路由、归一化、追踪和稳定运行。

#### 阶段 2：高性能 fast path

- Rust PDF 字符坐标和阅读顺序。
- 页眉页脚、水印、隐藏文本与重复文本处理。
- DOCX/XLSX/PPTX 原生结构解析。
- 页面复杂度分类器。
- 阶段缓存和困难页路由。

这一阶段开始形成 Rust 方案的成本和吞吐优势。

#### 阶段 3：质量与企业能力

- 结果融合、跨页表格和结构校正。
- 可视化调试器和人工审核。
- 多租户配额、审计、SBOM 和模型许可证登记。
- 自有 benchmark、回归集和 RAG 端到端评价。
- 灰度发布和解析器 A/B 测试。

#### 阶段 4：选择性模型内嵌

- 把稳定的小型页面分类、方向检测和 OCR 模型转换为 ONNX。
- 使用 `ort` 在 Rust worker 中推理。
- 评估 Candle 的纯 Rust/Metal 部署。
- 大型 VLM 继续保留为外部服务。

### 10.8 是否值得使用 Rust

适合使用 Rust 的条件：

- 每天处理大量文档，需要控制 CPU、内存和 GPU 成本。
- 对恶意文件、崩溃隔离和数据本地化要求高。
- 需要私有化、边缘或单机分发。
- 需要稳定的统一 IR、插件平台和多解析器治理。
- 团队有 Rust 工程能力，并愿意长期维护 PDF/OOXML fast path。

不适合一开始全量使用 Rust 的条件：

- 当前目标只是快速验证 RAG 产品。
- 主要工作是频繁尝试最新 OCR/VLM 模型。
- 文档量不大，GPU 成本不是问题。
- 团队缺少 PDF 规范、版面算法和 Rust 经验。

综合建议是采用 **Rust 控制面与确定性数据面 + Python/Java AI worker** 的混合架构。它比纯 Python 平台更容易获得稳定吞吐和资源治理，又不会为了语言纯度重复建设快速变化的模型生态。

## 11. 参考资料

### 社区讨论

- [Reddit: Best open-source tools for parsing PDFs, Office docs, and images before feeding into LLMs?](https://www.reddit.com/r/Rag/comments/1n0pc66/best_opensource_tools_for_parsing_pdfs_office/)

### OpenDataLoader PDF

- [GitHub repository](https://github.com/opendataloader-project/opendataloader-pdf)
- [Official documentation](https://opendataloader.org/docs)
- [Hybrid mode](https://opendataloader.org/docs/hybrid-mode)
- [JSON schema](https://opendataloader.org/docs/reference/json-schema)
- [OpenDataLoader benchmark](https://github.com/opendataloader-project/opendataloader-bench)
- [PDF Association announcement and benchmark caveat](https://pdfa.org/opendataloader-pdf-v20-tops-open-source-pdf-benchmarks-in-pdf-data-loading/)

### Docling

- [GitHub repository](https://github.com/docling-project/docling)
- [Supported formats](https://docling-project.github.io/docling/usage/supported_formats/)
- [Pipeline reference](https://docling-project.github.io/docling/examples/agent_skill/docling-document-intelligence/pipelines/)
- [Pipeline options](https://docling-project.github.io/docling/reference/pipeline_options/)
- [GPU support](https://docling-project.github.io/docling/usage/gpu/)

### MinerU

- [GitHub repository](https://github.com/opendatalab/MinerU)
- [Official documentation](https://opendatalab.github.io/MinerU/)
- [Output formats](https://opendatalab.github.io/MinerU/reference/output_files/)
- [License text](https://github.com/opendatalab/MinerU/blob/master/LICENSE.md)

### PaddleOCR

- [GitHub repository](https://github.com/PaddlePaddle/PaddleOCR)
- [PP-StructureV3 usage](https://www.paddleocr.ai/latest/en/version3.x/pipeline_usage/PP-StructureV3.html)
- [PP-StructureV3 introduction](https://www.paddleocr.ai/main/en/version3.x/algorithm/PP-StructureV3/PP-StructureV3.html)

### 其他工具

- [Marker](https://github.com/datalab-to/marker)
- [Marker model license](https://github.com/datalab-to/marker/blob/master/MODEL_LICENSE)
- [Unstructured](https://github.com/Unstructured-IO/unstructured)
- [Unstructured partitioning](https://docs.unstructured.io/open-source/core-functionality/partitioning)
- [MarkItDown](https://github.com/microsoft/markitdown)
- [MarkItDown OCR plugin](https://github.com/microsoft/markitdown/tree/main/packages/markitdown-ocr)
- [Apache Tika](https://tika.apache.org/)
- [Apache Tika supported formats](https://tika.apache.org/2.9.2/formats.html)
- [olmOCR](https://github.com/allenai/olmocr)
- [Dolphin](https://github.com/bytedance/Dolphin)
- [Dolphin license](https://github.com/bytedance/Dolphin/blob/master/LICENSE)

### Rust 生态

- [lopdf](https://docs.rs/lopdf/latest/lopdf/)
- [pdf-rs](https://github.com/pdf-rs)
- [pdf_oxide](https://docs.rs/pdf_oxide/latest/pdf_oxide/)
- [calamine](https://docs.rs/calamine/latest/calamine/)
- [rs-docx](https://docs.rs/rs-docx/latest/rs_docx/)
- [ort: ONNX Runtime Rust binding](https://docs.rs/ort/latest/ort/)
- [Hugging Face Candle](https://github.com/huggingface/candle)
- [Tantivy](https://docs.rs/tantivy/latest/tantivy/)
