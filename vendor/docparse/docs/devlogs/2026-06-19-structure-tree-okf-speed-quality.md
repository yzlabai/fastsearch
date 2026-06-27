# devlog · 结构树 + OKF 导出 + 速度/质量三杠杆（2026-06-19）

> 一次会话内连做四块、10 个提交。起点是用户问"速度和质量还有没有可提升的"，顺着"把结构暴露给 agent"一路做到 OKF 标准化产物层。全部确定性、信封内、零依赖；难版面交 `--layout`。
>
> 相关 plan：[speed-quality-iteration.md](../plans/speed-quality-iteration.md)、[document-structure-tree.md](../plans/document-structure-tree.md)、[2026-06-19-okf-export.md](../plans/2026-06-19-okf-export.md)；调研 [refer/document-structure-tree-for-agentic-retrieval.md](../refer/document-structure-tree-for-agentic-retrieval.md)；状态 [status.md](../status.md) Phase 10/11/12。

## 缘起

确定性快路径实测已到天花板（born-digital sub-20ms、CPU 利用 364%–453%）。盘点后真杠杆只在三处：模型路径并行、图像解码盲区、批量并行（→ Phase 10）。做完速度/质量，转向一个更大的产品问题：长文档/论文需要**结构树**便于 agentic 检索——调研发现这是"差最后一公里的免费奖品"（标题层级早已算出、Tagged StructTree 早已读，但都被拍扁）。于是一路做：结构树 → 书签喂层级 → 标题检测收紧 → OKF 标准化产物 → 管道/agent 分发。

## 做了什么

**Phase 10 · 速度/质量三杠杆**（`dd401cd`，[plan](../plans/speed-quality-iteration.md)）
- **S1 OCR 页并行自适应内存**：`enhance.rs` 固定 `MAX_PAGE_PARALLELISM=8` → `desired_parallelism(cores,ram)`（总内存一半 ÷ 100MB每页，上限核数；libc 取物理内存，`DOCPARSE_OCR_PARALLELISM` 覆盖）。18 核机从被卡 8 → 吃满核（解锁 Phase 8b 的 10× headroom），小内存高核反被内存闸住更安全。
- **S2 图像解码盲区**：JPX（`hayro-jpeg2000`）/ 16-bit 高字节降采样 / 2-4bit DeviceGray 解包 / 1-bit 2-entry 调色板按亮度定极性。只接管原 `None` 档=零回归。
- **S3 批量 `--jobs N` 文件级并行**：确定性档并行（实测 40 个小 PDF **≈3× 吞吐**），模型档强制串行防爆内存。

**结构树 Phase A**（`63e7608`，[plan](../plans/document-structure-tree.md)）
- 新 `core/outline.rs`：`Section{id,title,level,page,bbox,children}`，级别栈建树（id=标题出现序）。**派生 on-demand 不入 IR** → `-f json` 字节不变（设计偏离回写：原拟加 `Document.outline` 字段，改派生换字节不变 + 单一真源）。
- `chunk.rs` 加 `section_id`；`heading_path` 弃字号栈改走真实 `level` 栈（修非单调字号偏差）。**跨模块单测**证 chunk `section_id` 精确索引进树、面包屑与树一致。
- `-f outline` + MCP `outline` 工具（`id`/`max_depth`）+ REST `?format=outline`，三接口字节一致——把树暴露成 **agent 导航接口**（调研指出是当前 OSS 真空）。

**结构树 T2 · /Outlines 书签**（`8663397`）
- `pdf/outlines.rs`：读 catalog `/Outlines`（UTF-16BE 标题、`/Dest` 及 `/A`→GoTo、**命名目标**含 `/Names`→`/Dests` 名树 `Kids` 递归），**号码容错锚定**（剥前导节号 + 折空白/大小写，解 "Introduction" vs "1 Introduction"）打 `tag="H<level>"`——复用 tagged-heading 通道，**零 IR/core 改动**（选此法因 `Document{}` 字面量遍布 29 处）。安全降级：锚不上不打 tag、不覆盖已有 tag、无书签文档字节不变。

