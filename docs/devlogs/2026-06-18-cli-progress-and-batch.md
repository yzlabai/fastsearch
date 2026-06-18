# devlog · CLI 处理速度可视化 + 文件夹批量解析(2026-06-18)

一句话:**给 CLI 补上两块缺失的体验——① 实时进度/速度可视化(让"速度快"这个卖点看得见),② 文件夹/多文件批量解析 + 聚合统计报告。两者共用一套从单文件抽出的管线,默认 TTY 开、非交互零污染,坏文件不中断整批。**

需求分析:[plans/cli-progress-visualization.md](../plans/cli-progress-visualization.md)。功能说明:[cli-batch-and-progress.md](../cli-batch-and-progress.md)。

## 缘起

用户两连问:先"想要命令行里能看到处理速度的可视化",随后"能不能指定文件夹一次解析所有文件并出统计报告"。读代码核对现状:

- CLI 一次性跑完才出结果,**纯解析快路径全程静默**;唯一计时埋点在 REST 的 `x-docparse-ms`,CLI 路径零计时。
- 输入是单个 `input: Option<PathBuf>` → 一个 backend → stdout/`-o`;**全仓无任何目录/glob/批量处理**。每个 parser 的 `supports()` 是按扩展名判,正好够批量时筛文件。

两块都是"基础设施缺口",且第二块天然复用第一块的每文件速度小结。

## 关键约束(决定形态)

1. **解析一次性、开跑前不知页数**:`DocumentParser::parse` 一把梭返回完整 `Document`,无回调无流式。→ 基础解析只能给 spinner(未知长度),给不了"第 N/M 页"。
2. **rayon 页并行无 per-item 钩子**:PDF 页解释 / OCR 增强都走 `par_iter`。→ 页级进度靠在 map 闭包内触发回调 + 线程安全进度条自增。
3. **离散相位序列**:`parse → ocr → layout → table → formula → transcribe → vlm → output`,每相位已是清晰的 `if cli.xxx` 块。→ 相位计时几乎零成本。
4. **通道分流**:stdout 是数据出口,进度只能去 **stderr**;非 TTY(管道/CI/MCP/REST)必须静默。复用已有的 `std::io::IsTerminal`。
5. **批量并行有内存闸**:OCR 走内存受限池(≤8,~100MB/扫描页);叠文件级并行会爆这个上限。→ 批量**串行**,每文件内部已吃满核。

## 做了什么

### ① 进度/速度可视化(方案 A 地板 + 方案 B 进度条一并落地)

引 `indicatif = "0.17"`(成熟、stderr 友好、`ProgressBar` 内部 `Arc` 故可被 rayon worker 并发 `inc`)。

- 新增 [cli/progress.rs](../../crates/docparse-cli/src/progress.rs):`Reporter`(运行时钟 + 相位计时表 + TTY 开关)/ `PhaseGuard`(Drop 时记录相位耗时并清条)/ `ProgressMode{auto,always,never}`。spinner 给未知长度相位、page_bar 给已知页数的 OCR 相位、files_bar 给批量。
- `core/enhance.rs`:抽出 `process_page` 纯函数;新增 `apply_with(doc, enhancers, on_page: Option<&(dyn Fn()+Sync)>)`,`apply` 委托之。回调在页并行闭包内**每页触发一次**(增强或透传都触发,使进度条能到总数)。**`core` 不引 indicatif**——回调是 CLI 传入的 trait object,守"core 不依赖上层"分层。
- CLI 加 `--progress <auto|always|never>` + `--quiet`;每相位裹计时,guard 紧贴重活、在既有 `eprintln!` JSON 计数**之前**析构(否则与活动进度条同写 stderr 串字);结尾 `Reporter::finish` 打速度小结 `✓ <file> · N pages · X MB · T s · P pages/s · M MB/s` + 多相位时追加 `parse … · ocr …` 拆分行。

### ② 文件夹批量 + 聚合报告

- 新增 [cli/batch.rs](../../crates/docparse-cli/src/batch.rs):`collect_files`(目录展开,默认顶层、`-r` 递归,按 `supports()` 筛扩展名,显式点名的文件一律纳入,排序去重)→ 逐文件跑管线 → `write_output` 落 `--out-dir/<原文件名>.<格式后缀>`(保全名避免 `a.pdf`/`a.docx` 撞 `a.json`)→ 聚合 `FileStat` → 渲染报告。
- **失败隔离**:单文件 parse 失败 = 一行 `ERROR: <首行>`,`stats.push` 后继续——把"坏页→空页不 panic"不变量延伸到文件级。
- 报告三形态(按用户选择):人读对齐表格→**stderr**(随进度开关)、`--report-json <FILE>`、`--report-csv <FILE>`(`csv_field` 处理逗号/引号/换行转义)。
- **共用管线**:从 `main` 抽出 `parse_and_enhance(input, cli, reporter: Option<&Reporter>)`(Some=单文件,带 spinner/页条 + 相位 JSON;None=批量,安静,files_bar + 报告代之)和 `render_doc(doc, cli)`。单文件路径改为调这两个,**行为逐字节不变**。
- CLI:`input: Option<PathBuf>` → `inputs: Vec<PathBuf>`;加 `--out-dir`/`-r`/`--report-json`/`--report-csv`;`Format` 补 `Copy`(`render_doc`/`write_output` 借引用 match)。子命令 match 改**借用**(`if let Some(cmd) = &cli.command`)避免部分移动,好把 `&cli` 整体传给新函数。

