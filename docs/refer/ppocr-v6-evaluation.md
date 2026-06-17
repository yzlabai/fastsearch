# 调研 · PP-OCRv6 是否接入 docparse-rs(2026-06-17,**Gate 0 spike 已跑通**)

> 触发:用户要求"深入研究 PP-OCRv6,看本项目是否可接入"。本文是接入前的事实与可行性分析。
> **2026-06-17 更新:Gate 0 spike 已实跑——tract 0.23 能加载并前向 PP-OCRv6 tiny det+rec(静态化后,无须 vendored 补丁),tract==onnxruntime 数值一致。结论从"待验证"升级为"已验证可行",见 §6。**
> 上游事实来源:PaddleOCR 官方文档 / arXiv 2606.13108 / HuggingFace `PaddlePaddle/pp-ocrv6` collection(2026-06 抓取)。
> 现役管线事实来源:[crates/docparse-ocr/src/lib.rs](../../crates/docparse-ocr/src/lib.rs);选型背景见 [paddleocr-v5-evaluation.md](paddleocr-v5-evaluation.md)。

## 1. PP-OCRv6 是什么(上游事实)

PaddleOCR 团队 **2026-06-11** 发布的下一代通用 OCR,核心是 **PPLCNetV4 统一骨干**,检测/识别两阶段管线沿用 **DBNet 检测 + CTC 识别** 的经典结构(与 v4/v5 同),但骨干/neck 全部重设计:

| 组件 | v6 设计 | 与 v4/v5 的关系 |
|---|---|---|
| 骨干 | **LCNetV4**(MetaFormer 范式:token-mixing 深度卷积 + channel-mixing,任务自适应下采样),det/rec 统一 | 替换 v5 的 PPHGNetV2 / LCNetV3 分立骨干 |
| 检测 neck | **RepLKFPN**(DilatedReparamBlock:7×7 深度卷积 + 膨胀分支,等效大感受野),比 v5 少 31% 参数 | 替换 RSEFPN |
| 识别 neck | **EncoderWithLightSVTR**(1×7 深度卷积局部建模 + 全局 self-attention,加性跳连),tiny 档退化为直接 reshape | 与 v4 现役 rec 同属 SVTR 家族(**关键:现役 v4 rec 的 SVTR-LCNet 已在 tract 上跑通**) |
| 识别 head | **CTC + NRTR 双头**;**NRTR 只在训练参与,推理仅 CTC 并行解码** | 推理接口=纯 CTC,与 v4/v5 **完全一致** |
| 朝向分类 | 论文未述独立 cls 模块 | 可继续复用现役 `ch_ppocr_mobile_v2.0_cls`(独立小模型,与 rec/det 解耦) |

**多语种**:单模型覆盖 **50 种语言**(简/繁中、英、日 + 46 种拉丁系),tiny 档 49 种(无日文)。字典较 v5 扩约 **200 个变音符**支撑拉丁系。

## 2. 三档型号与参数(上游事实)

| 档 | 端到端参数 | det 参数 | rec 参数 | det Hmean | rec Acc | ONNX |
|---|---|---|---|---|---|---|
| **tiny** | 1.5M | 438k | 1.11M | 80.6% | 73.5% | ✅(49 语言,无日文) |
| **small** | 7.7M | 2.48M | 5.29M | 84.1% | 81.3% | ✅ |
| **medium** | 34.5M | 22M | 19.2M | **86.2%** | **83.2%** | ✅ |
| 对照:PP-OCRv5_server | ~86M | — | — | 81.6% | 78.1% | ✅(现役质量档) |

medium 相对 v5_server:**det +4.6pp / rec +5.1pp**,且参数仅 ~40%。官方称 medium 以 34.5M 参数**超越主流十亿级 VLM 的 OCR 任务表现**。

**速度**(官方):medium 对 v5_server GPU 2.37×;V100 ONNX Runtime **0.67s vs 0.77s**;**tiny 在 Apple M4 上 6.1×**;medium 在 Xeon+OpenVINO 5.2×。

