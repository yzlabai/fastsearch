# CLI 进度可视化与批量解析

> docparse CLI 的两项体验功能:**实时进度/速度可视化** 与 **文件夹/多文件批量解析 + 聚合报告**。
> 实现见 [devlogs/2026-06-18-cli-progress-and-batch.md](devlogs/2026-06-18-cli-progress-and-batch.md);需求分析见 [plans/cli-progress-visualization.md](plans/cli-progress-visualization.md)。

## 1. 进度与速度可视化

跑文档时,CLI 在 **stderr** 实时显示进度,结束打一行速度小结。**默认在交互终端(TTY)自动开**,无需任何 flag;管道 / 重定向 / CI / MCP / REST 下自动静默,**绝不污染 stdout 数据**。

```bash
docparse paper.pdf -f markdown -o out.md
#   解析中:  ⠋ parse 0s            （青色 spinner，未知页数时转动）
#   结束:    ✓ paper.pdf · 15 pages · 1.47 MB · 0.01s · 2503.3 pages/s · 244.6 MB/s
```

多相位(如 `--ocr`)时,追加各相位耗时拆分,OCR 相位还有逐页进度条:

```bash
docparse scan.pdf --ocr -f text -o out.txt
#   ocr [==========>          ] 6/12 pages · 21/s · ETA 0s    （逐页爬动）
#   ✓ scan.pdf · 12 pages · 2.3 MB · 0.6s · 20.4 pages/s · 3.9 MB/s
#     parse 0.01s · ocr 0.58s
```

### 控制开关

| Flag | 作用 |
|---|---|
| `--progress auto` | **默认**。仅交互终端显示;管道/重定向/CI 自动关 |
| `--progress always` | 强制显示,即使输出被重定向(进度仍只走 stderr) |
| `--progress never` | 完全关闭 |
| `--progress json` | 机器可读 JSON-lines 事件到 stderr(无进度条/ANSI),供 CI/封装解析 |
| `--quiet` | 同 `--progress never`(连 json 也静默) |

### 机器可读事件(`--progress json`)

stderr 输出 JSON-lines(每行一个事件,stdout 仍是纯数据):

```bash
# 单文件:一条 summary 事件
docparse paper.pdf -f json --progress=json > out.json
#   stderr: {"event":"summary","scope":"file","file":"paper.pdf","pages":15,"bytes":...,"seconds":0.05,"pages_per_sec":292.4,"mb_per_sec":28.6}

# 批量:每文件完成即流式发 file 事件 + 收尾 summary
docparse ./papers -r --out-dir ./out --progress=json
#   {"event":"file","file":"alpha/paper.pdf","path":"...","bytes":...,"pages":15,"seconds":0.04,"ok":true}
#   {"event":"file","file":"beta/paper.pdf",...,"ok":true}
#   {"event":"summary","scope":"batch","files":2,"ok":2,"failed":0,"pages":29,"bytes":...,"seconds":0.15,"pages_per_sec":198.0}
```

`file` 事件 schema 与 `--report-json` 的每文件对象一致(失败文件带 `"error"` 字段、`"ok":false`)。

### CPU / 内存用量(`--stats`)

加 `--stats` 在运行结束时打一行资源用量到 stderr(`getrusage`,零额外依赖——libc 本就在依赖树里):

```bash
docparse paper.pdf --stats
#   resources: peak RSS 50.5 MB · CPU 0.44s (user 0.41 + sys 0.03) · 338% util · wall 0.13s

docparse ./papers -r --out-dir ./out --stats
#   （报告之后）resources: peak RSS 53.8 MB · CPU 0.75s ... · 517% util · wall 0.14s
```

- **peak RSS**:整个进程的峰值常驻内存。
- **CPU**:累计 CPU 时间(user+sys,跨所有线程)。
- **util%**:`CPU / wall` —— **>100% 是正常且期望的**,说明逐页并行/OCR 真的用上了多核。

`--stats` 是显式开关,无论 `--progress` 设成什么都打印(同 `--quality`/`--profile`);**但 `--progress json` 下改为发一条 `resources` 事件**(stdout 仍纯净):

