# 下一步迭代计划 · Phase 3（N1–N6）

> 承接 [phase-2-summary.md](../phase-2-summary.md)：近期执行层 M1–M7 已收官并合入 main。本文规划**远期层**——把系统从"能跑、架构成立"推到"可度量地赢、可被 agent 调用、难例可补"。
>
> 上层战略见 [roadmap.md](../roadmap.md) §5 P3–P4 与 §6 记分牌；竞争定位见 roadmap §2。

---

## 1. 当前态与缺口

> **进度快照（2026-06-10）**：N1 ✅、N2 ✅、N4 大部 ✅、N5（a+b）✅；**仅剩 N3**（部署选型待决策，已暂缓）与远期 N5c/N6。记分牌：vs ODL NID 0.764/MHS 0.627/TEDS 0.098；vs Docling 0.833/0.645/0.187（clean LTR 0.94–1.00 达同台水平）。以下原文保留为立项时的判断。

M1–M7 已交付：纯 Rust 多格式（PDF/DOCX/HTML）→ 带 provenance 的统一 IR → 段落/有框表格/标题 → RAG 切块+chunk↔bbox 引用 → 质量路由+可插拔外接边界。差异化记分牌（确定性/引用率/成本/零依赖/广度）**已用数字兑现**。

**最大缺口**（立项时）：质量记分牌（NID/TEDS/MHS 与 Docling 0.882 同台）**还是空的**——"比 Docling 好"目前是**架构论证，不是度量**。这是下阶段第一优先。其余缺口：无服务化接口（agent 还不能直接调）、无真实 enhancer（难例边界只有 StubOcr）、语义层只到有框表格、无安全预检。

```mermaid
flowchart LR
  N1[N1 评测与基线<br/>把"更好"变成数字] --> N2[N2 服务化接口<br/>agent 可调]
  N1 --> N4[N4 语义层续<br/>无框表/列表/多栏]
  N2 --> N3[N3 真实 enhancer<br/>难例端到端]
  N5[N5 安全预检] -.随时.-> N2
  N3 --> N6[N6 ONNX 小模型·远期]
```

---

## 2. 里程碑

### N1 · 评测与基线（NID/TEDS/MHS，与 Docling 同台）— **最高优先** · *记分牌*
**没有数字，"更好"就是口号**（roadmap §6）。也是后续所有改动的回归门。

- [x] **差异化指标自动化**：`scripts/metrics.sh` → `docs/testresults/2026-06-09-differentiation-metrics.md`。实测：体积 5.15MB、预热延迟 <10ms、吞吐 700 页/s、确定性 20/20、引用率 100%（运行时依赖 0）。
- [x] **评分脚本 + 提取器就绪**：`scripts/eval/score.py`（NID/TEDS/MHS，合成自检过）+ `extract.py`（chunks→评测格式）。流水线 `docparse -f chunks | extract.py | score.py vs gt` 通。
- [x] **与 Docling 同台**（用户提供 `tmp/refer/docling` 源码+测试集解阻）：`scripts/eval/{docling_gt_extract,compare_docling}.py` 拿 Docling 自带 13 份 born-digital 测试 PDF 的 **groundtruth（=Docling 自身输出）** 对比，**无需安装/运行 Docling**。结果见 [testresults/2026-06-09-docling-comparison.md](../testresults/2026-06-09-docling-comparison.md)。
- **测得（与 Docling 一致度，非人工真值）**：LTR 10 份 NID **0.600**、MHS **0.257**；含表 6 份 TEDS **≈0.006**（表格检出召回 **1/6**）；RTL 3 份 NID≈0（超范围）。13/13 解析 0 panic。
- **数据驱动的结论**：① 最大差距是**无框表格**（学术 booktabs）——M4 只做有框 → N4 无框表格**坐实为最高优先**；② 标题检测（字号众数）弱于 Docling 模型标注 → N4 标题分级；③ 阅读顺序中等一致；④ RTL 未支持（记录在案）。
- [x] **与 ODL 同台**：`scripts/eval/{odl_extract,compare_odl}.py`，全 15 份 born-digital。ODL 是确定性同类，水平可达。
- **⚠️ 评测参照 bug 修复（2026-06-09）**：两个 GT 提取器都**漏列表文本**（`odl_extract` 没递归 `list items`；`docling_gt_extract` 跳过 `group`），一直**低估**我方 NID。修复后（我方产出未变）：
  - **ODL 同台**：LTR NID **0.651→0.722**、MHS **0.584→0.614**（标题去代码行+裁连续标题串）。
  - **Docling 同台**：LTR NID **0.698→0.763**、MHS **0.612→0.625**。
  - 逐文档：`multi_page` 0.600→0.984、`2305` 0.751→0.921、`redp5110` 0.813→0.972。
  - devlog：[benchmark-harness-fixes](../devlogs/2026-06-09-benchmark-harness-fixes.md)。
