# CLI 处理速度可视化 · 需求分析

> 状态：**M0+M1 已实施（2026-06-18）**，见文末「实施记录」。下文为原始需求分析，保留作设计依据。
> 目标读者：决定是否做 / 怎么做的人。
> 一句话：docparse-rs 定位"速度快"，但**命令行跑的时候用户看不到速度**——本文分析"在 CLI 里把处理进度/速度实时呈现出来"的需求、约束与可行方案。
> 代码现状锚点见每节 `file:line`；与代码不符以代码为准并回写本文。

## 1. 背景与动机

项目核心卖点是"**速度快、质量好**"（CLAUDE.md 开篇），但这个"快"目前**对用户不可见**：

- CLI 一次性跑完才出结果，中间**无任何进度/速度反馈**。现有 stderr 输出只有**各增强相位结束后的 JSON 计数**（`{"images_exported": N}`、`{"layout_enhanced_pages": N}` 等，[main.rs:629-781](../../crates/docparse-cli/src/main.rs#L629-L781)），且只在 `--ocr/--layout/...` 启用时才有，纯解析快路径**全程静默**。
- 唯一的计时埋点在 REST 服务：`Instant::now()` + 响应头 `x-docparse-ms`（[server.rs:106-130](../../crates/docparse-cli/src/server.rs#L106-L130)）。CLI 路径**零计时**。
- 跑大文件（几十上百页 PDF，或 `--ocr` 扫描件）时，用户无法判断"在跑还是卡了""快跑完了没""到底有多快"。对一个把速度当卖点的工具，这是**体验缺口也是营销缺口**——快，但没人看见快。

**诉求**（用户原话归纳）：在命令行里有可视化呈现，能看到文档处理速度的情况（如实时处理速度）。

## 2. 目标与非目标

**目标（In Scope）**
- G1 CLI 交互模式下，实时呈现处理进度（哪一相位 / 第几页 / 百分比）。
- G2 呈现速度指标：吞吐（页/秒、MB/秒）、各相位耗时、总耗时。
- G3 结束时给一行可读的"速度小结"（总页数 / 总耗时 / 平均吞吐）。
- G4 默认开启但**零污染**：管道 / 重定向 / CI / MCP / REST 下自动静默或降级为结构化输出。

**非目标（Out of Scope，本期不做）**
- N1 不改 `DocumentParser` trait 的一次性语义（见 §4 约束）——不为进度去做大手术。
- N2 不做 TUI 全屏界面 / 不引入交互控件。
- N3 不改 MCP / REST 协议输出（它们走结构化时延字段，不是进度条）。
- N4 不做跨文件批处理的总进度（当前 CLI 一次一个输入）。

## 3. 用户场景

| 场景 | 现状 | 期望 |
|---|---|---|
| S1 跑百页数字 PDF `-f markdown` | 全程静默，几秒后吐结果 | 进度条 / spinner + 结束 "120 页 / 1.4s / 86 页·s⁻¹" |
| S2 `--ocr` 扫描件（慢，模型推理） | 仅结束时一行 JSON 计数 | 逐页进度 + 实时页/秒，看得出 OCR 在动 |
| S3 多相位 `--ocr --layout --table-model` | 三段 JSON，无相位耗时 | 分相位进度（解析→OCR→版面→表），每段耗时 |
| S4 `... -f json > out.json` 重定向 | （同 S1） | **不输出进度到 stdout**；进度去 stderr 或自动关 |
| S5 CI / 脚本里跑 | 同 S1 | 静默或 `--progress=json` 机器可读流 |

## 4. 架构约束（决定可行性，开工前必读）

研究自 [main.rs](../../crates/docparse-cli/src/main.rs)、[parser.rs](../../crates/docparse-core/src/parser.rs)、[enhance.rs](../../crates/docparse-core/src/enhance.rs)、[pdf/lib.rs](../../crates/docparse-pdf/src/lib.rs)：

1. **解析是一次性的，且开跑前不知页数。** `DocumentParser::parse(&self, path) -> Result<Document>`（[parser.rs:1-19](../../crates/docparse-core/src/parser.rs)）一把梭返回完整 `Document`，**无回调、无流式、无预先页数**。页数只在 `Document.pages` 拿到、即解析**完成之后**。→ 基础解析相位**无法开箱给出"第 N/M 页"**，除非动 trait（N1 排除）或在 PDF 后端单独埋点。
2. **页级并行无 per-item 完成钩子。** PDF 内容流解释走 `inputs.par_iter().map(interpret).collect()`（[pdf/lib.rs:102](../../crates/docparse-pdf/src/lib.rs)）；OCR 增强走受限池 `par_iter`（[enhance.rs:124-187](../../crates/docparse-core/src/enhance.rs)，`MAX_PAGE_PARALLELISM=8`）。rayon 不暴露"某项完成"事件。→ 想要页级进度，需在 map 闭包内自增 `AtomicUsize` + 另一线程定时读计数渲染（侵入小、保序输出不受影响），**不能**指望 rayon 原生回调。
3. **CLI 主流程天然是离散相位序列。** [main.rs:621-800](../../crates/docparse-cli/src/main.rs)：`parse → image export/embed → ocr → layout → table → formula → transcribe → vlm → drop placeholders → output`。每相位已是清晰的 `if cli.xxx { ... eprintln!(计数) }` 块。→ **相位级进度（含每相位耗时）几乎零成本**，是性价比最高的一层。
4. **必须按通道分流。** stdout 是数据出口（`println!(rendered)` 或 `-o` 写文件，[main.rs:797-800](../../crates/docparse-cli/src/main.rs)）；进度只能去 **stderr**。已有 `std::io::IsTerminal` 在用（OCR 下载确认，[main.rs:277](../../crates/docparse-cli/src/main.rs)）→ 复用它判交互；非 TTY 自动静默/降级。MCP 走 stdio JSON-RPC（[mcp.rs](../../crates/docparse-cli/src/mcp.rs)）**绝不能**被进度污染；REST 已有时延头，不动。
5. **无进度/TUI 依赖。** 根 `Cargo.toml` `[workspace.dependencies]` 无 `indicatif`/`atty`/`is-terminal`（终端判断用 stdlib `IsTerminal`）。→ 要么引 `indicatif`（成熟、stderr 友好、自带速率/ETA），要么手搓 `\r` 刷新行（零依赖，但要自己处理速率/终端宽度/非 TTY）。依赖按通用规范"先问"。

**约束小结**：相位级进度 = 低成本高价值（约束 3+4）；页级进度 = 中成本（约束 2，需 atomic + ticker）；基础解析的页内进度 = 高成本（约束 1，触 trait，本期不做）。

## 5. 功能需求

| 编号 | 需求 | 优先级 |
|---|---|---|
| FR1 | 交互 TTY 下默认显示进度（相位名 + spinner/bar），输出到 stderr | P0 |
| FR2 | 结束打印速度小结：总页数、总耗时、平均吞吐（页/s、MB/s） | P0 |
| FR3 | 每相位耗时（解析 / OCR / 版面 / 表 / …）单独计时并展示 | P0 |
| FR4 | 非 TTY（管道/重定向/CI）自动**不**输出进度到 stdout | P0 |
| FR5 | `--progress <auto\|always\|never>` 显式控制；`--quiet` 全关 | P1 |
| FR6 | 页级实时进度："第 N/M 页"+ 实时页/s（至少覆盖 OCR 慢相位） | P1 |
| FR7 | `--progress=json` 输出机器可读进度事件流（CI/上层封装用） | P2 |
| FR8 | OCR/版面相位显示模型加载与逐页推理两个子阶段 | P2 |

**速度指标定义**（FR2/FR3）
- 吞吐：`pages / wall_seconds`（页·s⁻¹）；`input_bytes / wall_seconds`（MB·s⁻¹）。
- 相位耗时：每个 `if cli.xxx` 块包 `Instant::now()`/`elapsed()`，与现有 JSON 计数并排或合并。
- 与现状兼容：保留现有 `{"...": N}` 计数语义（脚本可能在解析），新增计时为附加字段或独立行。

## 6. 候选方案

### 方案 A — 相位计时 + 结束小结（最小，零新依赖）
每相位包计时，stderr 打印 `[2/4] OCR … 0.81s`，结束打印 `✓ 120 页 · 1.42s · 84.5 页/s · 12.3 MB/s`。无实时刷新，无进度条。
- ✅ 改动最小（只动 [main.rs](../../crates/docparse-cli/src/main.rs) 相位块）、零依赖、不触 §4 难点、立即满足 FR2/FR3 与"看见速度"的核心诉求。
- ❌ 无页级实时反馈（慢 OCR 期间仍像"卡住"）。

### 方案 B — indicatif 相位/页进度条（推荐）
引 `indicatif`，相位级 `MultiProgress`，OCR/版面相位接 §4-2 的 `AtomicUsize` + ticker 喂"N/M 页 + 页/s + ETA"。结束小结同 A。TTY 判断 + `--progress` 控档。
- ✅ 满足 FR1/2/3/6，速率/ETA/终端宽度自适应 indicatif 自带、stderr 友好；体验最完整。
- ❌ 引一个依赖（按规范先问）；页级需在 PDF 解析 / enhance 闭包埋 atomic（侵入可控）。

### 方案 C — 全量页级流式（含基础解析页内进度）
改 `DocumentParser` 或 PDF 后端暴露逐页回调，连基础解析都逐页报。
- ✅ 最细。 ❌ 触 N1（trait 大改）、跨格式都要适配，**本期否决**。

**倾向**：先落 **方案 A 作为 P0 地板**（无依赖、马上有价值），再在确认引 `indicatif` 后叠加 **方案 B 的页级进度**。C 留作未来 trait 演进话题。

## 7. 范围切分（建议）

- **M0（P0，方案 A）**：相位计时框架 + 结束速度小结 + TTY/非 TTY 分流 + `--progress/--quiet`。无新依赖。
- **M1（P1，方案 B 增量）**：引 `indicatif`，相位进度条 + OCR/版面页级实时页/s（atomic+ticker）。
- **M2（P2）**：`--progress=json` 事件流 + 子阶段（模型加载 vs 推理）细分。

## 8. 验收标准

- AC1 跑百页 PDF，TTY 下见相位进度，结束有 `页数/耗时/页·s⁻¹/MB·s⁻¹` 小结。
- AC2 `docparse f.pdf -f json > out.json`：`out.json` **不含**任何进度字节（进度全在 stderr 或被关）。
- AC3 `--quiet` / 非 TTY 默认：stdout 纯净，行为与现状一致（现有 JSON 计数语义不回归）。
- AC4 MCP / REST 输出**逐字节不变**（进度逻辑只在 CLI 一次性路径生效）。
- AC5 进度开销可忽略（atomic 自增 + 定时刷新，不显著拖慢端到端；三件套回归字节不变）。
- AC6 `--ocr` 慢相位期间有可见的逐页推进（M1）。

## 9. 待决问题

1. **是否引 `indicatif`？**（依赖按通用规范先问）否则方案 B 改手搓 `\r` 刷新。→ 决定 M1 形态。
2. 默认行为：TTY 默认**开**进度（推荐，符合"让速度可见"）还是默认关、`--progress` 才开？
3. 速度小结的"页数"对非 PDF（DOCX/HTML/XLSX…合成布局）是否仍有意义？需按格式给合适的计量单位（页/节/行）。
4. 现有相位 JSON 计数：合并进新计时输出，还是并存？（避免破坏可能在解析它的脚本。）
5. `--progress=json` 的事件 schema 是否要与 MCP/REST 时延字段对齐，便于上层统一消费。

## 10. 落点速查（实施时）

| 改动 | 位置 |
|---|---|
| CLI flag（`--progress`/`--quiet`）| [main.rs](../../crates/docparse-cli/src/main.rs) `Cli` struct（clap derive） |
| 相位计时 + 小结 | [main.rs:621-800](../../crates/docparse-cli/src/main.rs) 主流程各相位块 |
| TTY 判断 | 复用 `std::io::IsTerminal`（[main.rs:277](../../crates/docparse-cli/src/main.rs) 同款） |
| 页级 atomic 计数（OCR）| [enhance.rs:124-187](../../crates/docparse-core/src/enhance.rs) `apply` 的 `process` 闭包 |
| 页级 atomic 计数（解析）| [pdf/lib.rs:102](../../crates/docparse-pdf/src/lib.rs) `par_iter().map(interpret)` |
| 依赖（如引 indicatif）| 根 `Cargo.toml` `[workspace.dependencies]` |
| 不可污染的通道 | [mcp.rs](../../crates/docparse-cli/src/mcp.rs)（stdio JSON-RPC）、[server.rs](../../crates/docparse-cli/src/server.rs)（已有 `x-docparse-ms`） |

---

## 实施记录（2026-06-18，M0+M1 一并落地）

按推荐方案落地：**方案 A（相位计时 + 速度小结，零依赖地板）+ 方案 B（indicatif 进度条 + OCR 页级条）一并实现**，默认 TTY 开（待决问题 1=引 indicatif、问题 2=默认开，均按推荐定。）

**改动**
- 新增 [crates/docparse-cli/src/progress.rs](../../crates/docparse-cli/src/progress.rs)：`Reporter`（持运行时钟 + 相位计时表 + 开关）/ `PhaseGuard`（Drop 时记录相位耗时并清条）/ `ProgressMode{auto,always,never}`。spinner 给未知长度相位（base parse），page_bar 给已知页数的 OCR 相位。**全程 stderr**，`auto` 用 `stderr().is_terminal()` 判 TTY。
- [crates/docparse-cli/src/main.rs](../../crates/docparse-cli/src/main.rs)：加 `--progress <auto|always|never>` + `--quiet`；主流程每相位（parse / ocr / layout / table / formula / transcribe / vlm）裹计时，guard 紧贴重活、在各相位既有 `eprintln!` JSON 计数**之前**析构（避免与活动进度条同写 stderr 串字）；结尾 `reporter.finish()` 打速度小结。
- [crates/docparse-core/src/enhance.rs](../../crates/docparse-core/src/enhance.rs)：抽出 `process_page` 纯函数；新增 `apply_with(doc, enhancers, on_page: Option<&(dyn Fn()+Sync)>)`，`apply` 委托之。回调在页并行闭包内每页触发喂进度条；`core` **不引** indicatif（回调是 CLI 传入的 trait object，保持分层）。输出与 `apply` 逐字节一致（新单测 `parallel_apply_preserves_order_and_is_deterministic` 守）。
- 依赖：根 `Cargo.toml` + cli `Cargo.toml` 加 `indicatif = "0.17"`（成熟、stderr 友好、`ProgressBar` 内部 `Arc` 故可被 rayon worker 并发 `inc`）。

**验收（实测）**
- AC1/FR2/FR3 ✓ `--progress=always` 出 `✓ <file> · N pages · X MB · T s · P pages/s · M MB/s`；多相位再加一行 `parse 0.01s · ocr 0.28s`。
- AC2 ✓ `-f json --progress=always > out.json`：stdout 零进度字节（`grep -c pages/s out.json` = 0）。
- AC3 ✓ `--progress=never` 与 `auto`（非 TTY 管道）下 stderr 字节 = 0；现有相位 JSON 计数语义不回归。
- 数据不变 ✓ 同输入 progress never/always 的 stdout `shasum` 相同（json + markdown 各验）；§1 三件套 text 输出不受影响。
- 质量门 ✓ `cargo fmt --check` 净、`cargo clippy -p docparse-cli -p docparse-core` 零 warning、core+cli 75 单测通过（含 enhance 确定性）。

**演示素材**：5 篇论文 PDF 置于 `/tmp/docparse-demo-papers/`（仓外，遵 §6 端到端样例不进 repo）——workspace 自带 `1901.03003`（15 页 / 1.47 MB，release ~0.01s ≈2500 页/s）、`2408.02509v1`（14 页）解析通过并出小结；另从 arXiv 拉的 `1706.03762`/`2005.14165`/`1810.04805` 命中 PDF 后端既有 xref 限制（`invalid start value`，与本改动无关）。OCR 页级条在 `chinese_scan.pdf --ocr` 上验证相位拆分（`parse 0.01s · ocr 0.28s`）。

**未做（留后续）**：FR6 多页扫描的页级条动画（手头无多页扫描样例，机制已通）、FR7 `--progress=json` 事件流、方案 C 基础解析页内进度（触 trait，本期否决）。