**OKF 导出 Phase 1**（`5eb3f34`，[plan](../plans/2026-06-19-okf-export.md)）
- 新 `core/okf.rs`：把结构树 + 带 `section_id` 的 chunks join 成 **Open Knowledge Format v0.1 bundle**（目录 md + YAML frontmatter，Google Cloud 厂商中立 RAG 交付格式）。一 Section 一 concept（`type`/`title`/`resource`(page+bbox)/`description`/`timestamp` + 扩展键）；目录嵌套镜像树；根 `index.md` 带 `okf_version`。slug 剥节号 + **截 64 字符防超长文件名**；**timestamp 用源 mtime**（自实现 `iso8601_utc` days-from-civil，无 chrono，绝不 wall-clock）。CLI `-f okf` 自动派生 `<stem>-okf/`、`--force`/非空报错、`--okf-resource-base`。**抓取 OKF SPEC §9 核实** conformance。

**OKF Phase 2 · 分发 + agent 接入**（`cc4e87c`）
- `Bundle::to_tar()` 确定性 ustar（固定 mode/uid/**mtime 0**，**手写无 tar 依赖**——tar crate 烙真实 mtime 破确定性）；CLI `--okf-tar`→stdout（实测 `|tar x` 还原==目录写出字节一致）；MCP `export_okf`；REST `?format=okf`→`application/x-tar`（独立 `render_okf_tar`，`render` 仍返 String 无改签）。

**结构树 Phase B · 标题检测收紧**（`0552cca`）
- `make_block` 标题判定整体加 `nchars ≤ 120` 闸（**门控全部路径含 tag**）——杀致密双栏 PDF 把跨栏合并行误判成的 200–500 字"标题"。残留 ~100 字跨栏碎片是 line-reconstruction 天花板，由 `--layout` 解决（实测 2408 `--layout -f outline` 产出真章节树）。

**文档同步**（`13dcc30` README、`a292cd0` SKILL.md）：结构树/outline/okf/`--jobs`/5 MCP 工具/`section_id` 全部回写到用户面文档。

## 验收

- **34 个测试二进制全绿**、clippy `--all-targets` 0 warning、fmt clean。
- 三件套（lorem/bialetti/1901）`-f json/markdown/text` + `chunks/outline` **逐字节不变**（所有新特性均叠加，不动既有路径）。
- 端到端 smoke 全通过：`-f outline`、`-f okf` 目录、`--okf-tar | tar t`、batch `--jobs`、chunks `section_id`、MCP `outline`/`export_okf`、REST `?format=okf`(application/x-tar)。
- 确定性硬证：okf 两次 `build` 字节相等、tar mtime 全零、`--okf-tar | tar x` == 目录写出。

## 教训

1. **"差最后一公里的奖品"**：结构树不需要新模型/新抽取——标题层级早已算出、StructTree 早已读，只差树化 + 暴露。先盘代码现状再判工作量，省掉一个臆想中的大工程。
2. **复用既有通道胜过加 IR 字段**：书签当作 H-tag 注入（复用 `tag_level`→`Block.level`→树），避开 29 处 `Document{}` 字面量的大改，且对无书签文档字节不变。
3. **确定性是硬约束、要贯穿到产物边界**：OKF timestamp 用源 mtime 不用 wall clock；tar 手写而非用 `tar` crate（后者烙真实 mtime）——否则 `git diff`/复现价值尽失。
4. **不变量要门控全路径**："标题必短"必须连 tag 路径一起卡，否则书签/StructTree tag 会绕过长度闸让合并行变标题。
5. **诚实划边界**：致密双栏 line-reconstruction 是模型天花板（`--layout` 覆盖），OKF consumer/RAPTOR 是信封外——写进 plan，不硬塞进确定性主流程。
6. **背靠规范核实**：抓 OKF SPEC §9 逐条对（`type` 必填、`index.md` 保留文件、绝对链接），而非凭印象实现新格式。

## 边界（本轮明确不做，理由见 plan/status）

OKF Phase 3 consumer（读 bundle 回 IR，docparse 是生产者）、RAPTOR 语义树（需 LLM，信封外）、更深双栏 line-reconstruction（高风险易回归多栏，`--layout` 已覆盖）、非中英 OCR（VLM 域）。
