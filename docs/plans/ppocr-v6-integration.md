# 功能设计 · PP-OCRv6 内嵌接入(2026-06-17)

> 调研与可行性见 [docs/refer/ppocr-v6-evaluation.md](../refer/ppocr-v6-evaluation.md)(**先读**)。本文是执行设计:目标、阶段门、落点、回退。
> 核心判断:接口与 v4/v5 同构(DB+CTC),**真风险只在 tract 算子**,故全程**门控在 Gate 0 spike** 上——spike 不过则不动产品代码。

## 0. 目标与非目标

**目标**:把 PP-OCRv6 作为 `--ocr` 的可选模型档接入,理想终态:
- **tiny 档**取代 PP-OCRv4-mobile 成为**新默认**(体积↓、Apple M4 6.1× 提速、精度↑);
- **medium 档**取代 PP-OCRv5-server 成为**质量档**(参数 40%、det+4.6/rec+5.1pp、ONNX 更快);
- 顺带解锁 50 语言单模型(补 G4 拉丁系多语种缺口)。

**非目标**:
- 不碰 PaddleOCR-VL(那是 VLM,走 G8b 服务路线,非本地 tract);
- 不改 CTC 解码 / 朝向 cls / 连通域后处理算法(接口同构,无须动);
- 不在 spike 未过时改任何产品代码。

## 1. 现状落点(改动面预估:通过则近零)

接入点全在 [crates/docparse-ocr/src/lib.rs](../../crates/docparse-ocr/src/lib.rs),且现役已为"任意 PP-OCR 系换目录即用"做了泛化(v5 接入时落地):

| 机制 | 现状 | v6 是否需改 |
|---|---|---|
| `find_file` 文件发现 | `*det*.onnx`/`*rec*.onnx`/`*dict*.txt` 子串匹配 | ❌ v6 文件名含 det/rec,免改 |
| `sanitize_dims` 维度消毒 | 双前缀 `(p2o.)?DynamicDimension.` | ❌ v6 同系命名,已覆盖 |
| CTC 贪心解码 | 按字典长度泛化(`classes-1=dict`) | ❌ v6 字典更大,逻辑无关 |
| 字典断言 | `≥6000` 条校验 | ❌ v6 更大,过 |
| `DET_SIDE` | 常量 960 | ⚠️ **可能** A/B 到 640(v6 训练分辨率)——见 Gate 2 |
| cls 朝向 | 独立模型可缺省 | ❌ 复用现役 cls |
| `fetch-models.sh` | 有 ocr/layout/unirec/ppv2 tier | ✅ **加 `ppocr-v6` tier**(唯一确定要写的代码) |
| `--ocr-models <dir>` CLI | 已存在,指向任意 PP-OCR 目录 | ❌ 直接可用 |

> 即:**若 Gate 0 通过,产品侧改动 = `fetch-models.sh` 加一个 fetch 函数 + 可能一个 `DET_SIDE` 调参**。这正是 v4→v5 的接入体量。

## 2. 阶段门(Gate)

### Gate 0 —— tract 算子可行性 spike ✅ **已通过(2026-06-17)**

仿 `scripts/spike/ppv2/` 套路,已建 `scripts/spike/ppocrv6/prepare.py`,模型 `PaddlePaddle/PP-OCRv6_tiny_{det,rec}_onnx`(det 2.0MB / rec 4.4MB,Apache-2.0)。**结果(详见 [评估 §5-§6](../refer/ppocr-v6-evaluation.md))**:

