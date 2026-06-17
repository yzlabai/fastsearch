# 研究+设计 · PP-OCRv6 用户体验:消除静态化 + 自动下载(2026-06-17)

> 触发:v6 设为默认后,UX 比 v4"下完即用"倒退——要 HF CLI + `pip install onnx pyyaml` + 跑 `prepare.py` 静态化。本文研究如何把 UX 拉回甚至超过 v4。
> 背景:[refer/ppocr-v6-evaluation.md](../refer/ppocr-v6-evaluation.md)、[plans/ppocr-v6-integration.md](ppocr-v6-integration.md)。
>
> **状态(2026-06-17):§1+§2 已实现并提交**——`prepare.py` 整条 Python 链已删,raw HF ONNX 经 `onnx_loader`(`with_ignore_value_info`)直载、字典经 `load_dict`/`parse_yml_char_dict` 从 rec yml 抽;v4 零回归、v6 输出与旧静态化版逐字一致(含 `、` 修正)、全测试+clippy+fmt 过。**§3 自动下载待用户定策后实现**(见 §5 决策表第一行)。

## 1. 核心突破:静态化整步**可消除**(已实测验证)

之前判断 PP-OCRv6 官方 ONNX 必须经 Python 静态化(钉 batch=1 + strip `value_info` + infer_shapes)才能喂 tract,因 tract 报 `Impossible to unify Sym(DynamicDimension_0) with Val(1)`。

**实测发现**:tract-onnx 0.23 自带 **`Onnx::with_ignore_value_info(true)`** builder——[tract-onnx-0.23.1/src/model.rs:193](file) 里 `if !self.framework.ignore_value_info { for info in &graph.value_info {...} }`,即该 flag 让 parser **跳过读取中间 `value_info`**,正是 Python `del graph.value_info[:]` 做的事。而 `with_input_fact([1,3,...])` 已钉 batch=1,`into_optimized` 自带 shape 重推——**Python 那三步在 Rust 端全有等价**。

实测(raw 即未经任何 Python 处理的 HF `inference.onnx`):

```rust
tract_onnx::onnx()
    .with_ignore_value_info(true)            // ← 唯一新增,等价 strip value_info
    .model_for_read(&mut &raw_bytes[..])?
    .with_input_fact(0, f32::fact([1,3,640,640]).into())?
    .into_optimized()?.into_runnable()?
```

| 验证 | 结果 |
|---|---|
| DET raw @640 / REC raw @{320,160} | **✓✓ 全部 load+run** → `[1,1,640,640]` / `[1,{40,20},6906]` |
| 数值 vs onnxruntime | rec ramp sum **39.990**(== 静态化版,== onnxruntime 40.000 容差内),first8 逐位一致 |
| 结论 | **`with_ignore_value_info` 仅丢 shape 提示、不改计算**,数值证明等价 |

**净效果:`prepare.py` + `onnx`/`onnxsim`/`pyyaml` 整条 Python 链可删**。比"自动静态化"更彻底——这一步直接消失。

## 2. 字典:从 yml 抽,Rust 端解析(消除 pyyaml)

rec `inference.yml`(56KB)的 `PostProcess.character_dict` 是受限 YAML 块列表:

```yaml
  character_dict:
  - '!'
  - '"'
  - $
```

格式恒定:`character_dict:` 后连续 `  - <scalar>` 行,scalar 是单引号串(`''`转义→`'`)或裸值。**~15 行 Rust 手解析即可**(无须引入 yaml 依赖):定位 `character_dict:` → 收 `  - ` 行至缩进回落 → 去引号。

> 备选:首次下载后把抽好的 dict 写 `ppocrv6_dict.txt`(复用现役 `find_file *dict*.txt`,且用户可查)。推荐此法——保持现有加载流不变。

## 3. 自动下载:ureq(已是 workspace 依赖)

`ureq` v2 已在 `[workspace.dependencies]`(docparse-vlm 用),docparse-ocr 加 `ureq.workspace = true` 即可,**零新依赖**。首次 `--ocr` 且模型缺失时 GET:

| 文件 | URL | 体积 |
|---|---|---|
| det.onnx | `https://huggingface.co/PaddlePaddle/PP-OCRv6_tiny_det_onnx/resolve/main/inference.onnx` | 2.0MB |
| rec.onnx | `.../PP-OCRv6_tiny_rec_onnx/resolve/main/inference.onnx` | 4.4MB |
| rec.yml(抽字典) | `.../PP-OCRv6_tiny_rec_onnx/resolve/main/inference.yml` | 56KB |
| cls.onnx(复用 v4) | `SWHL/RapidOCR/.../ch_ppocr_mobile_v2.0_cls_infer.onnx` | 585KB |

合计 **~7MB,一次性**。HF resolve URL 无须鉴权、稳定。

## 4. UX 对比(改造前后)