```bash
docparse paper.pdf -f json --stats --progress=json > out.json
#   stderr: {"event":"resources","available":true,"peak_rss_bytes":50659328,"cpu_seconds":0.358,"cpu_util_percent":330.9,"wall_seconds":0.108,...}
```

非 Unix 平台 getrusage 不可用,会打印/标记 unavailable。

**速度指标**:页数、体积(MB)、墙钟(s)、吞吐(页·s⁻¹ 与 MB·s⁻¹)。各相位(parse / ocr / layout / table / formula / transcribe / vlm)单独计时。

**通道保证**:进度全在 stderr。`docparse f.pdf -f json > out.json` 时 `out.json` 不含任何进度字节;管道下默认静默。

## 2. 批量解析(文件夹 / 多文件)

给一个**文件夹**、**多个输入**,或带 `--out-dir` 任一,即进入批量模式:逐文件解析、按需落盘、结束打聚合统计报告。

```bash
# 整个文件夹 → 每文件一份 markdown 落到 out-dir，结束打统计表
docparse ./papers -f markdown --out-dir ./out

# 递归子目录
docparse ./papers -r -f json --out-dir ./out

# 多个文件 / 文件夹混合
docparse a.pdf b.docx ./more -f text --out-dir ./out

# 批量 OCR（坏文件不中断整批）
docparse ./scans -r --ocr --out-dir ./out

# 只要统计、不落盘内容（省 --out-dir）
docparse ./papers
```

进度条逐文件爬动,结束留下表格(到 stderr):

```
file                pages   MB     time    pages/s   status
1901.03003.pdf      15      1.47   0.01s   1807.4    ok
2408.02509v1.pdf    14      0.56   0.01s   1078.6    ok
broken.pdf          —       0.73   —       —         ERROR: failed parsing cross reference table: invalid start value
──────────────────────────────────────────────────────────────
3 files · 2 ok · 1 failed · 29 pages · 2.76 MB · 0.03s · 961.1 pages/s
```

### 落盘命名

`--out-dir/<原文件名>.<格式后缀>`,保留**完整原名**避免 `a.pdf` 与 `a.docx` 撞同一个 `a.json`:

| 输入 | `-f json` | `-f markdown` | `-f text` | `-f chunks` |
|---|---|---|---|---|
| `report.pdf` | `report.pdf.json` | `report.pdf.md` | `report.pdf.txt` | `report.pdf.json` |

只有**解析成功**的文件会写出;失败的只进报告。

### 报告形态

| Flag | 输出 |
|---|---|
| (默认) | 人读对齐表格 → **stderr**(随 `--progress` / `--quiet` 开关) |
| `--report-json <FILE>` | 机器可读 JSON:每文件 `{file, path, bytes, pages, seconds, ok, error?}` + `totals` |
| `--report-csv <FILE>` | 每文件一行 CSV(`file,path,bytes,pages,seconds,ok,error`,字段按需转义) |

三者可叠加。`--report-json` / `--report-csv` 无视进度开关、总会写文件。

### 行为约定

- **递归**:默认只扫文件夹**顶层**,`-r` / `--recursive` 才入子目录。
- **文件筛选**:文件夹里只挑后端支持的扩展名(pdf/docx/html/xlsx/pptx/md/csv/srt/tex/eml/图片/adoc);**显式点名的文件一律纳入**(即便扩展名陌生,交由解析时报错入表)。
- **失败隔离**:单个坏文件 = 一行 `ERROR: …`,**绝不中断整批**。
- **默认顺序、`--jobs N` 可文件级并行**:默认文件逐个跑(每文件内部已页级并行)。带 `--jobs N` 对**确定性档**开文件级并行;**任一模型 flag 开启时强制回落串行**防爆内存(详见下方「文件级并行」)。
- **模型加载一次**:OCR / UniRec(`--table-model`/`--formula-model`/`--transcribe-model`,~700MB)模型**整批只加载一次**并复用(惰性:纯数字或不带模型的批量永不加载),不再每文件重载。
- **递归落盘镜像**:递归时按相对路径镜像到 `--out-dir`(`sub/x.pdf` → `out/sub/x.pdf.json`),不同子目录的同名文件**不再相互覆盖**。

