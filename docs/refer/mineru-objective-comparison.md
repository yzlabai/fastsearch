# 客观对比 · docparse-rs vs MinerU(2026-06-18)

> 与 [docling-objective-comparison.md](docling-objective-comparison.md) 同体例:**刻意把对方优势摆全**,供选型与对外表述时保持诚实。MinerU 能力面依据其本地源码(`tmp/refer/MinerU`,3.3 / 2026-06-11)与官方 README 核实,非凭印象。
>
> 一句话:这是**两个定位不同的系统**,不存在单向"更好"——MinerU 用大模型(VLM)换**质量上限与语言广度**,代价是 GPU/重 Python 环境;docparse-rs 用**确定性 + 纯 Rust 单二进制**换部署、速度与可复现,代价是难版面质量天花板与 OCR 语言面。

---

## 0. 两句话画像

- **MinerU**(OpenDataLab / 上海 AI Lab,孵化自 InternLM 预训练):面向"高精度文档解析喂大模型"的 **Python 解析平台**。核心竞争力是自研 **MinerU2.5-Pro-1.2B 视觉语言模型** + 成熟 pipeline,OmniDocBench v1.6 端到端 **85.75(纯 CPU pipeline)~ 95.39(hybrid/vlm)**,业内第一梯队;生态、社区、国产芯片适配、多语言(109 语种 OCR)全部拉满。
- **docparse-rs**:面向 **agent / RAG** 的**纯 Rust 单二进制确定性解析器**。核心竞争力是 born-digital 走"结构提取"快路径不渲染像素(<10ms 预热、~700 页/s)、同输入逐字节一致、page+bbox 双向引用闭环、零运行时依赖;模型全部 opt-in、只在难页触发。

## 1. MinerU 明显占优的轴(诚实清单)

| 轴 | 事实(MinerU) | 我方差距 |
|---|---|---|
| **绝对精度上限** | OmniDocBench v1.6 端到端 Overall:pipeline **85.75**(纯 CPU 可跑)、vlm **95.30**、hybrid **95.39(high)/95.26(medium)**。VLM 端到端解决难表/信息图/旋转/手写 | 我方套官方公式 Overall 代理 ≈75(pipeline-tool 档),且口径更老(我方为 OmniDocBench 早期方法,**两者分数不可直接对齐**,但量级差距真实) |
| **难表/难版面** | TableFormer 系 + VLM 端到端解多级表头/合并格/无线表;cross-page 表合并;表内图/公式识别 | 我方 UniRec-0.1B 固定 960×1408 输入在大/宽/密学术表上有天花板(端到端 0.52,已证伪诸多便宜旋钮) |
| **OCR 语言广度** | **109 语种**检测+识别、方向/竖排/印章文本 | 我方 PP-OCRv6/v4 中英为主,RTL/韩文等域外(明确记 0) |
| **VLM/图表理解** | 图片描述、图表→数据、截断段落合并、`effort` 强度档(medium/high) | 我方仅 `--vlm-describe/--vlm-tables` 外接 OpenAI 兼容服务(mock 验收),无自研 VLM |
| **GPU/吞吐** | CUDA(Volta+)/MPS 加速;vLLM/SGLang/LMDeploy 后端;`mineru-router` 多 GPU 一键负载均衡;滑窗+流式写盘扛万页长文 | 我方 CPU-only(tract),无 GPU 路径 |
| **国产芯片** | 昇腾/寒武纪/燧原/沐曦/摩尔线程/昆仑芯/天数/海光/壁仞/平头哥十余家适配 | 无 |
| **生态成熟度** | PyPI 月级海量下载、MCP、LangChain/LlamaIndex/RAGFlow/Dify/FastGPT 原生集成、Python/Go/TS SDK、在线 web/桌面端/Gradio、3 篇 arXiv 技术报告、大社区 | 我方 0 社区、集成仅 MCP/REST + Python 薄客户端 + LangChain Loader;鲁棒性在 ~20 样例+三件套+压测 1847 输入验证,长尾未经海量锤炼 |
| **格式细节增强** | 自动去页眉/页脚/脚注/页码保语义连贯;多模态/NLP 双版 Markdown;层级 JSON + 丰富中间产物 + 版面/span 可视化 | 我方有阅读顺序与 chunk breadcrumb,但无系统化页眉页脚剔除、无可视化产物 |

## 2. docparse-rs 明显占优的轴(实测)

