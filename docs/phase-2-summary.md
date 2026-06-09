# 阶段总结 · Phase 2：M1–M7 近期执行层（对标 Docling）

> 时间：2026-06-09 · 状态：✅ 近期执行层 M1–M7 全部完成 · 41 单测、clippy 零 warning、逐字节确定性
> 代码规模：4,088 行 Rust（5 crate）；本阶段 7 个里程碑提交。承接 [phase-1-summary.md](phase-1-summary.md)。

---

## 1. 缘起与目标

Phase 1 交付了纯 Rust 的 PDF 文本抽取骨架（1,501 行）。Phase 2 的任务（见 [plans/beating-docling.md](plans/beating-docling.md)）：把它推进成一个**在数字原生文档上能赢过 Docling** 的系统——不在扫描 OCR/神经表格上硬拼，而在**部署、确定性、可溯源、成本**上赢，并把 born-digital 的结构与多格式广度做到 Docling 一个档。执行拆成 7 个里程碑 M1–M7，每个独立有用户价值、各带 plan + devlog。

## 2. 交付：M1–M7

| # | 里程碑 | 交付 | 关键验证 |
|---|---|---|---|
| **M1** | 文本保真 | 标准14 AFM 宽度 + 简单字体 Encoding/Differences/AGL 解码 + `Tc/Tw/Tz` | 修复 `rectification` 连字丢失；诊断纠正了根因（解码非宽度）|
| **M2** | IR 脊梁 | schema 版本化 + `Provenance` + 每 chunk `confidence` + `core::quality` 评分；`--quality` | **确定性 100/100 逐字节** |
| **M3** | 版面可读 | `core::layout`：段落聚合（门控防糊表格）+ 页眉页脚识别 | 1901 摘要成段、bialetti 表格逐行不糊 |
| **M4** | 有框表格 | `core::table`：内容流矢量线段→网格→单元格；Markdown 管道表格 | bialetti 检出 2 表、图形页**零误判** |
| **M5** | 多格式 | `docparse-docx`(docx-rs) + `docparse-html`(scraper) → 同一 IR；`core::synth` 合成坐标 | DOCX/HTML 端到端、标题→`##`、表格映射 |
| **M6** | RAG 切块 | `core::chunk`：chunk 带 page+bbox+标题面包屑；`locate()` 反查 | **引用可定位率 100%**，跨三格式统一 |
| **M7** | 路由+外接 | `core::quality::assess_page` + `core::enhance`：版本化 capability + `Enhancer` trait + 按页路由 | 29/30 数字页零模型、扫描页才升级；主流程独立 |

## 3. 架构演进

```
crates/
├── docparse-core/        格式无关核心（4 → 10 模块）
│   ├── ir.rs             + Provenance / confidence / Table / Cell / SCHEMA_VERSION
│   ├── reading_order.rs  （沿用）
│   ├── layout.rs         [新] 行→段/标题、页眉页脚、page_blocks 管线（output+chunk 共用）
│   ├── table.rs          [新] 有框表格检测（参考 wcag-algs，独立实现）
│   ├── synth.rs          [新] 无坐标格式的合成布局（DOCX/HTML 复用）
│   ├── chunk.rs          [新] RAG 切块 + chunk↔bbox 双向引用
│   ├── quality.rs        [新] 覆盖率/乱码评分 + 按页 assess
│   ├── enhance.rs        [新] 可插拔外接边界 + 质量路由
│   └── output.rs         改为消费 layout blocks + 渲染表格
├── docparse-pdf/         + encoding/encoding_tables/stdmetrics + resources/(AFM+AGL)
│                         interpreter 加 Tc/Tw/Tz + 矢量路径→线段
├── docparse-docx/        [新 crate] OOXML → IR
├── docparse-html/        [新 crate] DOM → IR
└── docparse-cli/         注册 3 后端；-f chunks、--quality、--route-plan
```

**核心设计兑现**：`core` 始终不依赖任何格式库；加格式只需 `impl DocumentParser` + 注册一行 + 复用 layout/output/chunk（M5 印证）；所有路径归一到同一带 provenance 的 IR。

## 4. 对记分牌的兑现（roadmap §6）

| 差异化指标 | 状态 |
|---|---|
| 确定性（同输入逐字节一致）| ✅ 全里程碑 100% |
| 引用可定位率（chunk→page+bbox）| ✅ 100%，跨 PDF/DOCX/HTML |
| 成本（多数页不碰模型）| ✅ 实测 29/30 数字页零模型 |
| 运行时依赖 | ✅ 0（AFM/AGL 内嵌、确定性核心无模型）|
| 多格式广度 | ✅ PDF + DOCX + HTML 同一 IR |

**质量记分牌（NID/TEDS/MHS 与 Docling 0.882 同台）尚未回填**——需要 born-digital 评测集与评分脚本，是下阶段最高价值项（见 [plans/next-iteration.md](plans/next-iteration.md)）。

## 5. 质量与工程

- **测试**：6 → **41**（core 23、pdf 14、html 3、docx 1）；纯算法（CMap/matrix/XY-cut/encoding/AFM/table/chunk/enhance）均有单测。
- **回归**：每个字体/解码/输出改动跑三件套（lorem/bialetti/1901）+ 2408；全程零回归。
- **clippy 零 warning**；近似/兜底均标注 TODO（如 cp1252≈WinAnsi、合成坐标非真实版面、单元格粗于视觉行）。
- **许可**：AFM/AGL 是 Adobe 事实数据 verbatim 内嵌（保留 notice）；表格/字体算法参考 veraPDF **独立重写**，不引入 GPL 代码。

## 6. 已知限制（诚实标注，承接各 devlog）

- 多栏左列散文暂不重排（fill_x 是整页右缘，待列检测）；旋转戳干扰；标题词内粘连。
- 有框表格单元格粗于视觉行（源按段画线）；合并单元格/多表分离/无框表格未做。
- DOCX/HTML 合成坐标非真实版面；内联格式拍平；DOCX 列表编号未解析。
- M7 不内置真实 OCR/LLM（只交付边界 + StubOcr 演示）；元素级 source 标签未加。

## 7. 下一步

近期执行层收官，转入远期（模块 9 安全预检、模块 10 服务化、P4 ONNX、真实 enhancer、**评测集回填记分牌**）。详见 [plans/next-iteration.md](plans/next-iteration.md)。
