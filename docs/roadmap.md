# 总体开发迭代计划 · Development Roadmap

> 本文是 docparse-rs 的**战略主计划**：长远愿景、大功能模块、分阶段路线。只写"做什么、为什么"，不写实现细节。
>
> - 近期可执行的解析器特性清单见 [iteration-guide.md §5](iteration-guide.md)（战术层，本文 P1 阶段的细化）。
> - 已完成工作的验证与设计回顾见 [phase-1-summary.md](phase-1-summary.md)。
> - 大格局背景（开源工具全景、平台架构、Rust 取舍）见 [refer/document-parsing-open-source-tools-research-2026.md](refer/document-parsing-open-source-tools-research-2026.md)。
> - 协作约定见 [../CLAUDE.md](../CLAUDE.md) 与 [../AI_AGENT_DEV_SPEC.md](../AI_AGENT_DEV_SPEC.md)。

---

## 1. 长远愿景

> 用 Rust 实现一个**完整、自洽的文档解析系统**：从文件接入到带位置的结构化内容（文本/版面/表格/阅读顺序 → 统一 IR → JSON/Markdown/Text/RAG 切块）**全链路在 Rust 内完成**。**对外**可供任意 Agent / RAG 系统直接调用；**对内**可灵活接入 OCR、大模型等外部服务增强难例。它是一个独立的系统/产品，而非某个更大平台的子组件。

四条不动摇的身份约束：

| 约束 | 含义 |
|---|---|
| **完整且自洽** | 核心解析全链路纯 Rust 自研；确定性路径**不依赖任何外部服务即可独立产出结果** |
| **面向 Agent** | 稳定版本化 IR + 坐标/溯源 + CLI/库/服务化接口；任意 agent 可消费，结果**可复现、可引用** |
| **AI 可插拔** | OCR / 大模型 / VLM 是**可选增强**，经统一插件边界灵活接入；主流程不被其绑定 |
| **纯 Rust · 不光栅化** | 无 JVM / C++；只解析内容流拿坐标、不渲染像素；单二进制易分发（边缘/内网/桌面/WASM） |

---

## 2. 战略定位（第一性原理）

调研报告是**参考与坐标系**（开源工具全景、能力维度、Rust 取舍），不是要照搬的架构。报告推荐"Rust 控制面 + Python/Java AI worker 作核心处理"的多语言平台；本项目的取舍不同：

> **docparse-rs 自身就是完整系统**——核心解析能力全部纯 Rust 自研、独立成立；OCR / 大模型是**可选外接增强**，用来补难例（扫描件、复杂表格、语义富化），而非系统赖以运转的核心。它从**内容流解释器 + 字体层**（ODL 委托给 veraPDF 的那一层）自底向上做起，正因为这是不可外包的根。

为什么这样定位：

- **agent 要的是确定、可复现、可引用**——确定性 Rust 路径天然满足；把它做完整就直接服务任意 agent。若核心依赖外部大模型，确定性与可控成本就丢了。
- **AI 作增强、不作底座**：多数数字文档无需任何模型即可高质量解析；模型只在质量检测判定为难例时**按页触发**，主流程不被 GPU / Python 依赖绑定。
- **语义层是最大奖品，增量自研**：表格 / 列表 / 标题分级是 veraPDF-wcag-algs 的等价物（数人年工程），从"先做有框表格"起步，**参考算法、独立实现、标注出处**，绝不复刻全量、绝不拷贝 GPL 代码。

增长次序：

```mermaid
flowchart LR
  A[确定性核心<br/>做完整·可独立交付] --> B[Agent 接入面<br/>稳定 IR / API / 溯源] --> C[AI 增强<br/>OCR/LLM 可插拔·补难例]
```

先把确定性核心做到能独立交付价值 → 打磨面向 agent 的接入面 → 再叠 AI 可插拔增强。**不在核心成立前先建编排机器。**

### 对标 Docling：赢 / 持平 / 不打

战略定位落到一个具名对手上才可证伪。Docling 是当前多格式通用解析器的事实首选（统一 `DoclingDocument`、PDF/Office/HTML 全覆盖、MIT、RAG 生态成熟，ODL 自测 benchmark 综合 **0.882**）。它的结构性弱点：**重依赖**（Python + 模型下载 + 冷启动）、**功能即代价**（OCR/表格/公式无法对所有页无条件开）、**模型许可复杂**（代码 MIT ≠ 模型 MIT）、**黑盒难溯源**。据此把"更好"分三档，**不笼统**：

