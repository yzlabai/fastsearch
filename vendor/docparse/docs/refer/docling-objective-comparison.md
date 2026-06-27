# 客观对比 · docparse-rs vs Docling(2026-06-10)

> 与 [testresults/benchmark-roundup](../testresults/2026-06-10-benchmark-roundup.md) 互补:那篇以我方记分牌为主轴;**本篇刻意把 Docling 的优势摆全**,供选型与对外表述时保持诚实。能力面依据 Docling 本地源码(`tmp/refer/docling`)核实,非凭印象。
>
> 一句话:这是**两个设计点不同的系统**,不存在单向"更好"——Docling 用模型换质量上限与广度,docparse-rs 用确定性换部署、速度与可复现。

## 1. Docling 明显占优的轴(诚实清单)

| 轴 | 事实 | 我方差距 |
|---|---|---|
| **格式广度** | 15+ 后端:PDF(三代解析引擎)/DOCX/**PPTX/XLSX**/HTML/Markdown/AsciiDoc/LaTeX/CSV/邮件/图片/METS/WebVTT 字幕… | 我方 3 种(PDF/DOCX/HTML),PPTX/XLSX 未做 |
| **难版面质量上限** | 神经版面模型(DocLayNet 系)对 CJK 信息图/旋转页/海报式排版有效——正是我方 0.12–0.22 的那批文档 | 确定性 XY-cut 的天花板,已实验证明不可强攻 |
| **表格结构精度** | TableFormer 真正求解**多级表头/合并单元格/无线表**,输出完整 cell 拓扑 | 我方四检测器只到"检出+简单网格",TEDS 0.098/0.187 即此差距的度量 |
| **内容增强** | 公式→LaTeX、代码块识别、图片分类/描述(VLM)、图表数据抽取、整页 VLM 管线(SmolDocling) | 我方当前全部没有;已规划 [Phase 4 G8](../plans/closing-docling-gaps.md) 三层补齐(确定性代码块→HTTP 外接→纯 Rust VLM spike) |
| **OCR 广度** | 7 引擎可换(EasyOCR/Tesseract×2/RapidOCR/ocrmac/远程)、80+ 语言、方向检测、**区域级 OCR**(混合页只 OCR 位图区) | 我方单模型组(中英)、无方向分类、无 JBIG2/CCITT、整页粒度、Form 流不解释 |
| **RTL** | 支持 | 我方明确不支持(0 分记录在案) |
| **生态与成熟度** | LangChain/LlamaIndex 等官方集成、`DoclingDocument` 事实标准、IBM 开源/LF AI & Data 托管、大社区、海量真实语料锤炼;tokenizer 感知的 HybridChunker | 我方 0 集成、0 社区;鲁棒性仅在 ~20 份样例 + 三件套上验证,长尾(加密 PDF/XFA/损坏文件)未经大规模检验 |
| **GPU 路径** | CUDA/MPS 加速,神经管线吞吐可大幅拉升 | 我方 CPU-only(tract),无 GPU 计划 |

## 2. docparse-rs 明显占优的轴(实测)

| 轴 | 实测 | Docling 对应 |
|---|---|---|
| 部署 | 19.1MB 单二进制、零运行时依赖、OCR 模型可选外部 16MB | Python 环境 + 模型下载(GB 级缓存)、冷启动需加载模型 |
| born-digital 速度 | <10ms 预热解析、700 页/s(CPU) | CPU 神经管线 ~1 页/s 量级(其公开数据;GPU 可改善但仍有模型开销) |
| 确定性 | 同输入**逐字节一致**,且跨 CLI/MCP/REST 接口一致(含 OCR 路径),有测试钉死 | 神经管线通常可复现但无字节级承诺,跨环境/版本漂移 |
| 安全预检 | 隐藏文本过滤(防 prompt injection,标注可审计)+ zip-bomb/页数守卫内置 | 无对应内置(ODL 有,Docling 没有) |
| Agent 原生 | MCP stdio server 内置,`locate(x,y)` 坐标反查闭环 | 需自行包装服务;无反查 |
| 按页成本模型 | 数字页零模型零加载,只有质量评分判定的难页触发推理 | 全管线过模型(版面模型对每页运行) |

## 3. 接近 / 各有侧重(别夸大我方)

- **元素级溯源**:**双方都有**——Docling 的 item 带 page+bbox provenance,并非"无引用";我方的增量在 `locate` 反查闭环、100% 覆盖保证与 chunk 粒度的 source 标签。表述时不应说"Docling 没有引用"。
- **许可**:双方代码均宽松(Docling MIT / 我方 Apache-2.0);模型许可双方都是"大多宽松但须逐个核对"(我方 PP-OCR Apache-2.0;Docling 各模型不一)。
- **质量分数的口径**:我方记分牌是**与 Docling 输出的一致度**——这个度量天然把 Docling 当"正确答案",我方 0.833 应读作"与它 83% 同构",**不能**读作"我方 83 分、它 100 分",也不能反向证明我方更准。真正分高下需要人工真值(双方都没在本仓库度量)。

## 4. 选型矩阵

| 场景 | 选 | 为什么 |
|---|---|---|
| 边缘/内网/桌面嵌入、Rust 栈、冷启动敏感 | **docparse-rs** | 单二进制、零依赖、<10ms |
| Agent 工作流要确定可复现可引用、大批量 born-digital | **docparse-rs** | 字节级确定 + 引用闭环 + 700 页/s |
| 安全敏感入口(用户上传→LLM) | **docparse-rs** | 隐藏文本过滤 + 资源守卫内置 |
| 复杂版面/扫描混合语料、表结构要全保真 | **Docling** | 神经版面 + TableFormer 是质量上限 |
| 格式杂(PPTX/XLSX/邮件/图片)、要公式/图表/图片理解 | **Docling** | 广度与增强面无可替代 |
| 已在 Python ML 生态、要 LangChain/LlamaIndex 现成接入 | **Docling** | 生态成熟度 |
| 两者混合:快路径自托管 + 难例升级 | **docparse-rs 路由 + 外接** | 我方 `Enhancer` 边界/ODL hybrid 同款思路;VLM 经 OpenAI 兼容服务(vLLM 等)接入(Phase 4 G8b) |

## 5. 给本仓库文档的措辞约束(回写)

1. 不说"比 Docling 好",说"在 X 轴上实测领先、在 Y 轴上明确不打"(roadmap §2 战场框架);
2. 引用对比表时必须带"一致度非准确率"口径;
3. "Docling 引用非全链路"的旧表述**停用**(见 §3);
4. 吞吐对比注明"对方为公开宣称值/典型值,非同机同台"。