1. 原始 ONNX 直喂 tract → `into_optimized` 报 `Impossible to unify Sym(DynamicDimension_0) with Val(1)`(符号 batch 维烙进 `value_info`)——**与 PPV2 同类动态图问题,非算子缺失**;
2. **静态化修复**(`prepare.py`):钉 batch=1 → 清空 `value_info` → `infer_shapes` 重推;空间维/rec 宽度保持动态(生产宽度分桶不变)。**onnxsim 非必需**(Python 3.14 rec 段 segfault,strip+infer 即足够);
3. 静态化后:`DET @640/960` → `[1,1,S,S]`、`REC @48×{320,160}` → `[1,{40,20},6906]` **全部 `into_runnable`+前向成功**;算子全是 Conv/BN/Erf/HardSigmoid/MatMul/Softmax 等标准件,**无 `GatherND`/`GridSample`/`TopK`,无须任何 vendored tract 补丁**(比 PPV2 干净);
4. **Gate 1 首付**:tract rec 输出 vs onnxruntime 1.27 逐值对齐(sum 39.990 vs 40.000,first8 四位一致)。

**门已过判据达成**:tiny det+rec 两图 `into_runnable` + 前向产出正确形状张量。 → 进 Gate 1。

### Gate 1 —— 真图端到端 ✅ **已通过(2026-06-17,v6 更优且快 2×)**

模型按 loader 约定铺平 `models/ppocr-v6/`(det/rec 静态化文件 + 抽出的 6904 字 dict + 复用 v4 cls),**`--ocr --ocr-models models/ppocr-v6` 零代码改动跑通 `chinese_scan.pdf`**:

1. **数值**:tract rec vs onnxruntime 1.27 逐值对齐(Gate 0 已记);
2. **正确性**:中文全对,且**修正 v4 错字** `丶`→`、`(顿号);残差仅无害空格;
3. **速度**:release 单页 **0.47s vs v4 0.97s = 2.06×**;**体积** ~6.1MB vs ~16MB;
4. **通道序疑虑证伪**:RGB 序喂入(v6 yml 标 BGR)输出正确,非关键。

详见 [评估 §6b](../refer/ppocr-v6-evaluation.md)。三件套(born-digital)不经 OCR,与模型档无关,无需回归。

### Gate 2 —— 调参与速度门

1. **`DET_SIDE` A/B**:960(现役)vs 640(v6 训练分辨率),取 det 召回更优者;若打平保持 960 减小改动面;
2. **速度量化**:本机(darwin/M 系)v6-tiny / v6-medium 每页耗时 vs 现役基线(v4-mobile 2.0s / v5-server 6.0s),验证官方 Apple M4 6.1× 在本机成立的量级;
3. **阈值复核**:`DET_THRESHOLD`/`UNCLIP_RATIO`/`MIN_CONFIDENCE` 是否需随 v6 概率分布微调(默认不动,有回归再调)。

### Gate 3 —— 产品化与默认切换

1. `fetch-models.sh` 加 tier:
   ```bash
   ./scripts/fetch-models.sh ppocr-v6        # tiny,新默认候选(~? MB)
   ./scripts/fetch-models.sh ppocr-v6-medium # 质量档(~? MB)
   ```
   各 `dl_file` 拉 `_onnx` 仓的 det/rec + 字典,落 `models/ppocr-v6{,-medium}/`;
2. **默认切换决策**(需 Gate 1/2 数据支撑):tiny 是否取代 v4-mobile 成 `--ocr` 默认?保守起步可"v6 作可选档、默认仍 v4",数据足够再翻默认;
3. 文档:更新 [docs/status.md](../status.md) 记分牌 + [docs/refer/ppocr-v6-evaluation.md](../refer/ppocr-v6-evaluation.md) 补实测列 + CLAUDE.md §1 命令示例;
4. **跨样例回归 + clippy 零 warning + fmt**(CLAUDE.md §1/§4 硬门)。

## 3. 风险与缓解

| 风险 | 概率 | 影响 | 状态 |
|---|---|---|---|
| ~~LCNetV4/MetaFormer 骨干 tract shape-infer 卡壳~~ | ~~中~~ | ~~阻断~~ | ✅ **已解**:Gate 0 静态化跑通,无须补丁 |
| ~~RepLKFPN/动态图~~ | ~~中~~ | ~~阻断~~ | ✅ **已解**:`prepare.py` strip value_info+infer |
| ~~EncoderWithLightSVTR 注意力算子缺失~~ | ~~低~~ | ~~阻断~~ | ✅ **已解**:MatMul+Softmax 全过 |
| 真图 BGR/RGB 通道序差异致字符错 | 低 | 质量回归 | Gate 1 真图逐字 diff(v6 yml 标 BGR) |
| v6 后处理阈值(0.2/0.4/1.4 vs 现役 0.3/1.6)致召回变化 | 中 | 质量回归 | Gate 2 阈值 A/B |
| 默认切换引入扫描件回归 | 低 | 质量回归 | Gate 1 逐字 diff 门;保守先作可选档 |