| 战场 | 定位 | 为什么 |
|---|---|---|
| **数字原生文档**（有文本层/结构的 PDF/DOCX/HTML） | **要赢** | 更快、确定、可溯源、零依赖单二进制——Rust 快路径跑常见页天然优于神经版面分析 |
| **结构理解**（表格/列表/标题层级，born-digital） | **要持平** | 有规则线/有标签时可确定性求解，对照 wcag-algs 独立实现 |
| **多格式广度** | **要持平** | DOCX/HTML 有显式结构、ROI 最高先行；广度是 Docling 的首选理由，须够得着 |
| **扫描 OCR / 神经表格 / 公式 / 手写** | **短期不打** | 需模型进步，纯 Rust 不解决；经可插拔边界外接，按页路由降低命中频率与成本 |

> 一句话：**在数字原生文档上用"零依赖 + 确定 + 可溯源 + 单二进制"赢下部署/成本/可复现这三件 agent 真正在乎的事；结构与广度够到 Docling 的 born-digital 水平；模型重的长尾外接而非塞进主流程。** 这与 §1 四条身份约束同构——确定性核心赢、AI 增强不绑定。每条战场如何量化见 §6 记分牌。

---

## 3. 生产架构：路由 + 质量回退 + 统一 IR 脊梁

调研报告 §6 推荐的"生产方案"本质是一套**编排架构**——文件路由 + 质量回退 + 统一规范化输出 + 质量评分，用来协调多个**独立工具**（Docling 统管、ODL 跑数字 PDF、MinerU/PaddleOCR 跑扫描、Tika 兜底长尾）。docparse-rs **不是编排这些工具，而是把这套模式内化成一个 Rust 系统**：把每家的看家本领吸收进对应层——确定性能力自研、模型能力外接。

### 各家优点 → docparse-rs 落点

| 报告里的工具 | 真正的看家本领 | docparse-rs 落点 | 形态 |
|---|---|---|---|
| **OpenDataLoader** | 数字 PDF 快确定性结构抽取 + 元素坐标 + 隐藏文本过滤 | PDF 确定性后端（模块 2） | 自研·核心 |
| **Docling** | 统一文档模型 + 多格式 + RAG 生态 | 统一 IR（模块 1）+ 多格式后端（模块 5）+ 输出/RAG（模块 6） | 自研·核心 |
| **MarkItDown** | 轻量低依赖、markdown-first 的简单入口 | 原生轻量输出 + 简单格式快路径（模块 5/6） | 自研·核心 |
| **MinerU / PaddleOCR** | 扫描 / 中文 / 公式 / 复杂表格（靠模型） | 外部 AI 服务接入（模块 8） | 外接·可选 |
| **Tika** | 长尾 / 旧格式检测 + 元数据 | 格式检测（模块 9）+ 长尾回退适配 | 自研 + 外接 |

> 关键：docparse-rs **自己就是**那个"统一模型"，而非报告里"以 Docling 作统一模型"；确定性的能力**内化自研**，模型重的能力**可插拔外接**。

### 生产流水线（一份文档怎么走完）

```mermaid
flowchart TB
  IN[文件接入] --> PROF[复杂度画像<br/>格式 + 页级路由]
  PROF -->|普通数字页| FAST[确定性 Rust 快路径<br/>PDF / OOXML / HTML 后端]
  FAST --> Q{质量评分<br/>覆盖率/乱码/阅读顺序/表格闭合}
  Q -->|达标| IR[(统一 IR + provenance)]
  Q -->|不达标| FB[按页回退]
  PROF -->|疑难页直送| FB
  FB -. 可插拔 .-> AI[外接 OCR / LLM / VLM]
  AI -->|结果归一| IR
  IR --> OUT[输出 / 切块 / 引用定位]
```

便宜确定的先上，只有质量评分判定为难例才**按页**升级到外接模型——多数页不碰模型，成本可控（报告 §7）。所有路径的结果都归一到**同一份带 provenance 的 IR**，下游只面对一种 schema。

