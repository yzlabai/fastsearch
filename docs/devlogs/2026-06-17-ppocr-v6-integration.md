# devlog · PP-OCRv6 内嵌 OCR + UX 自动下载(2026-06-17)

一句话:**把 PaddleOCR PP-OCRv6 tiny 接入为 `--ocr` 新默认——比 v4 更准(修顿号)、快 2×、体积减半;过程中发现 tract `with_ignore_value_info` 可让 raw HF ONNX 直载,反手砍掉整条 Python 静态化链;再补首次缺模型时交互确认自动下载,`--ocr` 回到"装好二进制直接跑"。**

计划:[plans/ppocr-v6-integration.md](../plans/ppocr-v6-integration.md)(Gate 0–3)+ [plans/ppocr-v6-ux-autoload.md](../plans/ppocr-v6-ux-autoload.md)(UX)。调研:[refer/ppocr-v6-evaluation.md](../refer/ppocr-v6-evaluation.md)。

## 做了什么

三个 commit,递进:

### 1. 接入 + 翻默认(`c045e04`)
- PP-OCRv6(2026-06-11 发布,LCNetV4+RepLKFPN+EncoderWithLightSVTR)检测/识别**接口与 v4/v5 同构(DB+CTC)**——NRTR 头只训练用,推理纯 CTC。故零代码改动经现役 `find_file` + 维度消毒 + 字典长泛化 CTC 接入。
- **Gate 0 spike**:曾判风险(tract 吃不下新算子)证伪。原始 ONNX 把符号 batch 维烙进 `value_info` → tract 报 `Impossible to unify Sym(DynamicDimension_0) with Val(1)`(**与 PP-DocLayoutV2 同类动态图,非算子缺失**)。算子全标准件(Conv/BN/Erf/HardSigmoid/MatMul/Softmax,**无 GatherND/GridSample/TopK**),无须任何 vendored 补丁。
- 当时用 Python `prepare.py`(钉 batch=1 + strip value_info + infer_shapes)静态化跑通,tract==onnxruntime 逐值对齐。
- **Gate 1 真图**:`chinese_scan.pdf` 端到端比 v4-mobile **更准**(`上海丶深圳`→`上海、深圳`,v4 把顿号 `、` 误为笔画 `丶`)+ **快 2.06×**(0.47s vs 0.97s)+ **体积减半**(~6MB vs 16MB)。
- 翻默认:`main.rs` 三处(CLI/MCP/serve)`default_value` v4→`models/ppocr-v6`;v4 留回退档。

### 2. raw ONNX 直载,删 Python 整步(`774fe54`)—— 本次最大收获
- **关键发现**:tract-onnx 0.23 自带 `Onnx::with_ignore_value_info(true)`([model.rs:193](file)),正是 Python `del graph.value_info[:]` 的等价物——让 parser 跳过读取烙进图的符号维、从钉死的输入 fact 重推所有中间 shape。配合本就有的 `with_input_fact`,**Python 那三步在 Rust 端全有等价**。
- 实测:raw HF `inference.onnx` 经 `onnx_loader()`(统一封装该 flag)直接 load+run,数值与静态化版逐字一致(rec ramp sum 39.990 == onnxruntime)。**整条 `prepare.py` + onnx/onnxsim/pyyaml 链删除**。
- 字典:`load_dict()`/`parse_yml_char_dict()` 优先 `*dict*.txt`,缺则从 `*rec*.yml` 的 `character_dict` 块用 ~15 行 Rust 解析(受限 YAML 子集,零 yaml 依赖,带单测覆盖引号/转义/空格/块尾)。
- flag 全局开**对 v4/v5 安全**(v4 chinese_scan 逐字不变)。
- provenance 诚实化:`source` `ocr:ppocr-v4`→`ocr:ppocr`(loader 是 generation 无关的,不再谎称 v4)。

### 3. 首次缺模型交互确认自动下载(`f40bdc4`)
- 策略=**首次交互确认**(用户定):**TTY 检测天然区分三接口**,无须 per-face 配置——CLI 终端 y/N 确认后拉;非 TTY(MCP/REST/管道/CI)退化为清晰错;`DOCPARSE_OCR_DOWNLOAD=1` 预确认;自定义 `--ocr-models` 路径不下载(URL 未知)。
- [fetch.rs](../../crates/docparse-ocr/src/fetch.rs):ureq(已是 workspace 依赖)拉 4 文件(det/rec onnx + rec yml + v4 cls,~7MB)。**实战踩坑**:HF xethub CDN 中途断流(`response body closed before all bytes were read`,det 成功、rec 断在 430KB/4.4MB,curl 同 URL 完整)→ 加 **3 次重试 + User-Agent + 原子 rename**(临时 `.partial` → 校验体积 → rename,半文件不落地)后稳定。
- `main.rs::ensure_ocr_models`:CLI 单文件路径 + 服务路径(`OcrState::get`)均接入。

## 验证

- 全测试过(docparse-ocr 26 含新 `parse_yml_char_dict` 单测);clippy 零 warning。
- v4 回归:chinese_scan 逐字不变(全局 flag 安全)。
- v6 raw == 旧静态化版逐字一致(含 `、`)== onnxruntime。
- born-digital 三件套(lorem/bialetti/1901.03003)不经 OCR,不受影响。
- 自动下载实测:空 `ppocr-v6` 目录 `DOCPARSE_OCR_DOWNLOAD=1 --ocr` → 拉 4 文件后端到端 OCR 正确;非 TTY 缺模型报清晰错;present 时零开销不提示。

## 关键收获 / lesson

- **"必须 Python 静态化"是错判**:PP-DocLayoutV2 当年靠 vendored tract 补丁跑通,惯性以为 v6 也要重武器。实则 v6 卡的是动态图(symbolic batch in value_info),而 tract 早有 `with_ignore_value_info` 一键解决。**先翻 tract API 再上 Python**——省掉整条外部依赖链。
- **接口同构 ≠ 图同构**:v4/v5/v6 对外都是 DB+CTC,但图内部算子/动态维差异大;迁移先 dump 算子(确认无 exotic)、再 spike load(暴露 shape-infer 坎),两道关分清"算子缺失"与"动态图"。
- **CDN 下载必须重试**:HF xethub 大文件偶发中途断流,单次 ureq 必栽;重试 + 原子落盘是底线。

## 待办

- **Gate 2(非阻断)**:更多扫描样例回归、后处理阈值(v6 yml `0.2/0.4/1.4` vs 现役 `0.3/1.6`)与 `DET_SIDE`(640 vs 960)A/B、medium 质量档量化(取代 v5-server)。
- **OmniDocBench 记分牌**:v6 默认后 `--ocr` 档分数回归(预期升,v4-mobile 0.42–0.44 是已知短板)。
- 既存 fmt 漂移(8 个未触碰文件,本机 rustfmt 版本差异)可单独纯格式 commit 清理。

## 关键文件

- 代码:`crates/docparse-ocr/src/{lib.rs(onnx_loader/load_dict/parse_yml_char_dict),fetch.rs}`、`crates/docparse-cli/src/main.rs(ensure_ocr_models + 三处 default)`
- 脚本:`scripts/fetch-models.sh`(ppocr-v6 tier,只下 raw 文件)
- 文档:plans×2、refer/ppocr-v6-evaluation、status.md Phase 8、README、CLAUDE.md
- 已删:`scripts/spike/ppocrv6/prepare.py`(raw 直载后无用)