**批量/单文件判定**:`inputs.len()==1 && 是文件 && 无 --out-dir` → 老的单文件路(stdout/`-o`);否则批量。

## 测试

- `core`:新增 `apply_with_fires_callback_once_per_page_and_matches_apply`——12 页混合文档,`AtomicUsize` 验回调恰好每页一次,且输出与 `apply` 逐字段相等(回调不改结果)。
- `cli/progress.rs`:`mode_decides_enabled_deterministically`(always/never/quiet 组合)、`disabled_reporter_makes_no_bars`。
- `cli/batch.rs`:`output_ext` 映射、`csv_field` 转义、`truncate` 按字符、`totals` 计数、`render_table` 含行/总计/ERROR、`render_json` 结构、`collect_files_filters_recurses_and_includes_explicit`(顶层筛选 + 递归 + 显式纳入 + 缺失报错,用 pid 后缀临时目录,遵仓内无 tempfile crate 约定)。
- 全绿:**core 64 + cli 21**(原 12),clippy 零 warning,fmt 净。

## 实测验收(release)

- 单文件:`✓ 1901.03003.pdf · 15 pages · 1.47 MB · 0.01s · 2503 pages/s · 245 MB/s`;`parse 0.01s · ocr 0.28s` 相位拆分。
- 批量 5 篇论文文件夹:表格逐行(2 ok + 3 arXiv `invalid xref` ERROR——PDF 后端既有缺口,正好演示失败隔离),`--out-dir` 只写出成功的两份 `<name>.pdf.{md,json}`,JSON/CSV 报告结构正确。
- **不变量验证**:`--progress=never`/非 TTY 管道下 stderr 0 字节;同输入 progress on/off 的 stdout `shasum` 相同;批量 `--out-dir` 产物与单文件 `-o` **逐字节相同**(仅与 stdout 差一个 `println!` 尾换行,系既有行为)。

## 边角与待办

- **批量 OCR 模型重载**:`PpOcrEnhancer::new` 现仍每文件构造(ONNX 重载)。一批扫描件会被模型加载主导;后续可把模型加载提到批量循环外复用(改 `parse_and_enhance` 签名传预载模型),本期未做,已记。
- **递归同名落盘冲突**:不同子目录同名文件落 `--out-dir` 会撞(`a/x.pdf`、`b/x.pdf` → 都 `x.pdf.json`);v1 扁平命名,后续可镜像相对路径。
- `--progress=json` 事件流(FR7)、方案 C 基础解析页内进度(触 trait,否决)留后续。

## 续:收口两个待办(同日)

**① 批量模型复用**:批量原先每文件重载模型。发现代码已有现成模式——服务端 `OcrState`(`OnceLock` 惰性载 + 缓存)、`EnhanceState`(惰性 `UniRec`,~700MB,"每服务器只载一次"是初衷)。照搬到 CLI 路:新增 `RunModels`(持 `OcrState` + 3 个 `LazyUniRec`,各 `OnceLock<Result<UniRec,String>>`),`from_cli` 构造一次,`parse_and_enhance` 改签名收 `&RunModels`,OCR/table/formula/transcribe 从缓存取而非 `::new`。**全惰性**——纯数字 `--ocr` 批量永不触模型,守"数字零成本"不变量;批量首个扫描页载、其余复用。实测 batch `--ocr` 两份 scan:scan1 9.33s(含一次性载)、scan2 6.98s(仅推理),~2.3s 差正是被复用掉的加载。**版面模型未纳入**——`enhance_document`/`formula`/`transcribe` 收的是路径、内部 `LayoutModel::new`,连服务端也每次重载;复用需改这 3 个 docparse-ocr 公有签名(跨 crate),属更大改动,记入已知限制。

**② 递归落盘镜像**:`collect_files` 返回 `Vec<BatchInput>{path, rel}`——`rel` 是文件相对输入文件夹的路径(顶层/显式文件取裸文件名)。`write_output` 落 `--out-dir/<rel>.<后缀>` 并建父目录。实测 `alpha/paper.pdf` + `beta/paper.pdf`(同名异目录)→ `out/alpha/paper.pdf.md` + `out/beta/paper.pdf.md`,**不再互覆**。同一子目录同名仍覆盖(与源端本就重名一致)。

新单测:`collect_files` 验 `rel` 镜像(顶层裸名、递归带子路径、显式文件裸名)。单文件 `-f json` 逐字节不变(`shasum` 同前)、`--ocr` 逐字不变;全工作区 34 套件 + clippy 零 warning。