### 脊梁：统一规范化 IR + 质量评分

报告这条推荐的落脚词是"建立统一的规范化输出和质量评分"——它不是附属，而是整套架构成立的**脊梁**，也是"集合各家优点"在技术上的**真正核心**：

| 脊梁 | 缺了它就 | 对应模块 |
|---|---|---|
| **规范化 IR**（带 provenance） | 无法归一/融合多来源结果，无法对下游屏蔽差异 | 模块 1 |
| **质量评分** | 路由不知走哪条、回退不知何时升级、生产不知能否放行 | 模块 7 |

**工程次序**：IR + provenance + 质量评分这套**契约**便宜且高杠杆，应**早定**（所有解析路径都要路由在它上面）；路由 / 回退 / 插件那套**机器**晚建（P3）。**先定契约，再造机器。**

---

## 4. 能力分层与大功能模块

```mermaid
flowchart TB
  subgraph P[接入与治理 · 远期平台层]
    SEC[安全预检 / 隐藏文本过滤]
    PROF[文档复杂度画像 / 页级路由]
  end
  subgraph D[确定性数据面 · 纯 Rust 核心 · 当前主战场]
    PDF[PDF 后端<br/>内容流解释器 + 字体层]
    FMT[多格式后端<br/>DOCX/PPTX/XLSX/HTML]
    LAY[版面与阅读顺序]
    SEM[语义结构层<br/>表格/列表/标题]
  end
  subgraph I[统一 IR · 格式无关 · 长期接口]
    DOC[Document/Page/Block/Span + provenance]
  end
  subgraph X[AI 增强 · 可插拔外接 · 按页触发]
    AI[OCR / Layout / VLM 服务]
  end
  subgraph O[输出与 RAG]
    QC[质量检测与回退]
    OUT[JSON/MD/Text + 结构化切块 + 引用定位]
  end
  P --> D --> I --> O
  PROF -. 复杂页 .-> X -. 结果归一 .-> I
```

| # | 大模块 | 职责（做什么 / 为什么） | 当前状态 | 算法/架构参照 |
|---|---|---|---|---|
| 1 | **统一 IR** | 格式无关数据模型 + 版本化 schema + provenance（解析器/版本/置信度）。系统最重要的长期接口 | 基础 IR 已有，未版本化、无 provenance | 报告 §6 Document IR / `document-ir` |
| 2 | **PDF 确定性后端** | 内容流解释器 + 字体层 + 精确坐标，纯 Rust 复刻 ODL 快路径 | 骨架可用；文本保真度待提升 | veraPDF-parser（`pd.font`/CMap） |
| 3 | **版面与阅读顺序** | XY-cut 多栏排序、页眉页脚/水印识别、段落聚合 | XY-cut 已有；页眉页脚/段落待做 | ODL XY-Cut++ |
| 4 | **语义结构层** | 表格识别、列表层级、标题分级——把 chunk 升维成结构。最大、最有价值、最难 | 未起步 | veraPDF-wcag-algs（`TableBorderConsumer`/`ClusterTableConsumer`） |
| 5 | **多格式后端** | DOCX/PPTX/XLSX/HTML，各 `impl DocumentParser` 汇入同一 IR | trait 已留挂载点，未实现 | 报告 §10.6 `parser-ooxml`/`parser-web` |
| 6 | **输出与 RAG** | 序列化 + 结构化切块 + chunk↔页码/bbox 双向引用定位 | JSON/MD/Text 已有；切块/溯源待做 | 报告 §10.6 `document-export`/`document-chunk` |
| 7 | **质量检测与回退** | 覆盖率/乱码率/阅读顺序异常评分，决定是否触发外接复核 | 未起步 | 报告 §8 / §10.8 |
| 8 | **外部 AI 服务接入** | OCR / 大模型 / VLM 作**可选增强**：版本化 capability（格式/元素/语言/设备/版本）+ 统一边界，按页触发补难例，主流程可无之独立运行 | 未起步 | 报告 §10.5–§10.6 `parser-plugins` |
| 9 | **安全预检与画像** | 恶意对象/ZIP bomb 防护、隐藏文本过滤（防 prompt injection）、复杂度路由 | 未起步 | ODL 隐藏文本过滤 / 报告 §10.2–§10.3 |
| 10 | **Agent 接入面与运行时** | 面向 agent 的消费接口：CLI（已有）/库/服务化（REST/gRPC/MCP）+ 调度/优先级队列/阶段缓存/可观测 | CLI 已有 | 报告 §10.6 `document-runtime`/`server` |