## 4. 回退

- **Gate 0 不过且补丁不可行**:不动产品代码,把 spike 结论(卡的算子 + 为何不可补)写入 [docs/refer/ppocr-v6-evaluation.md](../refer/ppocr-v6-evaluation.md) 补"负结论"节,**维持现状**(v4 默认 / v5 质量档),挂"待 tract bump 或 paddle2onnx 调 opset 重导"待办;
- **任一 Gate 质量回归**:v6 退为可选档(`--ocr-models models/ppocr-v6`),默认不变,零破坏。

## 5. 验收清单(DoD)

- [x] **Gate 0:tiny det+rec 在 tract `into_runnable` + 前向成功**(2026-06-17,静态化后,无 vendored 补丁;`scripts/spike/ppocrv6/prepare.py`)
- [x] **Gate 1:真图 chinese_scan 端到端跑通——v6 更优(修正 `丶`→`、`)+ 2.06× 快 + 体积减半,零代码改动**
- [ ] Gate 2:`DET_SIDE`/阈值 A/B 定案 + medium 档量化(可选打磨)
- [x] **Gate 3**:`fetch-models.sh ppocr-v6` tier + **默认已翻**(`main.rs` 三处 CLI/MCP/serve `default_value` → `models/ppocr-v6`)+ **首次缺模型交互确认自动下载**(ureq);CLAUDE.md/README/status 已回写。
- [x] **静态化整步已消除(`774fe54`)**:`prepare.py` 弃用并删除——tract `with_ignore_value_info` 让 raw HF ONNX 直载,字典从 rec yml 解析。下方 §6 的 prepare.py 布局**已作废**,以本条为准。
- [x] [docs/status.md](../status.md) Phase 8 + [devlogs/2026-06-17-ppocr-v6-integration.md](../devlogs/2026-06-17-ppocr-v6-integration.md) 已补
- [ ] Gate 2(非阻断):`DET_SIDE`/阈值 A/B、medium 量化、更多扫描回归。

## 6. 产品化落点(⚠️ 历史:静态化路线,已被 raw 直载取代)

> **此节描述的 `prepare.py` 静态化布局已作废**(见 [ppocr-v6-ux-autoload.md](ppocr-v6-ux-autoload.md))。现役生产布局:`fetch-models.sh ppocr-v6` 直接下 4 个 **raw** 文件到 `models/ppocr-v6/`——`PP-OCRv6_tiny_{det,rec}.onnx`(HF `inference.onnx` 原样)+ `PP-OCRv6_tiny_rec.yml`(供 `load_dict` 抽字典)+ v4 `*cls*.onnx`;loader(`onnx_loader`/`load_dict`)直接消化,**无任何离线 prep**。下文留作 Gate 0 spike 的历史记录。

原静态化布局(已弃):
- `*det*.onnx` / `*rec*.onnx`:`inference.onnx` 经 `prepare.py` 静态化(钉 batch=1 + strip value_info + infer_shapes);
- `*dict*.txt`:从 rec `inference.yml` 的 `character_dict` 抽;
- `*cls*.onnx`:复用 v4。

## 7. 一句话

**Gate 0+1 已过**:PP-OCRv6 tiny 经静态化在 tract 跑通、真图 `chinese_scan` 比 v4 **更准(修正顿号错字)+ 快 2.06× + 体积减半**,且**零代码改动**(复用现役泛化)。剩 Gate 3 产品化(`fetch-models.sh` tier + 默认切换)与 Gate 2 打磨——**tiny 取代 v4-mobile 成新默认的证据已足**。
