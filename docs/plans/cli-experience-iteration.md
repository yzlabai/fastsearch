# CLI 体验迭代计划

> 状态：规划中（未排期）。承接 2026-06-18 已落地的 **进度可视化 + 批量解析**(见 [status.md](../status.md) Phase 9、[devlogs/2026-06-18-cli-progress-and-batch.md](../devlogs/2026-06-18-cli-progress-and-batch.md)、功能说明 [cli-batch-and-progress.md](../cli-batch-and-progress.md))。
> 本文收集后续 CLI 体验/性能的待办,含 review 暴露的硬伤与已记限制,按优先级排里程碑。**与代码不符以代码为准并回写本文。**

## 0. 背景

Phase 9 给 CLI 补了两块:① stderr 进度/速度可视化,② 文件夹批量 + 聚合报告(含整批模型复用、递归落盘镜像)。落地后仍有几处**已知限制**与 **review 暴露的健壮性缺口**,以及当初划到下一轮的能力(`--progress=json`、文件级并行)。本轮把它们成体系地排出来。

现状锚点:
- 共用管线 [cli/main.rs](../../crates/docparse-cli/src/main.rs) `parse_and_enhance`/`render_doc`;模型复用 `RunModels`(`OcrState` + `LazyUniRec`)。
- 批量 [cli/batch.rs](../../crates/docparse-cli/src/batch.rs):`collect_files`/`collect_dir`/`write_output`/报告渲染。
- 进度 [cli/progress.rs](../../crates/docparse-cli/src/progress.rs):`Reporter`/`ProgressMode`。
- 版面/表/公式/转写推理在 [docparse-ocr](../../crates/docparse-ocr/src/)。

## 1. 里程碑总览

| 里程碑 | 主题 | 含项 |
|---|---|---|
| **M1** ✅ | 模型复用收尾 + 健壮性硬伤 | ~~I1 版面模型复用~~ ✅、~~I2 符号链接环~~ ✅、~~I3 落盘路径硬化~~ ✅ |
| **M2** | 机器可读 + 报告增强 | ~~I4 `--progress=json` 事件流~~ ✅、~~I5 报告显示相对路径~~ ✅、~~I10 `--stats` CPU/内存用量~~ ✅、I6 报告小料 |
| **M3** ✅ | 吞吐 | ~~I7 文件级并行 `--jobs`~~ ✅(2026-06-19,Phase 10/S3,见 [speed-quality-iteration.md](speed-quality-iteration.md)) |
| **M4（候/按需）** | 深水区 | I8 基础解析页内进度(触 trait)、I9 模型加载失败语义 |

---

## 2. 待办明细

### I1 · 版面模型整批/整服务只载一次（M1，P0）— ✅ 已落地(2026-06-18)

**问题**:`--layout` 及 `--formula-model`/`--transcribe-model` 内部用到的版面检测,曾**每文件从路径加载 `LayoutModel`**——批量多 PDF 反复吃 ONNX 加载;**连服务端(MCP/REST)也每请求重载**。Phase 9 收口时只覆盖了 OCR + UniRec,版面留下了。

**已落地**:把 docparse-ocr 三个公有函数从"收路径、内部 `LayoutModel::new`"改为"**收预载 `&LayoutModel`**":
- `layout::enhance_document(doc, bytes, &LayoutModel, scale)`
- `formula::enhance_formulas(doc, bytes, &LayoutModel, &UniRec)`
- `transcribe::transcribe_pages(doc, bytes, &LayoutModel, &UniRec)`

调用侧:
- CLI:`RunModels` 加 `LazyLayout`(`OnceLock<Result<LayoutModel,String>>`,键 `cli.layout_model`),`parse_and_enhance` 三处从缓存取。**惰性**——不带这些 flag 的批量永不加载。
- 服务端:`EnhanceState` 加惰性 `layout`(`OnceLock<Arc<LayoutModel>>`,同 `unirec` 模式)+ `loaded_layout()`,**服务也只载一次**(原每请求重载,顺带免费修掉;`LayoutModel` 实证 `Send+Sync`,可跨并发请求 `Arc` 共享)。

