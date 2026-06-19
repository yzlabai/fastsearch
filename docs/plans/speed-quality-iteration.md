# 速度 / 质量提升迭代计划

> 状态：**S1/S2/S3 全部实施完成（2026-06-19）**。起因：用户问"速度和质量还有没有可提升的"。
> 验收：全工作区套件绿 + clippy 零 warning；三件套字节不变；批量 `--jobs 8` vs `--jobs 1` 实测 **~3× 吞吐**（CPU util 346%→680%）、产物逐字节一致；OCR 并行各 override 值产物逐字节一致。
> 经实测盘点（见 §0），确定性快路径已到天花板（born-digital sub-20ms、453% 多核利用），
> 剩余真杠杆只在 **模型路径并行**、**图像解码盲区**、**批量文件级并行** 三处。本文把这三项排成可验收里程碑。
> **与代码不符以代码为准并回写本文。**

## 0. 背景与盘点（为什么是这三项）

实测（release，18 核，`--stats`）：
- `lorem`/`bialetti` wall <1ms；`1901.03003`/`2408.02509v1` 10–20ms、CPU 利用 364%–453%。
- 确定性路径已 rayon 页并行 + `lto=thin`/`codegen-units=1`/`strip`——**born-digital 没有可榨油水**。

排除掉的非杠杆：
- `target-cpu=native` / `lto=fat`：CPU 密集路径几个 %，但**破单二进制可分发性**（与"边缘/内网分发"定位冲突）——不做。
- 大表 / CJK 复杂版面质量：Phase 6 已逐一证伪便宜旋钮，是 UniRec-0.1B 固定输入的模型天花板，真杠杆=更大表模型或 `--vlm-tables` 服务，**在"内嵌+保速"信封外**——本轮不碰。

确认的三个真杠杆（用户已确认全做）：

| 项 | 维度 | 一句话 | 背书 |
|---|---|---|---|
| **A** | 速度 | OCR 页并行上限从硬编 8 改为按可用内存自适应 | memory note：8→18 线程实测 5.5×→10×，现被固定 8 卡住 |
| **B** | 质量 | 补图像解码盲区：JPX 扫描 + 16/4/2-bit 低位深 + 1-bit 调色板 | `images.rs` 四处 `// TODO`：当前 position-only=丢内容 |
| **C** | 速度 | 批量 `--jobs N` 文件级并行（确定性路径） | cli-experience-iteration I7（原标"需 spike"） |

现状锚点：
- 并行池：[core/enhance.rs](../../crates/docparse-core/src/enhance.rs) `MAX_PAGE_PARALLELISM=8` + `ocr_pool()`（`OnceLock` 一次性构建）。
- 图像解码：[pdf/images.rs](../../crates/docparse-pdf/src/images.rs) `XImage::decode`/`decode_bilevel`/`build_images_from_resources`。
- 批量：[cli/batch.rs](../../crates/docparse-cli/src/batch.rs) `run` 的 `for inp in &files` 串行循环 + `RunModels`（`OnceLock` 已 `Sync`）。

---

## 1. 里程碑总览

| 里程碑 | 项 | 主题 | effort / 风险 |
|---|---|---|---|
| **S1** | A | OCR 页并行自适应内存 | S / 低 |
| **S2** | B | 图像解码盲区（JPX + 低位深 + 1-bit 调色板） | M / 低 |
| **S3** | C | 批量 `--jobs` 文件级并行（确定性路径，OCR/模型档强制小上限） | M / 中 |

落地顺序：S1（最干净、有 measured headroom）→ S2（质量真缺口）→ S3（吞吐，含安全闸）。**三项均已落地。**

> **已落地小结**（2026-06-19）：
> - **S1** `enhance.rs`：`desired_parallelism(cores, ram)` 按"总物理内存一半 / 100MB/页"定线程数、上限=核数；`total_physical_ram()`（libc，macOS `hw.memsize` / Linux `_SC_PHYS_PAGES`）；`DOCPARSE_OCR_PARALLELISM=N` 显式覆盖。固定 8→18 核机吃满核。3 个纯函数单测 + 现有"并行==串行"守门。
> - **S2** `images.rs`：JPX（`hayro-jpeg2000`，Gray/RGB→Gray8/Rgb8，CMYK/alpha 仍 position-only）；16-bit 高字节降采样；2/4-bit DeviceGray 行对齐解包；1-bit Indexed 2-entry 调色板按亮度定极性折入 `invert`。5 个单测。CMYK-JPX/RGB 低位深/复杂 indexed 仍 position-only（标 TODO）。
> - **S3** `batch.rs` + `Cli.jobs`：`--jobs N` 默认 1=原串行路径（逐字节不变）；`effective_jobs(req,cores,files,has_model)` 纯函数（模型 flag→强制 1，否则 clamp 到 cores∧files）；`>1` 走有界 rayon 池 `par_iter` 索引保序收集；模型档被闸时 stderr 提示一行。`effective_jobs` 单测 + e2e 串/并产物一致。

