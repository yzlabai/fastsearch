# 测试结果 · 差异化记分牌（N1a）

> 日期：2026-06-10 · 来源：`scripts/metrics.sh`（可重复）· 这些是**无需 ground truth** 的指标（roadmap §6 差异化记分牌）。
> 质量记分牌（NID/TEDS/MHS）见 `compare_odl.py` / `compare_docling.py` 与 docs/testresults/ 对应文档。

| 指标 | 测得 | 目标（roadmap §6）| 判定 |
|---|---|---|---|
| 二进制体积（release 单文件）| **23.46 MB** | < 30 MB（含 OCR+版面推理栈与按需渲染器），运行时依赖 0 | ✅ |
| 解析延迟（lorem，预热中位）| **<10ms（低于 time -p 分辨率）** | < 100ms（无模型加载）| ✅ |
| 首次冷加载（lorem，含 dyld/FS）| **0.48s** | 一次性，无模型下载 | — |
| 吞吐（2408，14 页，3 次中位 0.02s）| **700.0 页/s** | 显著领先 Docling（待同台）| 我方基线 |
| 确定性（2408，20 次 JSON）| **20/20** 逐字节一致 | 100% | ✅ |
| 引用可定位率（全样例 chunk 带 bbox+page）| **216/216 (100%)** | 100% | ✅ |

- **运行时依赖 = 0**：AFM/AGL 内嵌，确定性核心无模型；单文件可直接分发（边缘/内网/WASM 友好）。Docling 需 Python + 模型下载。
- **冷启动**含进程启动 + lopdf 装载，无模型加载/下载。
- **吞吐**为我方实测基线；Docling/ODL 为其公开宣称值，非同机同台（见 benchmark-roundup）。