**影响面**:3 个签名 + 5 处调用(CLI parse_and_enhance ×3、EnhanceState ×2)。`formula.rs`/`transcribe.rs` 去掉 `use std::path::Path`。
**验收**:全工作区 34 套件 + clippy 零 warning;`--layout` 单文件(`layout_enhanced_pages:1`)+ 2 文件批量实测产出正常、模型复用;单文件 `-f json`/`--ocr` 逐字不变。**Phase 9 模型复用至此彻底收尾。**

### I2 · 递归遇符号链接环不爆栈（M1，P1，review 暴露）— ✅ 已修(2026-06-18)

**问题**:`collect_dir` 用 `path.is_dir()` 判断是否递归,**`is_dir()` 跟随符号链接**;`-r` 扫到符号链接环(`a/link → a`)会**无限递归 → 爆栈**。违反项目"不 panic"红线(虽属病态输入)。

**已落地**:递归前跳过符号链接目录——`if path.is_dir() && !path.is_symlink()`(`Path::is_symlink` 自 1.58 稳定),即**默认不跟随符号链接目录**(更安全的默认);符号链接文件仍纳入(无环险)。`#[cfg(unix)]` 单测 `recursive_does_not_follow_symlink_cycles` 造 `loop -> root` 环验证终止 + 不重采;真二进制实测 `-r` 遇活环正常收口、只产出真实文件。如需跟随,后续可加 `--follow-symlinks` + canonicalize 访问集去环。

### I3 · 落盘相对路径硬化（M1，P2）— ✅ 已落地(2026-06-18)

**问题**:`write_output` 的 `rel` 来自 `strip_prefix(base)`(必相对、无 `..`)或显式文件的 `file_name()`,实测无逃逸。但 `file_name()` 在病态路径会回退到 `as_os_str()`,理论上能拼出绝对/越界路径(`dir.join("/x")` 在 unix 会**替换** `dir`)。当前不可达,但属脆弱点。

**已落地**:`safe_rel(rel)` —— 落盘前校验"相对 + 无 `ParentDir`/`RootDir`/`Prefix` 组件",否则退化为纯 `file_name()`(再不行用 `"out"`)。`write_output` 先过 `safe_rel`。单测 `safe_rel_blocks_escape` 覆盖绝对路径、`../../x`、`a/../../b` → 都收敛到裸名。belt-and-suspenders,正常输出零变化。

### I4 · `--progress=json` 机器可读事件流（M2，P1，原 FR7）— ✅ 已落地(2026-06-18)

**问题**:CI / 上层封装拿不到进度——人读表格与进度条无法解析。

**已落地**:`ProgressMode` 扩 `Json` 变体(`--progress json`);`Reporter` 拆 `human`/`json` 两个开关(互斥——json 模式人读 UI 全关、无 ANSI/条),加 `json()` + `emit(&Value)`。stderr 输出 JSON-lines:
- 单文件:`finish()` 发一条 `{"event":"summary","scope":"file","file","pages","bytes","seconds","pages_per_sec","mb_per_sec"}`。
- 批量:每文件完成即流式发 `{"event":"file",...}`(schema 同 `--report-json` 的每文件对象,经 `file_value()` 复用保证一致)+ 收尾 `{"event":"summary","scope":"batch",...}`(经 `totals_value()`)。

stdout 纯数据不受影响(`-f json --progress=json > out.json` 实测 `out.json` 零事件)。单测 `json_mode_is_machine_not_human`;实测单文件 + 批量每行合法 JSON。**未做(够用即止)**:`phase_start`/`phase_end` 细粒度相位事件(当前只到文件/汇总粒度)。

### I5 · 报告显示相对路径,消歧递归同名（M2，P2，review 暴露）— ✅ 已落地(2026-06-18)

**问题**:递归批量里 `alpha/paper.pdf`、`beta/paper.pdf` 在人读表格里都显示为 `paper.pdf`,两行看着一样。

**已落地**:`FileStat` 携 `rel`,`label()` 返回相对路径;人读表格、JSON `file`、CSV `file` 都用 `rel`(全源路径仍在 `path` 字段)。实测递归批量表格显示 `alpha/paper.pdf`/`beta/paper.pdf`,消歧。顶层/单文件 `rel==裸名`,显示不变。