### 增强 flag 透传

所有增强选项(`--ocr` / `--layout` / `--table-model` / `--formula-model` / `--transcribe-model` / `--vlm-*`)在批量下对**每个文件**生效,与单文件语义一致。

### 文件级并行(`--jobs N`)

默认串行(`--jobs 1`)。大量**小数字 PDF** 时,单文件内部的页级并行吃不满核心(页少),加 `--jobs N` 让多个文件**同时**跑,把空闲核用起来:

```bash
# 40 个小数字 PDF：串行 ~0.34s vs --jobs 8 ~0.11s（≈3× 吞吐，产物逐字节一致）
docparse ./papers --out-dir ./out -f json --jobs 8
```

| 约定 | 说明 |
|---|---|
| **默认 1** | 不带 `--jobs` 或 `--jobs 1` = 原串行路径,产物**逐字节不变** |
| **仅确定性档** | 任一模型 flag(`--ocr` / `--layout` / `--table-model` / `--formula-model` / `--transcribe-model` / `--vlm-*`)开启时**强制回落 `jobs=1`**——OCR/扫描页 buffer(~100MB/页)+ 模型(~700MB)叠文件级并行会爆内存。被闸时 stderr 提示一行(不静默降级) |
| **上限** | 实际并行度 = `min(N, 核数, 文件数)` |
| **保序** | 按输入顺序索引收集,报告 / JSON 事件 / 落盘**顺序确定**,与串行一致 |

> 为什么模型档不并行:扫描 buffer 与模型显存/内存随文件数线性放大,文件级并行会突破"每文件内存受限"的设计闸。模型档的吞吐杠杆在**单文件内**的页级并行(见下方调优)。

### 性能调优:OCR 页级并行(`DOCPARSE_OCR_PARALLELISM`)

OCR/扫描的页级并行度**默认按可用物理内存自适应**(总内存一半 ÷ ~100MB每页,上限=核数)——大内存高核机自动吃满核,小内存机被内存闸住防 OOM,无需手动设。需要固定时(压测 / CI / 显存受限)用环境变量覆盖:

```bash
DOCPARSE_OCR_PARALLELISM=4 docparse scan.pdf --ocr     # 钉死 4 页并行
```

适用于单文件多页扫描与批量 OCR(与 `--jobs` 正交:`--jobs` 是文件级、本变量是页级)。

## 3. 与单文件模式的关系

单个文件、且没给 `--out-dir` 时,仍走经典单文件路:结果到 stdout 或 `-o`,行为**逐字节不变**。批量的落盘产物与单文件 `-o` 字节相同(仅与 stdout 输出差一个尾换行——stdout 走 `println!`、落盘走 `fs::write`,系既有行为)。

## 4. 已知限制

> 这些都已排进下一轮迭代计划 [plans/cli-experience-iteration.md](plans/cli-experience-iteration.md)。

- **模型整批只载一次**(已处理):OCR、UniRec、版面(`--layout`/`--formula-model`/`--transcribe-model`)模型在一次批量里**只加载一次**并复用(惰性,不带 flag 永不加载);服务端(MCP/REST)也改为每服务只载一次。
- **递归不跟随符号链接目录**(已处理):`-r` 不进入符号链接指向的目录(避免符号链接环无限递归爆栈);符号链接文件仍纳入。如需跟随,后续 `--follow-symlinks`(计划 I2 已修部分)。
- **同子目录同名仍覆盖**:递归已按相对路径镜像,但**同一**子目录下同名文件(或跨多个输入根的相同相对路径)仍会覆盖——这与源端本就重名一致。
- **文件级并行**(已处理):`--jobs N` 对确定性档开文件级并行(实测小 PDF 批量 ≈3× 吞吐);模型档强制串行防爆内存。见上方「文件级并行」。