> 模块即未来的 crate 边界，但**按需拆分**——不为架构整齐提前建空 crate（反 MVP，见 AI_AGENT_DEV_SPEC §3）。

---

## 5. 分阶段路线图

> 阶段以"独立有用户价值"为拆分依据，非按工时。每阶段进入前补 plan、完成后补 devlog（SDD 流程见 AI_AGENT_DEV_SPEC §4）。

| 阶段 | 主题 | 目标（用户视角） | 牵头大模块 | 状态 |
|---|---|---|---|---|
| **P0** | 纯 Rust PDF 抽取骨架 | 数字 PDF 端到端读出带坐标文本，三种输出 | 1,2,3 | ✅ 已完成 |
| **P1** | 文本保真与版面可读 | 数字 PDF 文本接近无损；输出按段落/表格可读，而非逐行 | 2,3 | ✅ 已完成（M1–M3）|
| **P2** | 语义结构 + 多格式 | 输出是**结构**（表格/列表/标题层级）而非纯文本流；覆盖 DOCX/HTML | 4,5 | ✅ 基本完成（M4 有框表格 + M5 DOCX/HTML；表格四检测器 bordered→ruled→**cluster**→borderless，确定性检出达 ODL 量级；多级表头/无框结构属 N3 神经域）|
| **P3** | Agent 接入与 AI 增强 | 成为任意 agent 可直接调用的**完整系统**：稳定 IR 协议、引用定位、服务化接口（REST/MCP）；难例可插拔接入 OCR/LLM；质量回退、安全预检 | 1,6,7,8,9,10 | 🚧 进行中（M2 IR 版本化/provenance、M6 切块溯源、M7 质量路由+可插拔边界 ✅；服务化接口、安全预检、真实 enhancer 待做）|
| **P4** | 选择性模型内嵌 | 稳定小模型（页面分类/方向/轻量 OCR）以 ONNX 内嵌提速；大 VLM 仍外接 | 8 | ⬜ 远期·可选 |

**各阶段大致内容（高层，细节见对应 plan）：**

- **P1** — 标准 14 字体度量、简单字体 Encoding/Differences、字间距操作符（修文本保真）；段落聚合、Markdown 表格雏形（修可读性）。细化清单见 [iteration-guide.md §5](iteration-guide.md)。
- **P2** — 语义层从"先做有框表格检测"起步，再到列表层级、标题分级；并行验证多格式后端（DOCX 先行），坐标按 PDF 约定折算。
- **P3** — IR 版本化 + provenance；结构化切块与 chunk↔bbox 溯源；面向 agent 的服务化接口（REST/gRPC/MCP）；质量评分与失败页可插拔回退到外接 OCR/LLM；版本化插件协议；安全预检与复杂度画像/路由。
- **P4** — 把已稳定的小模型转 ONNX 用 `ort`/Candle 在 Rust worker 内推理，评估纯 Rust/Metal 部署；重型、迭代快的模型保持外部服务。

> **可执行里程碑（M1–M7）与依赖次序**见 [plans/beating-docling.md](plans/beating-docling.md)：它把上面 P0–P4 按竞争杠杆细化为带验收的里程碑，并据 §3"先定契约"把 **IR 脊梁（版本化 + provenance）从 P3 提前到紧跟 P1**——便宜且解锁所有溯源/切块/路由。

---

## 6. 记分牌：怎么证明"更好"（可证伪）

不立指标的"更好"是口号。采用两类记分牌，每个阶段/里程碑完成都回填数字（落 `docs/testresults/`）。

**质量记分牌（对标 Docling，同尺）**——复用 ODL benchmark 三项指标，只在 **born-digital 子集**上同台，扫描件**显式弃权**并记录（那不是我们的战场，§2）：

- **NID** 阅读顺序 · **TEDS** 表格结构 · **MHS** 标题层级。目标：born-digital 子集三项不低于 Docling（其综合 0.882）。
- benchmark **不可拼榜**（报告 §5.2）：只在自建可比子集比，不把不同项目的单分拼排行。