| 步骤 | 现状(commit c045e04) | 改造后 |
|---|---|---|
| 装工具 | `pip install -U huggingface_hub onnx pyyaml` | 无 |
| 下载 | `fetch-models.sh ppocr-v6`(需 HF CLI) | 自动(ureq) |
| 静态化 | `python scripts/spike/ppocrv6/prepare.py` | **消失**(`with_ignore_value_info`) |
| 抽字典 | prepare.py 内 pyyaml | Rust 手解析 |
| 首跑 | 4 步 + Python 环境 | `docparse scan.pdf --ocr` 即可 |

**结果:从"HF CLI + pip + python + 4 命令"→"直接跑,首次自动拉 ~7MB"**,达到甚至超过 v4 旧的"下完即用"(v4 还要先 `fetch-models.sh ocr`)。

## 5. 设计决策(需定)

| 决策 | 选项 | 倾向 |
|---|---|---|
| **自动下载是否默认开** | (a) 默认自动拉;(b) 缺模型时报清晰错+提示命令,显式 `--ocr-fetch` 才下;(c) 首次交互确认 | **(b)**:自动联网拉取是"外向动作",项目一贯 models opt-in;给一条可复制命令最稳。但用户明确要"自动下载"→ 可做 (a) 但加 `--no-download`/`DOCPARSE_NO_DOWNLOAD` 逃生阀 + 下载前 stderr 告知来源/体积 |
| **缓存位置** | (a) `models/ppocr-v6/`(项目相对,现状);(b) `~/.cache/docparse/`(跨 cwd 复用) | (b) 更"哪都能跑",但偏离现有 `models/` 约定;(a) 简单。建议 (a) 起步,后续可加 (b) |
| **`with_ignore_value_info` 是否全局开** | 仅 v6 / 所有 PP-OCR(含 v4/v5) | 全局安全(OCR 模型 shape 全可从输入推);**须回归 v4/v5 验证后再全局** |
| **prepare.py 去留** | 删 / 留作离线备用 | 加载路径不再需要;`fetch-models.sh` 简化为只下 3 raw 文件。prepare.py 可降级为"离线/无网时手动备路",或直接删 |
| **校验** | 是否校 sha / 体积 | 至少校下载非空 + onnx magic;HF 无稳定 sha API,体积下限即可 |

## 6. 改动落点(若实施)

| 改哪 | 改什么 |
|---|---|
| [crates/docparse-ocr/src/lib.rs](../../crates/docparse-ocr/src/lib.rs) `PpOcrEnhancer::new` | 3 处 `tract_onnx::onnx()` → `.with_ignore_value_info(true)`;det/rec/cls 直接吃 raw ONNX |
| 同上 / 新 `dict.rs` | dict 加载:若 `*dict*.txt` 缺而有 `*rec*.yml`,Rust 解析 `character_dict` |
| 同上 / 新 `fetch.rs` | `ensure_models(dir)`:缺文件时 ureq 拉 HF raw → 落盘;逃生阀 + 来源告知 |
| [crates/docparse-ocr/Cargo.toml](../../crates/docparse-ocr/Cargo.toml) | 加 `ureq.workspace = true` |
| [crates/docparse-cli/src/main.rs](../../crates/docparse-cli/src/main.rs) | 可选 `--ocr-fetch`/`--no-download` 旗;错误文案 |
| [scripts/fetch-models.sh](../../scripts/fetch-models.sh) | ppocr-v6 tier 简化:只下 3 raw 文件,删静态化提示 |
| `scripts/spike/ppocrv6/prepare.py` | 删或降级注释为"离线备路" |
| docs | 回写 CLAUDE/README/status:静态化步消失 |

## 7. 风险

| 风险 | 缓解 |
|---|---|
| `with_ignore_value_info` 全局开破 v4/v5/其它模型 | 先只对 v6 开;回归 v4 chinese_scan 逐字 + 三件套后再考虑全局 |
| 自动下载在 CI/airgapped/cron 环境联网失败 | 逃生阀 `--no-download`;`fetch-models.sh` 离线路保留;缺网报清晰错 |
| HF URL/repo 改名 | 错误信息带 URL;`fetch-models.sh` glob 已抗 repo 重组 |
| 下载中断留半文件 | 下到临时文件 + 校验(onnx magic/体积)后原子 rename |
| 未经同意联网 | 下载前 stderr 打印来源+体积+Apache 许可;提供关闭 |

## 8. 结论

**两个 UX 痛点都可根除,且核心(消除静态化)已实测验证**:
- **静态化 → 消失**(`with_ignore_value_info(true)`,raw ONNX 直载,数值等价,零 Python);
- **字典 → Rust 解析 yml**(零 yaml 依赖);
- **下载 → ureq 自动拉**(零新依赖,~7MB 一次性)。

落地后 `--ocr` 回到"装好二进制直接跑"。建议实施顺序:① lib.rs 上 `with_ignore_value_info` + dict-from-yml(纯本地,先把 Python 砍掉)→ ② 回归 v4/v5 不破 → ③ ureq 自动下载(带逃生阀)→ ④ 简化 fetch-models.sh、删 prepare.py、回写文档。**①②风险最低收益最大,可先做**。