| 轴 | 实测(docparse-rs) | MinerU 对应 |
|---|---|---|
| **部署** | **~29MB 单二进制**,零运行时依赖,无 JVM/C++/Python;模型全可选外部文件(OCR 默认仅 ~7MB) | Python 3.10–3.13 环境 + 重依赖(torch/vLLM/opencv/pypdfium2…);磁盘建议 ≥20GB SSD、RAM 16–32GB、VLM 路径 VRAM 8GB |
| **born-digital 速度** | **<10ms 预热、~700 页/s(CPU)**,数字页零模型零加载 | pipeline CPU 可跑但过模型;VLM/hybrid 高精度路径需 GPU,单页量级更慢 |
| **确定性** | 同输入**逐字节一致**,且跨 CLI/MCP/REST/库**四面一致**(含 OCR 路径),有测试钉死 | VLM 推理本质非字节可复现;pipeline 较稳但无字节级承诺 |
| **冷启动/资源底线** | 纯 CPU、无 GPU、内存以页 buffer 为闸;`--ocr` 首用 TTY 下确认拉 ~7MB | 首次需下模型(VLM/UniMERNet 等);高精度档需 GPU/大显存 |
| **引用闭环** | 每 chunk 带 page+bbox+heading breadcrumb,`locate(x,y)` 坐标反查,100% 覆盖 | 有坐标/版面,但无内置双向反查 API |
| **安全预检** | 隐藏文本过滤(防 prompt injection,标注可审计、绝不静默丢)+ zip-bomb/页数守卫 + 逐页复杂度画像 | 无对应内置 |
| **按页成本模型** | 数字页快路径不渲染像素;只有质量评分判定的难页路由到模型并按需渲染 | pipeline/VLM 对每页过模型 |
| **许可纯净** | **Apache-2.0**;无 veraPDF 代码(仅参考算法);模型均 Apache-2.0;两处 attributed tract 补丁 | "MinerU 开源许可"(基于 Apache-2.0 + 附加条件,曾为 AGPLv3);商用需读附加条款 |

## 3. 同名能力的"同名不同实现"

| 能力 | MinerU | docparse-rs |
|---|---|---|
| PDF 文本 | pypdfium2 / pdftext(C++ PDFium 系)+ 必要时 OCR | **自研内容流解释器**(graphics/text matrix 状态机),无 PDFium |
| 公式→LaTeX | MFR(UniMERNet 系)模型 | UniRec-0.1B(`--formula-model`,papers 0.874) |
| 表格→结构 | TableFormer/表模型 + VLM,输出 HTML | UniRec-0.1B(`--table-model`,合并格 span 语义;clean 表 0.810) |
| 版面/阅读顺序 | 自研版面模型(已弃 AGPL 的 doclayout-yolo/mfd-yolo);VLM 端到端 | DocLayout-YOLO / **PP-DocLayoutV2**(双后端,后者带原生阅读顺序);否则确定性 XY-cut |
| OCR | PaddleOCR 系,109 语种 | PP-OCRv6 tiny(默认)/ v4 回退,中英 |
| 整页理解 | MinerU2.5-Pro-1.2B VLM(核心卖点) | `--transcribe-model`(UniRec 整页转写,中英)/ 外接 VLM |
| 多后端 | pipeline / vlm-engine / hybrid-engine + http-client | 单一确定性核 + opt-in enhancer 边界 |

## 4. 选型建议(什么时候用哪个)

**选 MinerU,当:**
- 追求**最高解析精度**,难表/信息图/手写/扫描多,可接受 GPU + Python 环境;
- 需要**多语言**(非中英,尤其 RTL/小语种)或**图表→数据/VLM 图片理解**;
- 已有 GPU 集群、要多卡高并发批处理生产管线(mineru-router);
- 接入 Dify/RAGFlow/FastGPT 等现成 RAG 平台,或用国产芯片。

**选 docparse-rs,当:**
- 要**单二进制零依赖**落地(边缘/客户机/CI/serverless 冷启动),不想背 GPU 与 GB 级 Python 环境;
- 文档以 **born-digital**(数字版 PDF/Office)为主,要**极致吞吐 + 字节级可复现**;
- 给 **agent/RAG** 用,需 page+bbox 双向引用、MCP 直连、`locate()` 反查;
- 安全敏感(隐藏文本/zip-bomb 预检)、许可要纯 Apache-2.0;
- 中英为主,难页占比低、可接受用 opt-in 模型或外接 VLM 兜底质量。

**互补用法**:docparse-rs 走快路径 + 质量评分/路由筛出难页,只把难页交给 MinerU(或其 VLM 服务)——前者扛吞吐与确定性、后者补质量上限,正是 docparse-rs `Enhancer` 边界与 `--route-plan` 的设计意图。

## 5. 一句话结论

> MinerU 是"**精度与广度的天花板**"(VLM 路线,代价是重环境/GPU);docparse-rs 是"**部署与确定性的地板**"(纯 Rust 单二进制,代价是难版面天花板与语言面)。二者不是替代关系——born-digital 高吞吐确定性场景选 docparse-rs,难文档/多语言/高精度生产选 MinerU,大规模管线可让前者把难页路由给后者。

---

*依据:MinerU `tmp/refer/MinerU` README + `pyproject.toml`(3.3,2026-06-11);docparse-rs [status.md](../status.md) / [README](../../README.md) / [omnidocbench 记分牌](../testresults/2026-06-12-omnidocbench.md)。OmniDocBench 口径两边不同(MinerU 报 v1.6,我方为早期方法),分数仅作量级参考,不可逐点对齐。*