- **现状判定**：**clean born-digital LTR 已达 docling/ODL 水平**（`multi_page`/`code`/`picture`/`redp5110`/`2305` 均 0.92–0.99）。聚合被两类**确定性天花板**拖低：CJK 复杂版面（`skipped_*`/`normal_4pages`）、最难双栏论文首页（`2203`/`2206` 作者块+版权脚注阅读顺序）——属 N3 enhancer/版面模型，非确定性可达。
- **仍待**：真·人工真值（当前是 agreement-with-Docling，非 accuracy）；TEDS 换精确 APTED；用户若外采人工标注集可进一步出"准确率"。

### N2 · 服务化接口（MCP → REST）— *模块 10* · ✅ 完成（2026-06-10，[plan](n2-serving.md)/[testresults](../testresults/2026-06-10-n2-serving.md)）
P3 的"面向 agent 可直接调用"。**次序反转为 MCP 先行**（stdio JSON-RPC 零新依赖、agent 立即可连；详见 plan §2）。

- [x] **MCP server**（`docparse mcp`，stdio，手写 JSON-RPC，零新依赖）：parse_document/get_chunks/locate 三 tool；locate 反查 bbox 中心命中同一 chunk；坏输入结构化 error 不死。
- [x] **REST**（`axum`+`tokio`，用户批准）：`POST /parse`（multipart → json/markdown/text/chunks，与 CLI 逐字节一致）、`GET /healthz`、`x-docparse-ms` 计时头；127.0.0.1 绑定。
- [ ] 调度/并发/阶段缓存的最小版；可观测（per-stage 计时 + quality 分）。
- **验收**：一个 agent 经 MCP 上传 PDF → 拿到带 bbox 引用的 chunks → 高亮回原坐标。
- **依赖/风险**：**新依赖**（HTTP 框架 / MCP SDK）按 CLAUDE.md §4 先征询选型。

### N3 · 真实 enhancer 接入 — *模块 8* · ✅ 完成（2026-06-10，P4 路线，[devlog](../devlogs/2026-06-10-n3-onnx-ocr.md)）
M7 只给了边界 + StubOcr。接一个真实模型证明可插拔端到端。
> 调研 ODL hybrid + Docling OCR 层 + 引擎质量后（[refer](../refer/n3-enhancer-odl-docling-research.md)），**路线已定为 P4 优先**：ONNX 内嵌 RapidOCR/PP-OCR 模型（中文事实标准；模型外部文件 ~16MB Apache-2.0；抽嵌入图原字节而非渲染——不破"不光栅化"）。**前置 spike：`tract`（纯 Rust）能否跑通三模型**——能则身份零妥协，不能再决策 ort feature-gate vs HTTP。tesseract CLI 降级为备选；HTTP 后端（ODL hybrid 同款）留作接最强模型的远期通道。N6 与本里程碑合并。

- [x] **`docparse-ocr`**（ONNX 内嵌，非外部进程）：PP-OCRv4 det+rec 经 `tract` 纯 Rust 推理；归一回 IR、confidence 封顶 0.99、`source: ocr:ppocr-v4`。
- [x] 元素级 `source` 标签（M7 遗留）：IR `TextChunk.source`（schema 0.4.0）。
- [x] 端到端：`chinese_scan` 经路由 → OCR → **14/14 行全对** + bbox 引用（`--ocr`，模型外部文件 ~16MB）。
- **验收 ✅**：扫描件 0 文本→14/14 行；数字页零路由零模型；OCR 路径确定性逐字节;79 单测、记分牌零回归。
- **依赖/风险**：外部 OCR 引擎/服务（进程或网络），**不进纯 Rust 核心**（身份约束）；选型先征询。

