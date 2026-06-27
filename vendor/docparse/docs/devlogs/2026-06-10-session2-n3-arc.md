# Devlog · 会话总结(下午场):N3 全弧——调研 → 决策 → spike → 落地 → 服务面对等

> 日期:2026-06-10(续 [上午场总结](2026-06-10-session-summary.md)) · 主题:把"真实 enhancer"从待选型推到全接口可用
> 方法:源码调研(ODL/Docling)→ 用户决策(P4 优先)→ 门控 spike → 实现+inline review → 每步 devlog+commit。

## 弧线(全部已推 main)

1. **调研**([refer](../refer/n3-enhancer-odl-docling-research.md)):ODL 核心零模型、难例 HTTP 外接;Docling 七引擎工厂,tesseract CLI 是子进程+TSV。**关键解锁**:扫描页=嵌入光栅图,抽原字节即可 OCR,不破"不光栅化"。
2. **引擎质量评估**:tesseract 中文是短板(chi_sim 质量差+插空格 bug);中文事实标准是 PaddleOCR;RapidOCR(ONNX)与 P4 契合 → **用户决策:P4 优先**。
3. **门控 spike**([devlog](2026-06-10-n3-tract-spike.md)):`tract`(纯 Rust)跑通 PP-OCRv4 det+rec,中文几乎全对,1.06s——纯 Rust 路线成立,身份零妥协。发现 paddle2onnx 维度名需等长消毒。
4. **落地**([devlog](2026-06-10-n3-onnx-ocr.md)):`docparse-ocr` crate + IR 0.4.0(Image 像素载荷/`source` 标签)+ 解释器 `Do` 惰性图抽取 + CLI `--ocr`。chinese_scan **14/14 行全对**、数字页零模型、OCR 确定性逐字节。开发中抓 4 个 bug(最值钱:置信度二次 softmax)。
5. **服务面对等**(fd57cbc):`OcrState` 懒加载,MCP `ocr:true` / REST `?ocr=true`;REST 与 CLI **字节一致**;模型缺失结构化报错。

## 本次 review 的修复(commit 同此 devlog)

- **CLI `--ocr` 误加载**:数字文档(无任何页需 OCR)也强制读模型,模型缺失即失败——违背"数字页零成本"。修:先 `assess_pages`,有需要才加载(`lorem --ocr --ocr-models /nonexistent` 现在正常出文本)。
- **Form XObject 嵌套扫描图**收不到(解释器不执行 form 内容流)——显式 TODO 入 `images.rs` 模块文档,留待 form 流解释。

## 终态

80 单测、clippy 零 warning、双记分牌零回归(vs ODL 0.764 / vs Docling 0.833)、二进制 19.08MB(tract ~12.5MB,20MB 门内)。**roadmap P0–P4 全阶段、模块 1–10 中除"复杂度画像"(模块 9 尾巴)外全部 ✅。**

## 剩余工作池(按价值排序)

1. **N5c 复杂度画像**(模块 9 收尾):页级信号(数字/扫描/混合/图覆盖/表密度)入 quality;
2. OCR 长尾:cls 方向分类换源、JBIG2/CCITT 1-bit、多语种字典、Form 流解释;
3. 人工真值评测集(一致度→准确率,需外采/标注决策);
4. 二进制瘦身 feature-gate(余量小但仍在门内)。