> ⚠️ Apple M4 提速是对**本机平台**(darwin/M 系)最直接的利好——与项目"速度快"定位高度契合。

## 3. ONNX 可得性与许可(上游事实)

- HF collection `PaddlePaddle/pp-ocrv6` 下 **6 个模型各有独立 `_onnx` 仓库**(如 `PaddlePaddle/PP-OCRv6_medium_rec_onnx` / `..._det_onnx`),另有 SafeTensors / 标准格式。
- **许可 Apache-2.0**(与现役 v4/v5/YOLO/UniRec 全系一致),沿用现有"不打包、`fetch-models.sh` 拉原仓"策略,**无许可障碍**。
- 导出工具 paddle2onnx,**opset 7~19**;动态维度命名沿用 `DynamicDimension.N` 系——现役维度消毒器(`sanitize_dims`,[lib.rs](../../crates/docparse-ocr/src/lib.rs))**已支持该前缀**。

## 4. 与现役管线的接口对照

现役管线([lib.rs](../../crates/docparse-ocr/src/lib.rs))抽象:`det(DBNet,prob map)→ 连通域 → unclip → 逐框 resize h=48 分桶 → rec(SVTR,CTC logits)→ 按字典长度泛化的 CTC 贪心解码`。逐项核对 v6:

| 接口面 | 现役假设 | v6 事实 | 是否兼容 |
|---|---|---|---|
| det 输入 | `[1,3,960,960]` ImageNet 归一,keep-ratio + 补零 | DB,训练 640×640,标准 mean/std | ✅ 接口同;`DET_SIDE=960` 是**可调常量**(v6 训练 640,值得 A/B) |
| det 输出 | `[1,1,H,W]` 概率图 | DB 概率图(1/4 分辨率上采) | ✅(后处理 threshold+unclip 通用) |
| rec 输入 | `[1,3,48,bucket]` `(x-0.5)/0.5` | **3×48×W**(同) | ✅ 完全一致 |
| rec 输出 | `[1,steps,classes]` CTC | CTC logits | ✅ |
| 字典 | `ppocr_keys_v1.txt` 一行一字,解码按字典长泛化 | 50 语言扩展字典(+~200 变音符) | ✅ 解码逻辑无须改;**仅需配套 v6 字典文件**(注意校验 `≥6000` 那条断言,v6 更大,过) |
| cls 朝向 | 独立 `*cls*.onnx`,可缺省降级 | v6 无新 cls | ✅ 复用现役 cls |
| 文件发现 | `*det*.onnx`/`*rec*.onnx`/`*dict*.txt` 子串匹配 | v6 文件名含 det/rec | ✅ `--ocr-models <dir>` 免改名 |
| 维度消毒 | 双前缀 `(p2o.)?DynamicDimension.` | paddle2onnx 同系命名 | ✅ 已覆盖 |

**接口结论:与 v4→v5 同样是"接口零变更、换模型目录即用"级别**——前提是 §5 的算子能在 tract 跑。

## 5. 曾判的真风险:tract 0.23 算子兼容 —— **spike 已证伪为可行**

接入前判断 v6 图内部全新算子组合可能像 PP-DocLayoutV2 般卡 tract。**2026-06-17 实跑证伪此风险**:

| 组件 | 引入的算子(实测 dump) | tract 0.23 实测 |
|---|---|---|
| LCNetV4 MetaFormer 骨干 | Conv / BatchNormalization / Erf(GELU) / HardSigmoid / Mul / Add | ✅ 全过 |
| RepLKFPN | 推理期 reparam 已折叠为静态 Conv | ✅ 全过 |
| EncoderWithLightSVTR | 深度卷积 + **MatMul×2 + Softmax**(轻注意力) | ✅ 全过(与现役 v4 SVTR 同族) |
| DB head | ReduceMean / AveragePool / 上采样 | ✅ 全过 |

