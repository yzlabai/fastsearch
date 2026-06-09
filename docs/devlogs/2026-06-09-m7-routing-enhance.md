# Devlog · M7 质量评分驱动路由 + 外接 AI 边界

> 日期：2026-06-09 · 里程碑：[plans/beating-docling.md](../plans/beating-docling.md) M7 · 状态：✅ 完成
> 结果：`core::enhance`——版本化 capability + per-page 路由 + 可插拔 Enhancer；主流程独立；数字页零模型、扫描页才升级

---

## 1. 目标

把 M2 的 `quality` 评分接上"按页判难例 + 可插拔外接"的**成本论点**（roadmap §6 差异化记分牌）。身份约束：主流程**无外接也能独立产出**（AI 可插拔，不绑定）。

## 2. 实现

| 文件 | 改动 |
|---|---|
| `core/quality.rs` | `assess_page`/`assess_pages` → `PageAssessment{page,chars,garbled_ratio,flags,needs_enhancement}`（无文本=扫描、高乱码=坏解码）|
| `core/enhance.rs`（新） | `Capability{name,version,handles_scanned,handles_garbled}`（版本化能力）；`Enhancer` trait（`capability()` + `enhance_page(&Page)->Option<Vec<Element>>`）；`plan`（只规划不跑，报告硬页率）；`apply`（按页跑首个 capable enhancer、归一回 IR、低 confidence 标记）；3 单测（含 StubOcr）|
| `cli/main.rs` | `--route-plan`：打印硬页路由计划（无注册 enhancer→展示哪些页需模型）|

**关键设计**：
- **主流程独立**：`parse()` 永不调 `enhance`；路由是**可选后置步**。单测 `no_enhancers_means_no_changes` 钉死这条。
- **按页升级**：`apply` 只替换 `needs_enhancement` 的页，数字页原样穿过。
- **归一回同一 IR**：enhancer 产出 `Element`，confidence<1.0（区分确定性 vs 模型）。
- **版本化 capability**：能力按 flag 匹配（scanned→handles_scanned…），可观测、可灰度。

## 3. 验证（成本论点）

`--route-plan` 跑真实样例：

| 样例 | 硬页/总页 | 说明 |
|---|---|---|
| 1901.03003 | **0/15** | 数字论文，全不需模型 |
| 2408.02509v1 | **0/14** | 同上 |
| chinese_scan | **1/1** | 扫描件，flag `scanned_no_text` → 升级 |

**29 个数字页零模型、仅扫描页升级**——成本论点成立：便宜确定的先上，只有质量评分判难例才按页触发外接。

- 单测：`plan` 只路由硬页；`apply` 只替换扫描页（数字页 confidence=1.0 不动、扫描页换成 confidence=0.5 的"[ocr]"占位）；无 enhancer 时零改动。
- 全套 41 单测过（core 23 +3 enhance）；clippy 零 warning；三件套零回归；确定性 15/15。

## 4. 边界与未做

- **不内置任何真实 OCR/LLM**：M7 只交付**边界 + 路由 + StubOcr 演示**。接真实模型（外部进程/HTTP）由调用方注入 `Enhancer`，超出纯 Rust 核心（roadmap 明确"AI 外接、不进主流程"）。
- **元素级 source 字符串**：当前用 confidence + 路由报告做页级归因；细到每元素"哪个 enhancer/parser"的 source 标签未加（避免再动 IR），留 TODO。
- **路由策略**：首个 capable enhancer 即用；多 enhancer 竞价/级联/打分未做。
- **安全预检/复杂度画像**（模块 9）不在 M7：恶意对象/ZIP bomb/隐藏文本过滤是独立工作。

## 5. 对记分牌（roadmap §6）

- **成本（差异化记分牌）**：实测 29/30 数字页零模型——"多数页走快路径"有数字佐证。
- **AI 可插拔**：统一边界 + 版本化 capability + 主流程独立，三条身份约束兑现（确定性核心赢、AI 增强不绑定）。

## 6. 里程碑 M1–M7 全部完成

近期执行层 M1–M7 收官。后续为远期：模块 9 安全预检、模块 10 服务化运行时（REST/gRPC/MCP）、P4 小模型 ONNX 内嵌、真实 enhancer 接入、born-digital 评测集回填 NID/TEDS/MHS 与 Docling 同台。见 [roadmap §5](../roadmap.md) P3–P4。
