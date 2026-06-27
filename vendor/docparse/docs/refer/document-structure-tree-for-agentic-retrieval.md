# 文档结构树抽取 · 面向 Agentic 检索的调研报告

> 调研问题：对长文档（论文、报告、书籍）解析时，如何抽取**逻辑文档结构**并形成**树状结构**，使 agent 能像翻目录、钻章节那样**导航式检索**，而非只拿到一堆扁平 chunk。
>
> 范围：① docparse-rs 现状与差距；② 结构树**抽取**技术全景；③ 树**表示/schema**；④ 面向 **agentic 检索**的树用法；⑤ 对 docparse-rs 的落地建议（分确定性核心 / 可选增强 / 外接三层，遵 roadmap §1 身份约束）。
>
> 战略坐标系见 [roadmap.md](../roadmap.md)、工具全景见 [document-parsing-open-source-tools-research-2026.md](document-parsing-open-source-tools-research-2026.md)。本文是**技术参考**，不是已定方案；落地前另起 plan。

---

## 1. 执行摘要

**核心判断**：docparse-rs 已经**抽到了构建结构树所需的几乎所有原料**，但在 IR/输出层把它们**拍扁**了——结构树是"差最后一公里的免费奖品"，而非需要新模型的大工程。

三个事实：

1. **标题已分级**：确定性版面层已对标题做字号分层（`assign_heading_levels`，1–3 级）与标签分级（Tagged PDF `H1..H6`）——[core/layout.rs:777-793](../../crates/docparse-core/src/layout.rs#L777-L793)。
2. **层级却被丢弃**：① chunk 的"标题面包屑"用的是**字号栈**而非已算出的 `level`（[core/chunk.rs:78-235](../../crates/docparse-core/src/chunk.rs#L78-L235)）；② Tagged PDF 的 `StructTreeRoot` 被**读取后只留每-MCID 的扁平 role 标签**，父子层级被显式弃用（[pdf/structure.rs](../../crates/docparse-pdf/src/structure.rs)）；③ IR 是严格**逐页扁平** `Document→Pages→Elements`，没有任何 `Section`/跨页结构节点（[core/ir.rs:239-248](../../crates/docparse-core/src/ir.rs#L239-L248)）；④ **PDF Outlines（书签）完全没读**。
3. **agent 拿不到导航面**：MCP/REST 只能"给我全文 chunks"，不能"列目录 → 取第 3.2 节 → 展开其子树"。

**建议方向**（与"确定性核心赢、AI 增强不绑定"同构）：

- **确定性核心（信封内、零模型、最高 ROI）**：把已有标题级别 + Tagged StructTree 层级 + PDF Outlines + 字号栈**归一成一棵 `Section` 树**，挂进 IR；chunk 改用**真实层级**算 `heading_path`；新增**树导航接口**（MCP/REST：`outline` / `get_section` / `subtree`）。这一步独立可交付，且正是 agentic 检索最缺的。
- **可选增强**：ToC（目录页）解析、版面模型的层级深度分类（补无标签、字号不单调的难文档）。
- **外接（信封外）**：RAPTOR 式"递归聚类+摘要"语义树、VLM 结构理解——需 LLM，按可插拔边界外接，不进主流程。

对标：Docling 有 `DoclingDocument` body 树、llmsherpa 有 block 树 + 标题下传、GROBID 出 TEI 树——但**全是 Python/Java + 模型 + 起服务**；unstructured 用同级 `#` 不反映层级（公认弱点）。docparse-rs 的差异化机会＝**确定 + 可溯源（每节点带 bbox/page）+ 零依赖单二进制 + agent 可导航**的结构树（相似开源产品深入横评见 §8）。

---

## 2. 问题定义：什么是"好的文档结构树"

一棵对 agentic 检索有用的结构树，应满足：

| 维度 | 含义 | 为什么 agent 在乎 |
|---|---|---|
| **层级正确** | 节点嵌套反映真实逻辑（3.2 在 3 下、在 2 之外） | 决定"钻取"路径是否走对 |
| **可变深度** | 不固定 3 层；书籍可 卷/章/节/小节 多级 | 长文档天然多级 |
| **内容归属** | 每节点挂其正文/表/图，且**只挂直属内容**（不重复继承到子节点） | 决定检索粒度与去重 |
| **可溯源** | 每节点带 page+bbox（标题位置）与内容位置 | 答案可引用、可定位（roadmap §6 引用率 100%） |
| **可导航** | 能"列子节点 / 取某节点 / 取子树 / 拿面包屑" | agentic 检索的核心交互 |
| **稳定可复现** | 同文档同树（确定性） | agent 结果可复现（roadmap §1 身份约束） |

**反模式**：unstructured 的"所有标题同级 `#`"——丢了嵌套，等于没树（[Procycons 2025 benchmark](https://procycons.com/en/blogs/pdf-data-extraction-benchmark/)）。

---

## 3. docparse-rs 现状与差距（已抽到什么 / 丢了什么）

> 据代码核查（file:line 为准）。结论：**原料齐全，差归一与树化。**

| 能力 | 现状 | 证据 |
|---|---|---|
| 标题**检测** | ✅ 字号>正文×1.25、全粗短行、编号/全大写文本型、Tagged `H1..H6` 覆盖几何 | [layout.rs:628-723](../../crates/docparse-core/src/layout.rs#L628-L723) |
| 标题**分级** | ✅ 无标签按全文字号档位映射 1–3 级（深档封顶 3）；标签直接取级 | [layout.rs:777-793](../../crates/docparse-core/src/layout.rs#L777-L793) |
| chunk **面包屑** | ⚠️ 用**字号栈**建，**不读已算的 `level`**——字号非单调下降时会错 | [chunk.rs:78-235](../../crates/docparse-core/src/chunk.rs#L78-L235) |
| chunk schema | `{id, kind(Heading/Paragraph/Table/Code/ListItem), text, page, bbox, heading_path: Vec<String>, char_len}` | [chunk.rs:26-40](../../crates/docparse-core/src/chunk.rs#L26-L40) |
| Tagged PDF 结构树 | ⚠️ 读 `StructTreeRoot`、算遍历序，但**层级被弃**，只留每-MCID 扁平 role（创作序≠视觉序，故意不当读序用） | [structure.rs](../../crates/docparse-pdf/src/structure.rs)、[ir.rs:98-102](../../crates/docparse-core/src/ir.rs#L98-L102) |
| 阅读组 `group` | ✅ 仅**页内**宏序（版面区域/PPV2 原生序 → XY-cut within），无跨页/章节组 | [ir.rs:91-97](../../crates/docparse-core/src/ir.rs#L91-L97) |
| **文档级结构节点** | ❌ 无 `Section`/`Outline`/树；IR 严格逐页扁平 | [ir.rs:239-248](../../crates/docparse-core/src/ir.rs#L239-L248) |
| **PDF Outlines/书签** | ❌ 完全未读（`/Outlines` 是现成的作者层级，长文档/书籍常带） | — |
| **ToC 目录页** | ❌ 未解析 | — |
| Markdown 层级 | ✅ 按 `block.level` 渲染 `#/##/###`（已比 unstructured 强） | [output.rs:110-117](../../crates/docparse-core/src/output.rs#L110-L117) |

**一句话差距**：标题"是什么、第几级"已知，但从未**连成树**、未喂给 chunk/接口；两个**现成的作者层级源**（Tagged StructTree、Outlines）一个被拍扁、一个没读。

---

## 4. 技术全景 A：结构树**抽取**

抽取层级的信号源由"可靠性 / 成本"排序——**确定性优先**正是 docparse-rs 的定位。

### 4.1 显式来源（确定、零模型）——最该先吃

1. **Tagged PDF 逻辑结构树（StructTreeRoot）**：作者声明的 `Document→Part→Sect→H1/P/...` 真·树，含父子。我们**已经在读**，只是丢了层级。**直接复用遍历，保留父子，即得树**——零新解析。注意 veraPDF 经验：标签树按创作序、不能直接当**视觉读序**用（structure.rs 已规避），但**层级嵌套**是可靠的。
2. **PDF Outlines / 书签（`/Outlines`）**：作者给的目录树，每项带标题 + 目标页/位置。长文档/书籍命中率高。难点：outline 文字与正文标题**略有出入**（排版/OCR），需归一化模糊匹配把 outline 项**锚定到正文标题**（[HiPS 2025](https://arxiv.org/html/2509.00909v1)、[ToC ICDAR2013](https://clgiles.ist.psu.edu/pubs/ICDAR2013-ToC.pdf) 同款挑战）。
3. **HTML/DOCX 原生标题**：`<h1..h6>` / Word 大纲级别——多格式后端可直接给层级（[VLDB headings hierarchy](https://www.vldb.org/pvldb/vol8/p1606-manabe.pdf)）。

> **杠杆**：①②对论文/报告/书籍覆盖面大且**完全确定**，应作为树的**首选层级源**，几何字号仅在缺失时兜底。

### 4.2 启发式（确定、几何）——兜底主力

无标签/无书签时，从**视觉特征**推层级：字号、字重、编号模式（`1 / 1.1 / 1.1.1`）、行距/缩进/段前距。我们已做到"分 1–3 级"，下一步是**栈式建树**：维护"当前祖先栈"，遇更高级标题出栈到合适深度、入栈、把后续内容挂到栈顶节点。这正是 chunk.rs 字号栈的**升级版**——把"栈"从"只为算面包屑"变成"建持久树 + 用真实 level"。

代表：Marker、PDFstructure 的字号/间距启发式（[Procycons](https://procycons.com/en/blogs/pdf-data-extraction-benchmark/)）。**局限**：字号非单调（如小标题加粗但同字号）、跨栏、扫描件——需 4.3/4.4 补。

### 4.3 学习式（模型，可选增强）

- **GROBID**：面向**科学论文**的事实标准。级联**序列标注**模型把 PDF 重构成 **TEI/XML** 层级（标题/作者/摘要/章节/参考文献），按文档不同层级分别训练（[GROBID Principles](https://grobid.readthedocs.io/en/latest/Principles/)、[CORE+GROBID 处理 3400 万篇](https://blog.core.ac.uk/2023/07/17/core-grobid-structured-text-from-34-million-scientific-documents-and-counting/)）。
- **版面/深度分类**：先识别物理块 + 定读序，再用模型把每块分到 `main-text / section / subsection / subsubsection` **绝对层级深度**（[Variable-Depth Logical Hierarchy, arXiv:2105.09297](https://arxiv.org/pdf/2105.09297)）。docparse-rs 已有 PP-DocLayoutV2（25 类含标题层级语义 + 原生读序），是**天然接入点**。
- **HiPS（textbook 分层）**：归纳出四类法——ToC 法 / 版面法 / LLM 法 / 学习法，并指出各自边界（[arXiv:2509.00909](https://arxiv.org/html/2509.00909v1)）。

### 4.4 LLM/VLM 式（外接，信封外）

把候选标题行喂指令模型（GPT/Llama），判级 + 剔非标题（[HiPS LLM 法]）。准但**重、不确定、要服务**——按 roadmap 属可插拔外接，不进确定性主流程。

---

## 5. 技术全景 B：树的**表示 / schema**

业界已有成熟的"文档树"数据模型，给 docparse-rs IR 设计直接参考：

| 模型 | 形态 | 关键点 | 借鉴 |
|---|---|---|---|
| **Docling `DoclingDocument`** | `body`（正文树根）+ ~~`furniture`~~（页眉页脚，1.8.0 起改 `content_layer` 标在元素上）+ `groups`（容器）；`NodeItem`/`GroupItem` 经 **JSON pointer** 引父子 | 单一文档树 + 容器节点 + **节点间引用**而非深嵌套 | IR 加 `Section` 节点 + 父/子引用；用 `content_layer` 思路把页眉页脚标出而非删 | [Docling 文档模型](https://docling-project.github.io/docling/concepts/docling_document/)、[schema JSON](https://github.com/docling-project/docling-core/blob/main/docs/DoclingDocument.json)、[arXiv:2501.17887](https://arxiv.org/html/2501.17887v1) |
| **GROBID TEI** | `<teiHeader>` + `<body><div><head>…` 嵌套 | 论文语义命名（abstract/section/ref） | 论文域语义标签可映射 | [GROBID](https://grobid.readthedocs.io/en/latest/Introduction/) |
| **JATS / DocBook** | 出版业 XML 标准，深层嵌套 sec | 标准化、可互转 | 导出兼容目标（已在 roadmap 待办） | — |

**给 docparse-rs 的 IR 树形（草案，待 plan 定稿）**：在现有逐页扁平 IR **之上叠加**一层文档树，不破坏既有契约——

```
Document
 ├─ pages: [...]                  // 既有：逐页扁平 Element（保留，向后兼容）
 └─ outline: Section (root)       // 新增：文档结构树
      ├─ title: "3 Methods"
      ├─ level: 2
      ├─ page / bbox              // 标题自身位置（可溯源）
      ├─ content_refs: [元素/区域引用]   // 直属正文/表/图（不含子节点的）
      └─ children: [Section, ...]        // 子节
```

设计要点（吸收 Docling 教训）：① 节点**引用**页内元素（id/bbox）而非搬运副本——单一真源、IR 不膨胀；② `content_refs` 只挂**直属**内容，检索时再决定是否合并子树（见 §6 auto-merging）；③ 页眉页脚/水印标 `furniture`，不混进 body 树；④ schema 版本化 + provenance（每节点可标来源：`outline`/`structtree`/`heuristic`/`layout-model`），延续 IR 0.7.0 的可审计风格。

---

## 6. 技术全景 C：面向 **Agentic 检索**的树用法

有了树，检索范式从"embed 一堆扁平 chunk"升级为"**结构感知 + 可导航**"。按"是否需 LLM/embedding"分档（与确定性定位对齐）：

### 6.1 结构感知切块（确定、信封内）——立即可得

- **Section-aware chunking**：按**节边界**切，而非定长滑窗——一个 chunk 不跨章节、不腰斩表格。我们的 chunk 已按块聚合，叠上真实树即"节内切块"。
- **面包屑上下文增强**：每 chunk 前置 `卷 > 章 > 节 > 小节` 路径（"contextual headers"）——显著提升 embedding 命中与 LLM 理解。我们**已有 `heading_path`**，只需改成走**真实树**（修 §3 的字号栈 bug）。
- **parent-document / small-to-big / auto-merging**：用小 chunk 检索、命中后**上溯父节点**返回完整上下文（LangChain Parent-Document Retriever、LlamaIndex Auto-Merging Retriever）。树的父子引用是其**直接前提**——我们补了树即解锁，且**纯确定性**（检索器侧用，解析侧零模型）。

### 6.2 分层检索接口（确定、最契合 agentic）——差异化重点

让 agent **像人翻书**：先 `列出目录/顶层节点` → `读某节摘要/标题` → `钻取子节` → `读叶子 chunk`。这正是 **A-RAG** 的"分层检索接口 + agent loop"范式（[arXiv:2602.03442](https://arxiv.org/pdf/2602.03442)）与 **TreeRAG**（[ACL 2025 Findings](https://aclanthology.org/2025.findings-acl.20.pdf)）的核心。

> **对 docparse-rs 的意义**：这层**不需要任何模型**——只是把结构树**暴露成 MCP tools / REST 路由**：`outline(doc)` 列树、`get_section(path|id)` 取节、`subtree(id)` 取子树、`breadcrumb(chunk)` 拿路径。**确定、可溯源、零依赖**，与现有四接口同构（[agent-integration.md](../agent-integration.md)）。这是把"结构树"变成"agent 能用的检索面"的**最短路径，也是最大差异化**。

### 6.3 语义聚合树（需 LLM/embedding，外接）

- **RAPTOR**：对 chunk **递归聚类 + 摘要**，自底向上建树，高层是主题概览、底层是事实，检索可在**不同抽象层**命中（[RAPTOR via RAGFlow](https://ragflow.io/docs/enable_raptor)）。强，但**离线要 embedding + LLM 摘要**——属外接增强，不进确定性核心。
- **TopoChunker / HiSem-RAG / TreeRAG**：拓扑感知、语义驱动的分层切块/检索（[TopoChunker arXiv:2603.18409](https://arxiv.org/pdf/2603.18409)、[HiSem-RAG](https://www.mdpi.com/2076-3417/16/2/903)）。
- **关键区分**：RAPTOR 类是**语义聚合树**（内容相似度聚类），与本文的**逻辑结构树**（作者层级）正交、可叠加——结构树是确定性底座，语义树是其上的外接增强。

---

## 7. 对 docparse-rs 的落地建议（分层，遵身份约束）

> 不是方案定稿，是 roadmap 映射；每阶段独立有价值，按 ROI 排序。落地前另起 plan（SDD）。

### 阶段 A — 确定性结构树 + 导航接口（信封内，最高 ROI，零新模型）

1. **IR 加 `Section` 树**（§5 草案）：在逐页扁平 IR 上叠 `Document.outline`，节点引用页内元素、带 page/bbox/level/provenance。**向后兼容**（既有 pages 不动）。
2. **归一多层级源**（优先级）：Tagged `StructTree` 父子（**复用已读、别再丢**）> PDF `/Outlines`（**新读** + 模糊锚定正文）> HTML/DOCX 原生标题 > 字号/编号栈兜底。冲突时高优先级胜，低优先级补缺。
3. **修 chunk 面包屑**：`heading_path` 改走**真实树/`level`**，弃字号栈（修 [chunk.rs](../../crates/docparse-core/src/chunk.rs) 的 §3 已知偏差）。
4. **导航接口**（§6.2）：MCP/REST + 库 API：`outline` / `get_section` / `subtree` / `breadcrumb`，输出跨接口字节一致（延续 N2）。**这是把树变成 agent 检索面的关键一步。**
5. **新输出形态**：`-f outline`（树 JSON）、section-scoped chunks（每 chunk 带 `section_id` + 真实 `heading_path`）。

**验收门**：树层级正确率（vs 人工/Tagged 真值）；引用率仍 100%（每节点带 bbox）；clean 论文/报告端到端建树；三件套字节不变（树是叠加，不改既有输出）。

### 阶段 B — 难文档补全（可选增强）

- **ToC 目录页解析**（[ICDAR2013](https://clgiles.ist.psu.edu/pubs/ICDAR2013-ToC.pdf)）：无书签的扫描书籍/老 PDF。
- **版面模型层级深度**：复用 PP-DocLayoutV2 的标题类语义，补字号不单调/跨栏难例（§4.3）。

### 阶段 C — 语义树（信封外，外接）

- **RAPTOR 式摘要树**：经可插拔边界外接 LLM/embedding 服务，叠在结构树上做多抽象层检索（§6.3）。**不进主流程**。

### 对标结论

| 战场 | 目标 | 依据 |
|---|---|---|
| **数字原生文档结构树** | **要赢** | 确定 + 每节点可溯源 bbox + 零依赖单二进制 + **agent 可导航接口**——Docling 有 body 树但重依赖、难溯源；unstructured 干脆无层级 |
| **论文专用结构（参考文献/作者/摘要语义）** | **持平/够到** | GROBID 是专精标杆；我们走通用 + Tagged/Outline 确定层级，论文域可映射语义标签 |
| **语义聚合树（RAPTOR）** | **外接不打** | 需 LLM，属增强边界，不绑定主流程 |

---

## 8. 相似开源产品横评（深入）

> 聚焦一个问题：**这些产品到底产不产出"可导航的逻辑结构树"，怎么产，怎么给检索用，许可/部署代价多大。** 排序 ≈ 与本调研目标的契合度。

### 8.1 能力矩阵

| 产品 | 结构树? | 数据模型 / 树形 | 检索接口 | 许可 | 运行时 | 与 docparse-rs 关系 |
|---|---|---|---|---|---|---|
| **llmsherpa / LayoutPDFReader**（nlm-ingestor） | ✅ **真树** | `Document`=block 树根 → `Section`(含 level 嵌套)/`Paragraph`/`Table`/`ListItem`，父子关系完整 | `chunks()` 智能切块 + **section 标题下传子 chunk**；按 section/table 取 | **Apache-2.0** | Python + ML 后端 + **Docker server** | **最接近的对标**：它证明了"逻辑树 + 标题下传"对 RAG 的价值；我们能做到**确定 + 零依赖单二进制 + 无需起服务** |
| **Docling**（DoclingDocument） | ✅ **真树** | `body` 正文树 + `groups` 容器，`NodeItem`/`GroupItem` 经 JSON pointer 引父子；`content_layer` 分正文/页眉脚 | 导出树 JSON / Markdown / 转 LlamaIndex·LangChain node | **MIT** | Python + 版面/表模型 + 冷启动 | 数据模型**直接借鉴**；我们差异＝确定/可溯源/单二进制（见 refer/[docling 对比](docling-objective-comparison.md)） |
| **GROBID** | ✅ **真树**（论文域） | TEI/XML：`teiHeader` + `body>div>head` 嵌套；级联序列标注 | 出 TEI；下游自取 | **Apache-2.0** | Java + ML 模型 | 论文专精标杆；我们走通用 + Tagged/Outline 确定层级 |
| **PaperMage**（allenai） | ⚠️ 分层非树 | `Document` 多层 segmentation：`sections`/`blocks`/`paragraphs`/`figures`… 各为一层 entity（可叠，但非单一父子树） | 按层迭代 | Apache-2.0 | Python 研究工具包 + 模型 | 论文域；分层思路可参考，但非导航树 |
| **RAGFlow DeepDoc** | ⚠️ 区域级 | DLR 版面识别（header/footer/段/**section**）+ TSR 表结构；产分块非持久树 | RAGFlow 内部切块 | Apache-2.0 | Python + 视觉模型（慢） | 工程化 ingestion 参考 |
| **MinerU** | ⚠️ 块级合并 | 跨栏/跨页段落合并、公式/表/图区分；出 Markdown/JSON 区块，非显式树 | 下游自切 | **AGPL-3.0** ⚠️ | Python + 多模型 | 强解析弱树；**AGPL 许可不可引**（refer/[mineru 对比](mineru-objective-comparison.md)） |
| **PyMuPDF4LLM** | ⚠️ 伪层级 | 字号→`#` 标题 + 含 **ToC 项** + `page_chunks` 字典 | `page_chunks` 列表 | **AGPL-3.0** ⚠️ | Python（PyMuPDF） | 务实但非真树；**ToC 读取**值得借鉴（我们没读 Outlines） |
| **OpenDataLoader (ODL)** | ⚠️ 输出有层级 | 保留标题层级 + 读序 → Markdown；底层 veraPDF 结构 | Markdown/JSON | — | Java | 本项目参照系；标题层级在输出、非可导航树 |
| **Marker** | ⚠️ Markdown 层级 | 版面模型 → 带 `#` 层级 markdown（书籍结构好） | Markdown | **GPL-3.0** ⚠️ | Python + 模型，**~54s/页**极慢 | 慢 + GPL，不可引 |
| **unstructured** | ❌ **扁平** | element 列表，**所有标题同级 `#`**、不反映嵌套 | element 列表 | Apache-2.0/商业 | Python + 模型 | 反面教材（无层级） |
| **MarkItDown**（MS） | ❌ 无 | 轻量转 Markdown，**标题/表评分 0.00** | Markdown | MIT | Python | 轻但无结构 |
| **Nougat**（Meta） | ⚠️ 隐式 | arXiv 单页图→Markdown（VLM），层级随 markdown | Markdown | MIT/CC | GPU VLM | 学术 OCR，重 |

### 8.2 三个关键观察

1. **真"逻辑树"的产品很少，且全是 Python/Java + 模型 + 部署**：只有 **llmsherpa、Docling、GROBID** 产出带父子的导航树，三者都**重依赖**（模型下载/冷启动/JVM/起服务）。docparse-rs 若做到**确定性 + 零运行时依赖 + 单二进制**的结构树，正落在 roadmap §2「数字原生文档结构——要赢」的空位。

2. **几乎没有产品把树暴露成"agent 可调用的导航接口"**：它们产出树**工件**（JSON/Markdown/TEI），导航靠下游自己写。llmsherpa 的 `chunks()` + section 下传最接近"检索友好"，但仍是库调用而非 **agent 工具**。把 `outline/get_section/subtree/breadcrumb` 暴露成 **MCP/REST tool**（§6.2）是真正的空白点——与 A-RAG 的"分层检索接口"理念一致，却几乎无 OSS 现货。

3. **许可是硬约束**：MinerU(AGPL)、PyMuPDF4LLM(AGPL)、Marker(GPL) **不可引入**（本项目 Apache-2.0，roadmap §7 许可底线）。可安全参考**算法/数据模型**的是 **Docling(MIT)、llmsherpa(Apache)、GROBID(Apache)、PaperMage(Apache)**——优先从这四家借鉴 schema 与切块策略。

### 8.3 评测基准（补 §9 风险 3：层级怎么量）

- **READoc**（[arXiv:2409.05137](https://arxiv.org/abs/2409.05137)）：把"文档结构化抽取"定义为 PDF→语义化 Markdown，3576 篇真实文档（arXiv/GitHub/Zenodo），**显式评测层级 ToC + 标题检测**（Standardization/Segmentation/Scoring 三件套）——是衡量"结构树质量"最对口的统一基准，可作 docparse-rs 树层级的验收尺。
- **OmniDocBench**（CVPR2025，已用于本项目表/公式记分牌）：多元 PDF 解析综合基准，含读序/标题维度。
- 二者结合：READoc 量层级/ToC，OmniDocBench 量读序/表/公式——覆盖结构树的"骨架（层级）+ 血肉（内容）"。

### 8.4 结论

最该深挖的对标是 **llmsherpa（验证了树 + 标题下传对 RAG 的价值、Apache 可参考）** 与 **Docling（最成熟的树 schema）**。docparse-rs 的可赢点不在"再造一个树解析器"，而在**三件别人没合在一起做的事**：① 确定性 + 每节点可溯源 bbox；② 零依赖单二进制（无 Python/JVM/模型/起服务）；③ **把树暴露成 agent 导航工具**（MCP/REST）。前两件是既有身份约束的延伸，第三件是当前 OSS 的真空。

## 9. 风险与开放问题

1. **层级源冲突**：Outline、StructTree、字号三者打架时的归一策略需实测定档（建议高优先级源胜 + provenance 标注分歧，别静默）。
2. **Outline↔正文锚定**：模糊匹配的误锚（同名标题、页码偏移）——需诚实回退（锚不上就降级到字号树），不臆造层级。
3. **可变深度评测**：层级正确性怎么量？参考 [arXiv:2105.09297](https://arxiv.org/pdf/2105.09297) 的层级评测；需自建论文/报告子集真值（延续 roadmap §6"不可拼榜、只在可比子集比"）。
4. **跨页节边界**：节常跨页，与现有"逐页扁平 + 页内 group"的张力——树需在页之上聚合，注意页眉页脚不污染节内容（Docling `furniture` 教训）。
5. **IR 膨胀**：节点**引用**而非搬运内容（§5 要点①），否则 JSON 翻倍。
6. **确定性**：归一/锚定/聚类都必须可复现（同文档同树），守 roadmap §1 身份约束。

---

## 10. 参考资料

**相似产品（结构树 / 切块）**
- [llmsherpa / LayoutPDFReader（层级 block 树 + RAG smart chunks）](https://github.com/nlmatics/llmsherpa) · [后端 nlm-ingestor（Apache-2.0, Docker）](https://github.com/nlmatics/nlm-ingestor) · [数据模型解读](https://deepwiki.com/nlmatics/llmsherpa)
- [PaperMage（allenai，论文多层 segmentation）](https://github.com/allenai/papermage) · [ACL 2023 demo](https://aclanthology.org/2023.emnlp-demo.45/)
- [RAGFlow DeepDoc（DLR 版面 + TSR）](https://github.com/infiniflow/ragflow/blob/main/deepdoc/README.md) · [ingestion pipeline](https://ragflow.io/blog/is-data-processing-like-building-with-lego-here-is-a-detailed-explanation-of-the-ingestion-pipeline)
- [PyMuPDF4LLM（字号→#、含 ToC、page_chunks）](https://github.com/pymupdf/pymupdf4llm)
- [MinerU（跨栏/跨页合并，arXiv:2409.18839）](https://arxiv.org/pdf/2409.18839)
- [2026 OSS PDF→Markdown 横评（Marker/Docling/MinerU/pdf-craft/PyMuPDF4LLM）](https://themenonlab.blog/blog/best-open-source-pdf-to-markdown-tools-2026) · [Marker vs MinerU vs MarkItDown](https://jimmysong.io/blog/pdf-to-markdown-open-source-deep-dive/)

**评测基准（结构 / 层级）**
- [READoc：真实文档结构化抽取统一基准（层级 ToC + 标题），arXiv:2409.05137](https://arxiv.org/abs/2409.05137)
- [OmniDocBench：多元 PDF 解析综合标注基准，CVPR2025](https://openaccess.thecvf.com/content/CVPR2025/papers/Ouyang_OmniDocBench_Benchmarking_Diverse_PDF_Document_Parsing_with_Comprehensive_Annotations_CVPR_2025_paper.pdf)

**工具 / 模型**
- [Procycons：Docling / Unstructured / LlamaParse PDF 抽取基准 2025](https://procycons.com/en/blogs/pdf-data-extraction-benchmark/)（unstructured 同级 `#` 不反映层级）
- [Docling 文档模型（body/furniture/groups, NodeItem/GroupItem）](https://docling-project.github.io/docling/concepts/docling_document/) · [DoclingDocument.json schema](https://github.com/docling-project/docling-core/blob/main/docs/DoclingDocument.json) · [Docling arXiv:2501.17887](https://arxiv.org/html/2501.17887v1)
- [GROBID 原理（级联序列标注 → TEI 层级）](https://grobid.readthedocs.io/en/latest/Principles/) · [简介](https://grobid.readthedocs.io/en/latest/Introduction/) · [CORE + GROBID（3400 万篇）](https://blog.core.ac.uk/2023/07/17/core-grobid-structured-text-from-34-million-scientific-documents-and-counting/)

**结构 / 层级抽取**
- [Variable-Depth Logical Document Hierarchy（物理块→读序→层级深度分类），arXiv:2105.09297](https://arxiv.org/pdf/2105.09297)
- [HiPS：教科书分层 PDF 分割（ToC/版面/LLM/学习四类法），arXiv:2509.00909](https://arxiv.org/html/2509.00909v1)
- [ToC 识别与抽取（异构书籍），ICDAR2013](https://clgiles.ist.psu.edu/pubs/ICDAR2013-ToC.pdf)
- [基于标题的 HTML 逻辑层级抽取，VLDB vol8 p1606](https://www.vldb.org/pvldb/vol8/p1606-manabe.pdf)

**Agentic / 分层检索**
- [RAPTOR（递归聚类+摘要树）via RAGFlow](https://ragflow.io/docs/enable_raptor)
- [A-RAG：分层检索接口 + agent loop，arXiv:2602.03442](https://arxiv.org/pdf/2602.03442)
- [TreeRAG：层级存储的树切块，ACL 2025 Findings](https://aclanthology.org/2025.findings-acl.20.pdf)
- [TopoChunker：拓扑感知 agentic 切块，arXiv:2603.18409](https://arxiv.org/pdf/2603.18409)
- [HiSem-RAG：语义驱动分层检索](https://www.mdpi.com/2076-3417/16/2/903)
