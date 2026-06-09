# 下一步迭代计划 · Phase 3（N1–N6）

> 承接 [phase-2-summary.md](../phase-2-summary.md)：近期执行层 M1–M7 已收官并合入 main。本文规划**远期层**——把系统从"能跑、架构成立"推到"可度量地赢、可被 agent 调用、难例可补"。
>
> 上层战略见 [roadmap.md](../roadmap.md) §5 P3–P4 与 §6 记分牌；竞争定位见 roadmap §2。

---

## 1. 当前态与缺口

M1–M7 已交付：纯 Rust 多格式（PDF/DOCX/HTML）→ 带 provenance 的统一 IR → 段落/有框表格/标题 → RAG 切块+chunk↔bbox 引用 → 质量路由+可插拔外接边界。差异化记分牌（确定性/引用率/成本/零依赖/广度）**已用数字兑现**。

**最大缺口**：质量记分牌（NID/TEDS/MHS 与 Docling 0.882 同台）**还是空的**——"比 Docling 好"目前是**架构论证，不是度量**。这是下阶段第一优先。其余缺口：无服务化接口（agent 还不能直接调）、无真实 enhancer（难例边界只有 StubOcr）、语义层只到有框表格、无安全预检。

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
- **仍待**：真·人工真值（当前是 agreement-with-Docling，非 accuracy）；TEDS 换精确 APTED；用户若外采人工标注集可进一步出"准确率"。

### N2 · 服务化接口（REST → MCP）— *模块 10*
P3 的"面向 agent 可直接调用"。CLI 已有，加库外的服务面。

- [ ] **REST**（`axum` 或 `actix`）：`POST /parse`（multipart 文件 → JSON/Markdown/chunks）、`GET /healthz`；流式可选。
- [ ] **MCP server**：把 parse/chunk/locate 暴露为 MCP 工具，agent 直连（claude.ai/Claude Code 等）。
- [ ] 调度/并发/阶段缓存的最小版；可观测（per-stage 计时 + quality 分）。
- **验收**：一个 agent 经 MCP 上传 PDF → 拿到带 bbox 引用的 chunks → 高亮回原坐标。
- **依赖/风险**：**新依赖**（HTTP 框架 / MCP SDK）按 CLAUDE.md §4 先征询选型。

### N3 · 真实 enhancer 接入 — *模块 8*
M7 只给了边界 + StubOcr。接一个真实模型证明可插拔端到端。

- [ ] 实现一个 `Enhancer`：外部进程（如 tesseract/PaddleOCR CLI）或 HTTP（VLM/LLM）；对扫描页/高乱码页产出文本，归一回 IR、低 confidence、记 provenance。
- [ ] 元素级 `source` 标签（M7 遗留）：每 chunk 标注"哪个 parser/enhancer"。
- [ ] 端到端：`chinese_scan` 经路由 → 真实 OCR → 可读文本 + 引用。
- **验收**：扫描件从 0 文本到可检索；数字页**仍零模型**（成本不破）。
- **依赖/风险**：外部 OCR 引擎/服务（进程或网络），**不进纯 Rust 核心**（身份约束）；选型先征询。

### N4 · 语义层续：无框表格 / 列表 / 多栏列检测 — *模块 4* · 🚧 进行中（首增量完成）
把 M4 的有框表格扩到更难的结构；顺带修 M3 多栏左列重排限制。
> N1 同台数据：含表文档表格检出召回 1/6、TEDS≈0——学术 PDF 多为**无框表格**，是与 Docling 的最大结构差距。

- [x] **无框表格检测**（`detect_borderless_tables`，对齐驱动 + 内容门控防双栏版面误判）。**召回 1/6→3/6、TEDS 0.006→0.028**，检出真实学术表，零正文回归。devlog：[2026-06-09-n4-borderless-tables.md](../devlogs/2026-06-09-n4-borderless-tables.md)。
- [ ] **列检测**：从文本对齐推断列边界（产出列右缘 → 修 M3 多栏左列 + 提 TEDS 结构精度）。
- [ ] **标题分级**（提 MHS 0.257）：编号/字重检测 section header，而非仅字号众数。
- [ ] **列表层级**：项目符号/编号 + 缩进 → 层级（DOCX numbering）。
- [ ] 合并单元格 row/col span；单元格文本边界（防渗漏）；TEDS 换精确 APTED。
- **验收**：TEDS/召回/MHS 经 `compare_docling.py` 持续回升。

### N5 · 安全预检与复杂度画像 — *模块 9*（可随时插入）
接入面的治理层，面向 agent/RAG 的安全底线。

- [ ] **隐藏文本过滤**（防 prompt injection）：渲染模式 `Tr 3`（不可见）/超小字号/页外/同色文本检测与标注（对照 ODL 隐藏文本过滤）。
- [ ] **资源防护**：ZIP bomb（DOCX/解压上限）、恶意/超深对象、超大页数早停。
- [ ] **复杂度画像**：格式 + 页级路由信号（数字/扫描/混合/旋转/表格密度），喂给 N1 评测与路由。
- **验收**：构造隐藏文本/zip bomb 样例被识别且不 panic、产生可追踪错误码（不静默吞）。

### N6 · 选择性小模型 ONNX 内嵌 — *模块 8 / P4·远期·可选*
- [ ] 把已稳定的小模型（页面分类/方向/轻量 OCR）转 ONNX，用 `ort`/Candle 在 Rust worker 内推理，评估纯 Rust/Metal 部署；重型 VLM 仍外接。
- **前置**：N1 评测先证明哪些难例值得、N3 外接先跑通再考虑内嵌提速。

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