---

## 2. 明细

### S1 · OCR 页并行自适应内存（A，P0）

**问题**：[enhance.rs:24](../../crates/docparse-core/src/enhance.rs#L24) `MAX_PAGE_PARALLELISM=8` 是硬编死值，
带 `// TODO: make this adaptive to available memory`。在大内存/高核机上 8→18 线程实测 5.5×→10×，
现在被固定 8 **白白丢掉接近一倍吞吐**；同时在**小内存高核**机上 18 线程 ×100MB/页 buffer 又可能爆。
两头都该由"可用内存"而非固定常数定。

**方案**：把 `ocr_pool()` 的线程数从 `clamp(1, 8)` 改为按内存预算自适应：
```
per_page  = 100 MB                      // 扫描页 buffer 经验值（保留 MAX_PAGE_PARALLELISM 注释里的口径）
budget    = total_physical_ram * 1/2    // 给 scan buffer 的内存预算（留一半给模型/系统）
threads   = clamp( min(cores, budget / per_page), 1, cores )
```
- **内存来源**：`libc`（**本就在树**，resources.rs 的 getrusage 先例，零新供应链面）。Linux=`sysconf(_SC_PHYS_PAGES)*_SC_PAGE_SIZE`；macOS=`sysctlbyname("hw.memsize")`。取**总物理内存**（可移植、稳定），按比例预算，不去查"可用"（平台分散、易抖）。取不到→回退旧值 8。
- **显式覆盖**：`DOCPARSE_OCR_PARALLELISM=N` 环境变量直接钉线程数（可测、可压、CI 可固定）。
- **上限仍 ≤ cores**：自适应只在"内存够"时放开到核数，绝不超核数过订阅。

**效果预期**：18 核 / 32GB → `min(18, 16GB/100MB=160)=18`（吃满核，对齐 10× headroom）；
4 核 / 8GB → `min(4, 40)=4`（不变）；16 核 / 4GB → `min(16, 20)=16`（peak ~1.6GB < 2GB 预算，安全）。

**影响面**：仅 `enhance.rs` 的 `ocr_pool()` + 一个平台内存小助手 + `docparse-core` 加 `libc` 依赖（已在 workspace）。
`apply`/`apply_with`/`process_page` **签名不变**，调用侧零改动。

**验收**：
- 新单测：`DOCPARSE_OCR_PARALLELISM` 覆盖生效；自适应公式在给定 (cores, ram) 下产出期望线程数（纯函数抽出来测，不依赖真机）。
- 现有"并行==串行+字节确定"守门测试不破。
- 三件套字节不变、`chinese_scan --ocr` 逐字不变。
- 实测 18 核机 `--stats` 多页扫描 util% 较改前上升（8→更高线程）。

### S2 · 图像解码盲区（B，P1）

四处 `// TODO`，当前都 `ImageKind::None`=可审计但 OCR 拿不到像素=**丢内容**。按价值/难度排：

**S2a · JPX（JPEG 2000）扫描** —— [images.rs:15/119](../../crates/docparse-pdf/src/images.rs#L15)
- 现状：`JPXDecode` 落 `ImageKind::None`。
- 方案：`hayro-jpeg2000`（**已是 hayro 传递依赖**，同 ccitt/jbig2 先例，加为 docparse-pdf 直接 workspace 依赖=零新供应链面）。
  `Image::new(bytes, &DecodeSettings::default())` → `.color_space()` 判通道 → `.decode()` 得 **8-bit 交错**（库已归一到 8-bit，`original_bit_depth` 仅信息性）。
  映射：`Gray→Gray8`、`RGB→Rgb8`；`ICC/Unknown{1|3}` 按 `num_channels` 归一；**CMYK / has_alpha → 仍 position-only**（与现有 CMYK-JPEG 一致，不冒色错风险）。
- 落点：`decode()` 的 DCT 分支同级加 `JPXDecode` 分支（裸单 filter 链，与 DCT/CCITT 同口径），或在 `decode_bilevel` 外提一个 `decode_jpx()`。注意 JPX 不是 1-bit，走 `decode()` 顶层而非 bilevel。

**S2b · 16-bit 深度** —— [images.rs:361](../../crates/docparse-pdf/src/images.rs#L361)
- 现状：`bpc` 非 8/1 直接 `continue`。
- 方案：`bpc==16` 时，`decompressed_content()` 得大端 16-bit/通道样本，**取高字节降到 8-bit**；通道数由 `len/(px*2)` 推（1→Gray8、3→Rgb8），与现有 8-bit 路径同构。16-bit 是高质量扫描/照片的常见档，价值最高、最直。

**S2c · 2/4-bit 低位深** —— [images.rs:362](../../crates/docparse-pdf/src/images.rs#L362)
- 方案：`bpc∈{2,4}` 时按**行字节对齐**解包到 8-bit（`stride=ceil(w*comps*bpc/8)`），样本值线性放大到 0..255（`v * 255 / (2^bpc-1)`）。通道数需从 `/ColorSpace` 读（DeviceGray=1 / DeviceRGB=3）——**sub-byte 不能靠字节数反推通道**（行 padding 致歧义）。范围保守：先只做 **DeviceGray** 灰度（扫描最常见）；RGB 低位深罕见，留 position-only + `// TODO`。

**S2d · 1-bit Indexed 调色板** —— [images.rs:366](../../crates/docparse-pdf/src/images.rs#L366)
- 现状：1-bit Indexed 一律 `continue`（怕 index0=白致反相）。
- 方案：读 2-entry 调色板（`/ColorSpace [/Indexed base hival lookup]`），按 base 色空间算两个 entry 的灰度，**亮者→白、暗者→黑**映射，消除极性歧义。范围谨慎：仅 `hival==1`（2 entry）的 DeviceGray/DeviceRGB base；复杂 base 留 position-only。

**影响面**：`images.rs` 单文件；docparse-pdf 加 `hayro-jpeg2000` 依赖。`XImage` 可能需多带 `/ColorSpace` 通道数（S2c/d）——build 时解析存字段，`decode()` 在 worker 线程无文档访问（守现有约束）。

**验收**：
- **必须有真实/构造样例验证逐项**（无样例的档明确标 `// TODO: 无样例`，不臆造解码）。优先找/造 JPX、16-bit、4-bit gray、2-entry indexed 各一。
- 单测：JPX 通道映射、16-bit 高字节降采样、低位深解包 stride、调色板极性。
- **回归红线**：三件套字节不变；现有 CCITT/JBIG2/DCT 扫描逐字不变（新分支只接管原 `None` 档）。
- 近似必标注（CMYK-JPX / RGB 低位深 / 复杂 indexed 仍 position-only，写明 TODO+影响）。

### S3 · 批量 `--jobs N` 文件级并行（C，P2）

**问题**：[batch.rs](../../crates/docparse-cli/src/batch.rs) `for inp in &files` **串行**跨文件；
大量**小数字 PDF** 时单文件内页并行吃不满核，文件级并行可提吞吐。

**约束（I7 风险分析）**：
- **内存闸**：OCR/扫描页 buffer ~100MB/页 + 模型（~700MB）。文件级并行成倍放大→**`--ocr`/UniRec/layout/vlm 任一开启时强制 `jobs=1`**（或极小上限），只对**纯确定性**批量放开并行。
- **嵌套 rayon**：文件级池 × 页级池双层；确定性单文件页并行本就轻（小文件页少），文件级用**独立有界池**（`min(jobs, cores)`），避免与页级过订阅。
- **保序/聚合**：`stats` 用索引收集保序（报告/JSON 事件顺序不依赖完成序）；`RunModels` 的 `OnceLock` 已 `Sync` 可共享；`--progress json` 的流式 file 事件在并行下改为**完成即发**（顺序无关，schema 不变）。

**方案**：
- `Cli` 加 `--jobs N`（默认 1=现状，`0`/缺省=1）。
- 安全闸：`effective_jobs = if any_model_flag { 1 } else { min(jobs, cores) }`；`any_model_flag = ocr | table_model | formula_model | transcribe_model | layout | vlm_*`。当请求 `>1` 却被闸到 1 时，**stderr 提示一行**（不静默降级）。
- 实现：`effective_jobs==1` 走原串行循环（**逐字节不变**）；`>1` 时用有界 rayon 池 `into_par_iter().map(...).collect()` 收集 `FileStat`，**保序**；进度条/JSON 事件在收集后或回调内发。
- 文档/帮助标注：`--jobs` 仅对确定性批量生效；模型档自动串行（内存）。

**影响面**：`batch.rs` `run` + `Cli` 加一字段。串行路径保持原样（`effective_jobs==1` 分支）。

**验收**：
- 单测：`effective_jobs` 在有/无模型 flag、不同 jobs/cores 下的取值；并行收集结果**保序**且与串行同集合。
- 实测：纯数字小 PDF 批量 `--jobs 8` vs `--jobs 1` 吞吐↑；`--ocr --jobs 8` 自动回落串行 + 提示。
- `--jobs 1`（默认）批量产物逐字节不变。
- clippy 零 warning、全工作区套件绿。

---

## 3. 非目标

- 大表/CJK 模型质量（模型天花板，信封外）。
- `target-cpu=native`/`lto=fat`（破可分发性）。
- CMYK-JPX/JPEG 色彩正确转换（APP14/ICC，单独议）。
- 文件级并行叠加 OCR 内存池（明确禁用，非延后）。

## 4. 跨项不变量（都要守）

- 坐标/字形宽度/分层不变量（CLAUDE.md §3）恒守。
- 解析失败页→空 Page 不 panic；新解码分支失败回退 `ImageKind::None` 而非 panic。
- 近似必标注；clippy 零 warning；字体/解码改动跨三件套回归。
- 单文件路 / `--jobs 1` / 不开新 flag 时**逐字节不变**。