**rec 全部算子**(实测):`Conv×37, BatchNorm×4, Erf×10, HardSigmoid×5, Add×52, Mul×25, MatMul×2, Softmax×1, ReduceMean×3, ...` —— **无 `GatherND`/`GridSample`/`TopK`** 这类当初坑 PP-DocLayoutV2 的算子,故 **无须任何 vendored tract 补丁**(比 PPV2 干净)。

## 6. Gate 0 spike 实测结果(2026-06-17,已通过)

脚本 `scripts/spike/ppocrv6/prepare.py`,模型 `PaddlePaddle/PP-OCRv6_tiny_{det,rec}_onnx`(各 `inference.onnx`+`inference.yml`,Apache-2.0,det 2.0MB / rec 4.4MB):

1. **原始 ONNX 直喂 tract 失败**:`into_optimized` 报 `Impossible to unify Sym(DynamicDimension_0) with Val(1)`——导出把**符号 batch 维**烙进中间节点 `value_info`,tract 拒绝 Sym 与具体 1 统一。**与 PP-DocLayoutV2 同类"动态图"问题,非算子缺失**。
2. **静态化修复**(`prepare.py`,沿用 PPV2 套路但更轻):`update_inputs_outputs_dims` 钉 batch=1 → **清空 `value_info`** → `infer_shapes` 重推。空间维(det H/W)与 rec 宽度**保持动态**,故单文件仍支持生产侧宽度分桶。(`onnxsim` 在 Python 3.14 上 rec 段 segfault,已设可跳过——仅 strip+infer 即足够,无须 onnxsim。)
3. **静态化后全过**:
   - `DET @640×640` → 输出 `[1,1,640,640]` DB 概率图(全分辨率);`DET @960×960` → `[1,1,960,960]` 同过 → **`DET_SIDE` 两值皆可,常量灵活**;
   - `REC @48×320` → `[1,40,6906]`;`REC @48×160` → `[1,20,6906]` → **CTC 6906 类(字典 ~6906)+ 宽度分桶生效**(320→40 步,160→20 步);
4. **数值一致(Gate 1 首付)**:确定性 ramp 输入下 **tract rec 输出 vs onnxruntime 1.27 逐值对齐**——sum 39.990 vs 40.000(40 时步各 softmax=1,差为 27.6 万值的浮点累积),first8 `0.99966…` vs `0.99963…` 四位小数一致。输出**已 softmax**(每时步和=1),与现役 v4 rec 同。

## 6b. Gate 1 真图端到端实测(2026-06-17,已通过——v6 更优且更快)

把静态化模型按现役 loader 约定铺平(`models/ppocr-v6/`:`PP-OCRv6_tiny_det_simp.onnx` / `..._rec_simp.onnx` / `ppocrv6_dict.txt`(6904 字,从 rec yml 抽)/ 复用 v4 `*cls*.onnx`),**`--ocr --ocr-models models/ppocr-v6` 零代码改动直接跑通** `chinese_scan.pdf`(现役 v4 14/14 行基准):

| 项 | v4-mobile(现役) | **v6-tiny** | 结论 |
|---|---|---|---|
| 中文正确性 | 14/14 基准 | 全对,且**修正 v4 错字** | ✅ **更优**:`上海丶深圳`(v4 把顿号 `、` 误为笔画 `丶`)→ v6 `上海、深圳` |
| 速度(release,单页,best of 3) | 0.97s | **0.47s** | ✅ **2.06× 快**(本机 Apple Silicon,印证官方 Apple M4 提速方向) |
| 体积(det+rec) | ~16MB | **~6.1MB**(1.7+4.4) | ✅ 更小 |
| 代码改动 | — | **0**(`find_file`+维度消毒+字典长泛化 CTC 全复用) | ✅ |
| 通道序疑虑 | — | RGB 序喂入输出正确 | ✅ **证伪非关键**(v6 yml 标 BGR,实测 RGB 序照对) |