### N4 · 语义层续：无框表格 / 列表 / 多栏列检测 — *模块 4* · 🚧 进行中（首增量完成）
把 M4 的有框表格扩到更难的结构；顺带修 M3 多栏左列重排限制。
> N1 同台数据：含表文档表格检出召回 1/6、TEDS≈0——学术 PDF 多为**无框表格**，是与 Docling 的最大结构差距。

- [x] **无框表格检测**（`detect_borderless_tables`，对齐驱动 + 内容门控防双栏版面误判）。**召回 1/6→3/6、TEDS 0.006→0.028**。devlog：[n4-borderless-tables](../devlogs/2026-06-09-n4-borderless-tables.md)。
- [x] **标题检测**（参 veraPDF）：编号/全大写（[heading-detection](../devlogs/2026-06-09-heading-detection.md)）+ **字重入 IR `TextChunk.bold`**（[font-weight](../devlogs/2026-06-09-font-weight.md)）。**MHS 0.265→0.412→0.514**。
- [x] **词距修复**（参 veraPDF `SPLIT_THRESHOLD_FACTOR`）：0.25→0.15em。**NID 0.601→0.626**。devlog：[word-spacing](../devlogs/2026-06-09-word-spacing-fix.md)。
- [x] **横线 booktabs 表格检测**（`detect_ruled_tables`，宽横线界定区域 + 区内稳定列推断）。**召回 3/6→5/6、TEDS 0.028→0.101、连带 MHS→0.603、NID→0.640**。devlog：[ruled-tables](../devlogs/2026-06-09-ruled-tables.md)。
- [x] **聚类表格识别 P1a/P1b**（`core::table_cluster`，参 veraPDF `ClusterTableConsumer` 独立重写）：header 锚定列状态机 + 吸引级联 + **按列喂入** + 精度门。找到 ruled/borderless 漏掉的**真实宽数值表**（2203 `Tags\|Bbox`、`Model\|mAP`、`Tabula` 等），**2203 表 3→4、MHS 0.694→0.708，零回归**。devlog：[P1a](../devlogs/2026-06-09-p1a-cluster-table-scaffold.md)/[P1b](../devlogs/2026-06-09-p1b-cluster-attraction.md)。🚧 P1c（结构校验替代内容启发式）待做。
- [ ] **表格结构精度**：多值单元格/多级表头/合并单元格 + TEDS 换精确 APTED。**仍属神经领域**（Docling TableFormer），确定性收益递减。
- [ ] 列检测修 M3 多栏左列；列表层级；title-case 同字号标题（amt/韩文）——**确定性天花板，属 N3 外接**。
- **验收**：经 `compare_docling.py` 持续回升。

> **本会话进展（参 veraPDF/ODL/Docling 源码 + harness 数据驱动）**：表格召回 1/6→5/6；TEDS 0.006→0.101；NID 0.601→**0.697**；MHS 0.265→**0.610**。速度始终领先。

### 与 ODL 同台（`compare_odl.py`，ODL 是确定性同类，水平**可达**）

跑通 ODL（MS OpenJDK 21；brew openjdk 原生库损坏）拿全 15 份 born-digital 输出对比，**结论明确**：
- **文本/阅读顺序/标题：已基本追平 ODL**——code_and_formula 0.999/1.0/1.0、picture_classification 0.998、2305-pg9 0.990/—/1.0。在这些维度我们=确定性同类水平。
- **表格检出覆盖：明显落后**——ODL `wcag-algs` 确定性检出**远多**于我方（2203：ODL 13 vs 我方 3；2305 全：12 vs 2；amt：4 vs 0）。这是**确定性可达**的差距，非神经领域。
- 汇总（LTR 12）：NID 0.651、MHS 0.583、含表 TEDS 0.052。

**→ 达 ODL 水平的明确路径**：把表格**检出覆盖**做到 ODL 级——深挖 `veraPDF-wcag-algs` 的 `TableBorderConsumer`/`ClusterTableConsumer`（比我方 bordered+booktabs+borderless 覆盖更多表型）。确定性、可量化、可达。这是下一步最高价值项。