### I10 · `--stats` CPU / 内存用量（M2，新需求 2026-06-18）— ✅ 已落地

**需求**(用户):命令行执行解析时带个参数看 CPU/内存使用情况。

**已落地**:`--stats` 在运行结束打一行资源用量到 stderr。新增 [resources.rs](../../crates/docparse-cli/src/resources.rs):`getrusage(RUSAGE_SELF)` 取 **peak RSS**(`ru_maxrss`,macOS=字节/Linux=KB,按 OS 折算)+ **CPU 时间**(user+sys,跨所有线程)→ **util% = CPU/wall**(>100% 即多核并行的证据)。依赖 `libc`(**本就在树里**=零新供应链面,同 hayro-ccitt 先例)。Unix-only,其它平台标 unavailable。
- 人读:`resources: peak RSS 50.5 MB · CPU 0.44s (user 0.41 + sys 0.03) · 338% util · wall 0.13s`。
- `--stats` 是显式开关,无论 `--progress` 都打印(同 `--quality`/`--profile`);**`--progress json` 下改发 `{"event":"resources",...}` 事件**(stdout 纯净)。
- 单文件 + 批量(报告后)都支持;`run_start` 时钟覆盖 parse+全部相位+输出。实测单文件 338% util、批量 517% util。单测 + 全工作区 34 套件 + clippy 零 warning。

### I6 · 报告小料（M2，P3）

慢文件高亮、按格式分组小计、总墙钟 vs 各文件耗时之和(暴露串行开销)、`--report json|csv` 也可写 stdout(`-` 约定)。按需取。**effort:S each。**

### I7 · 文件级并行 `--jobs N`（M3，P2，需先 spike）

**问题**:批量串行;大量**小数字 PDF** 时单文件内部页并行不足以打满,文件级并行可提吞吐。

**约束/风险**:
- **内存闸**:OCR/扫描页 buffer ~100MB/页 + 模型;文件级并行会成倍放大。`--ocr`/扫描批量必须**硬限并行度或禁用**。
- **嵌套 rayon**:文件级 × 页级双层并行,需实测是否过订阅、是否要分层池。
- 报告/进度聚合要保序(索引收集)、`RunModels` 的 `OnceLock` 已 `Sync` 可共享。

**方案**:`--jobs N`(默认 1=现状),`min(jobs, cores)`;扫描/OCR 路径强制小上限。**先 spike 量化**(数字批量 vs 扫描批量,不同 jobs 的吞吐/内存),再决定默认与上限。**effort:M + spike。风险:中。**

### I8 · 基础解析页内进度（M4 候，P3，深水）

需 `DocumentParser` trait 演进(逐页回调或先探页数),跨所有格式后端适配。Phase 9 需求分析已**否决本期做**(见 [plans/cli-progress-visualization.md](cli-progress-visualization.md) §4 约束1/方案C)。留作 trait 演进话题;若做,PDF 后端 `inputs.par_iter()` 处加 `AtomicUsize` + ticker 是落点。

### I9 · 批量模型加载失败语义（M4，P3）

现状:`OcrState`/`LazyUniRec` 用 `OnceLock` 缓存**首次加载结果含 Err**——一旦模型加载失败(如下载中断),整批所有需该模型的文件都拿同一错(快速失败、一致)。多数情况是对的;但**瞬时失败会毒化整批**。可选:对"瞬时"错(网络)允许有限重试再缓存。**先观察是否真痛,再决定。**

---

## 3. 非目标

- TUI 全屏界面、交互控件(Phase 9 非目标延续)。
- 跨文件内容去重/合并。
- 改 MCP/REST 协议输出形态(它们走结构化时延字段)。

## 4. 建议落地顺序

**M1 先行**(I1 版面复用是 Phase 9 真正收尾 + I2/I3 健壮性,合一个迭代;I2 是不 panic 红线,值得尽快)→ **M2**(I4 JSON 事件流 + I5 报告路径,机器可读与可读性)→ **M3**(I7 并行,需 spike,收益视场景)→ M4 按需。