差异仅余无害空格(列表序号后 `1. `、`人民币 5,000` 前空格)——非正确性回归。

## 6c. Gate 2 廉价旋钮 A/B(2026-06-17,已测并定案=保持现役全局常量)

实测 chinese_scan(release,逐字 diff),结论:**全局 det 常量(`DET_SIDE=960` / `DET_THRESHOLD=0.3` / `UNCLIP_RATIO=1.6`)即最优,v6 yml 的自家参数要么降级 v4、要么对 v6 无效——不改、不做 per-model 线穿**:

| 旋钮 | v6 | v4 | 定案 |
|---|---|---|---|
| `DET_SIDE` 640(v6 训练分辨率) vs 960 | 变(960 已正确,无须换) | **降级**:`5,000`→`5.000`(数字错)、`。`→`·` | **保持 960** |
| 阈值 `0.2`/`1.4`(v6 yml) vs `0.3`/`1.6` | **逐字不变**(v6 对该参数鲁棒) | **降级**:`。`→`·` | **保持 0.3/1.6** |

关键:det 后处理参数是**全局共享**(v4/v5/v6 同一管线),v4 对 0.3/1.6/960 调好;v6 鲁棒不挑参数。故"用 v6 yml 自家参数"非但无收益,还会破 v4——**per-model 参数化不被单样例证据支持,弃**。§6b 的无害空格(`1. `/`人民币 5,000`)源于检测框分组,非阈值可调,且非错误。

**剩余 Gate 2(需更多资源,非阻断)**:① medium 档质量/速度量化(需下 ~40MB+,且需 tiny 失手的难样例才显优势);② OmniDocBench `--ocr` 档记分牌回归(需基准 harness);③ 更多扫描样例回归(手头仅 chinese_scan)。

## 7. 结论与建议(Gate 0 已过,**可行**)

- **接入可行,且改动近零**:接口与 v4/v5 同级"换目录即用",曾判的唯一拦路虎(tract 算子)已 spike 证伪——静态化后跑通、数值对齐、**无须 vendored 补丁**。剩余只是真图调参(§6 待办 Gate 1/2);
- **tiny 档→新默认候选**:体积更小(det 2.0MB + rec 4.4MB vs v4-mobile 16MB 级)、Apple M4 6.1× 提速、精度高于 v4(rec 73.5% 是 v6 最弱档但 v4-mobile 在样例上已知有错字);**契合"速度快"定位**;
- **medium 档→质量档**取代 v5-server:参数 40%、det+4.6/rec+5.1pp、V100 ONNX 反而更快;
- 50 语言单模型顺带补 G4 多语种缺口(拉丁系),无需 monkt 多语言包拼目录;
- **代码改动**:复用 `find_file` 泛化 + 维度消毒 + 字典长泛化 CTC;**新增**=`fetch-models.sh` 加 `ppocr-v6` tier(含 `prepare.py` 静态化步,仿 ppv2)+ v6 字典落位 + `DET_SIDE`/阈值 A/B;
- **不走 VLM 误区**:v6 是两阶段 OCR(非 PaddleOCR-VL),正确接法就是本地 tract 推理,与现役同;PaddleOCR-VL 仍走 G8b 服务路线(见 v5 评估 §1)。

## 8. 一句话

PP-OCRv6 在**接口层与现役 v4/v5 同构(DB+CTC),许可同 Apache-2.0,ONNX 现成,Apple M4 提速对本机直接利好**——曾判的唯一拦路虎(tract 能否吃下 LCNetV4/RepLKFPN 新算子)**已 spike 证伪为可行**(静态化后跑通、tract==onnxruntime、零 vendored 补丁,比 PP-DocLayoutV2 干净)。**Gate 0 已过,剩真图调参**。功能设计见 [docs/plans/ppocr-v6-integration.md](../plans/ppocr-v6-integration.md)。