**进展（2026-06-09，聚类表格 P1a/P1b 已落地）**：深挖 + 重写见 [refer/opendataloader-verapdf-analysis.md](../refer/opendataloader-verapdf-analysis.md) 与 [plans/cluster-table-recognizer-rust.md](cluster-table-recognizer-rust.md)。P1b 已把 `ClusterTableConsumer` 的"按列喂入 + header 锚定 + 吸引级联"做出来，**零回归**下找到真实宽数值表。但仍用**数值/≥3列**保守内容门兜精度 → 非数值/2 列表走不到，2203 仍 4 vs ODL 13。**剩余 gap 由 P1c 关闭**：实现 gap 图 + `mergeClustersByMinGaps` 列碎片合并 + 真 `Table.validate`，用结构校验替代内容启发式，方能在保精度下放开更多表型逼近 ODL 覆盖。

### N5 · 安全预检与复杂度画像 — *模块 9* · 🚧 N5a+N5b 完成（[plan](n5-security-precheck.md)）
接入面的治理层，面向 agent/RAG 的安全底线。

- [x] **隐藏文本过滤**（防 prompt injection，2026-06-10）：`Tr 3/7`/超小字号(<1pt)/页外 → `TextChunk.hidden`，渲染输出全链路排除、IR JSON 保留可审计、quality 计数+flag；schema 0.3.0。同色文本显式 TODO（需填充色跟踪）。devlog：[n5a-hidden-text](../devlogs/2026-06-10-n5a-hidden-text.md)。
- [x] **资源防护**（2026-06-10）：`core::limits` 手写 ZIP 中央目录预检（绝对 2GiB + 压缩比 250x，不解压）+ 页数早停（50000）；接入 docx/pdf parser，可追踪错误不 panic。devlog：[n5b-resource-guards](../devlogs/2026-06-10-n5b-resource-guards.md)。
- [ ] **复杂度画像**（N5c，暂缓）：格式 + 页级路由信号（数字/扫描/混合/旋转/表格密度），喂给 N1 评测与路由——quality 已有雏形，待 N3 需要时扩。
- **验收 ✅**：隐藏文本 + zip bomb 构造样例均被识别、产生可追踪错误码、不 panic 不挂起（不静默吞）。

### N6 · 选择性小模型 ONNX 内嵌 — *模块 8 / P4* · ➡️ **已并入 N3 作为首选路线**（2026-06-10）
- 原"N3 外接先跑通再内嵌提速"的次序已反转：调研显示 ONNX 内嵌（RapidOCR/PP-OCR + 首选 `tract` 纯 Rust 运行时）在中文质量与部署形态上都优于 tesseract 外接，直接作为 N3 的实现路线。细节见 [n3-real-enhancer.md §-1](n3-real-enhancer.md)。重型 VLM 仍外接（N3b HTTP）。

---

## 3. 次序与依赖

| 里程碑 | 依赖 | 主要价值 | 新依赖 |
|---|---|---|---|
| **N1 评测基线** | — | 把"更好"变数字（**先做**）| Docling 对照（仅评测期）|
| N2 服务化 | N1（回归门）| agent 可调 | HTTP/MCP（先征询）|
| N3 真实 enhancer | M7 边界 | 难例端到端、成本佐证 | OCR/LLM（先征询）|
| N4 语义层续 | N1（评测验证）| TEDS/NID 提升 | 无 |
| N5 安全预检 | — | 治理底线（可随时）| zip 解析等 |
| N6 ONNX·远期 | N1+N3 | 提速难例 | ort/Candle |

**建议立即下一步：N1 评测基线**——它把记分牌从空变实，且成为 N4 等结构改动的回归门，杠杆最高。N2/N5 无强依赖可并行起。

---

## 4. 不变的边界（承接 roadmap §6）

- 确定性核心**纯 Rust 自研**；真实模型一律经 `core::enhance` 边界**外接**，主流程无之独立运行。
- 参考 veraPDF/ODL **算法**独立实现、标注出处；**不引入 GPL 代码**（Apache-2.0）。
- 新依赖先征询（CLAUDE.md §4）；近似必标注；字体/解码/输出改动必跨样例回归；clippy 零 warning。
- 每里程碑前补 `docs/plans/<name>.md`、后补 `docs/devlogs/`，记分牌回填 `docs/testresults/`。