> **现状（2026-06-10，去 RTL born-digital LTR）**：vs ODL NID **0.722** / MHS **0.614**；vs Docling NID **0.763** / MHS **0.625**。**clean 子集已达 docling/ODL 水平**（`multi_page` 0.984、`code`/`picture` 0.99、`redp5110` 0.972、`2305` 0.921）。聚合被两类**确定性天花板**拖低——CJK 复杂版面（`skipped_*`/`normal_4pages`）与最难双栏论文首页（`2203`/`2206`）——属 N3 enhancer/版面模型领域。⚠️ 关键纠偏：曾有两个 GT 提取器漏算列表文本，**一直低估** NID（修复后我方产出未变即 +0.07）。详见 [devlogs/2026-06-10-session-summary](devlogs/2026-06-10-session-summary.md)。

**差异化记分牌（Docling 结构上无法同台）**——这才是"更好"的硬证据：

| 指标 | 测法 | 目标 |
|---|---|---|
| 二进制体积 / 运行时依赖 | `ls -la target/release/docparse` | 单文件 < 20MB，运行时依赖 0 |
| 冷启动到首字节 | `time docparse small.pdf` | < 100ms（无模型加载） |
| 确定性 | 同文件跑 100 次 diff | 逐字节一致 |
| 吞吐（born-digital） | 页/秒 vs Docling 标准管线 | 显著领先（目标 ≥10×，待测） |
| 引用可定位率 | 每个输出 chunk 能否回指 bbox | 100% chunk 带 provenance |

> 记分牌即验收门：没有数字的阶段不算完成（SDD §4）。可执行里程碑与逐项验收见 [plans/beating-docling.md](plans/beating-docling.md)。

---

## 7. 关键原则与边界

| 维度 | 立场 |
|---|---|
| 第一性原理 | 价值 = **完整确定性核心 + 稳定 agent 接口 + 可插拔 AI**：核心解析自研到独立成立，AI 只作增强外接，确定性与可控成本不丢 |
| 明确**不做** | 训练模型；把重型 VLM/OCR 硬编进主流程；一次性复刻 wcag-algs 全量。AI 经可插拔边界**外接**，核心解析**完整自研** |
| 许可边界 | 参考 veraPDF/ODL **算法**、独立实现并标注出处；本项目 Apache-2.0，**不引入 GPL 代码**（详见 [../CLAUDE.md §5](../CLAUDE.md)） |
| 反 MVP / 分阶段 | 主路径错误处理/测试/文档同 PR 铺到位；只有"上下游未就绪 / 单阶段独立有价值 / 需 spike"才真分阶段（AI_AGENT_DEV_SPEC §3） |
| 质量底线 | 坐标/IR 不变量恒守；字体/解码改动必跨样例回归；近似必标注；零 warning（[../CLAUDE.md §3–4](../CLAUDE.md)） |

---

## 8. 怎么挑下一步

```mermaid
flowchart TD
  Q{想提升什么?} -->|文本正确率| A[P1: 字体度量/编码]
  Q -->|输出可读性| B[P1: 段落聚合 / MD 表格]
  Q -->|进入结构理解| C[P2: 表格检测起步<br/>对照 wcag-algs]
  Q -->|拓格式| D[P2: DOCX/HTML 后端]
  Q -->|供 agent/RAG 调用| E[P3: 服务化接口 + 切块溯源 + AI 可插拔增强]
```

进度（2026-06-09）：**近期执行层 M1–M7 全部完成**——M1 文本保真、M2 IR 脊梁、M3 版面可读、M4 有框表格、M5 多格式（PDF/DOCX/HTML）、M6 RAG 切块+chunk↔bbox 引用、M7 质量路由+外接边界。41 单测、clippy 零 warning、确定性逐字节。**后续为远期**：模块 9 安全预检、模块 10 服务化（REST/gRPC/MCP）、P4 小模型 ONNX、真实 enhancer 接入、born-digital 评测集回填 NID/TEDS/MHS 与 Docling 同台（§6 记分牌）。里程碑细节见 [plans/beating-docling.md](plans/beating-docling.md)，devlog 见 [devlogs/](devlogs/)。
