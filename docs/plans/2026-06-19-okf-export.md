# 计划：全面支持 Open Knowledge Format（OKF）导出

> 状态：**Phase 1 + Phase 2 已落地（2026-06-19）**· 复杂度：复杂（新输出格式 / 影响输出契约 / 跨 crate）· 责任人：—
>
> **Phase 2 已落地小结（分发 + agent 接入）**：`Bundle::to_tar()` —— **确定性 POSIX ustar**(固定 mode 0644/uid 0/gid 0/**mtime 0**/空 owner,同 bundle → 字节一致;**手写无 `tar` 依赖**,因 `tar` crate 烙真实 mtime 破确定性);ustar prefix/name 拆分长路径。CLI `--okf-tar` → tar 到 stdout(实测 `| tar x` 还原 == 目录写出逐字节一致)。MCP `export_okf` 工具(返回 `{okf_version, files:[{path,content}]}`,agent 直读不需解 base64)。REST `?format=okf` → `application/x-tar`(实测 `curl | tar t` 列出条目;走独立 `render_okf_tar`,`render` 仍返 String 无改签)。共用 `okf::build` + `okf_options_for`(basename+mtime,CLI/MCP/REST 三面同源)。验收:tar 确定性单测(两次 build 字节相等、512 对齐、双零块、ustar magic、mtime 全零)+ MCP export_okf 单测 + REST e2e tar;三件套 5 格式仍逐字节不变;clippy 0、34+ 套件绿。**表/图升格 concept(可选)未做**——按需。
>
> **Phase 1 已落地小结**：`core/okf.rs`（纯函数 emitter，join outline+chunk → `Bundle{files}`，`write_to`；frontmatter `type`/`title`/`resource`/`description`/`timestamp` + 扩展键 `section_id`/`page`/`bbox`；slug 剥前导节号 + 截断 64 字符防超长文件名；目录嵌套镜像树；根 `index.md` 带 `okf_version`，子目录 `index.md` 无 frontmatter）。CLI `-f okf`：`-o` 给则用、否则**自动派生 `<stem>-okf/`** 并 stderr 提示；派生且非空 → 报错(除非 `--force`)；`--okf-resource-base` 前缀；**timestamp 用源文件 mtime**（`iso8601_utc` 自实现 days-from-civil，无 chrono 依赖，绝不 wall clock）。批量 `--out-dir` 下每文件 `<rel-fullname>-okf/`（保全名免 a.pdf/a.docx 撞）。验收：8 单测(含 §9 conformance 自校验、确定性 `diff -r` 空、优雅退化、无 wall-clock)；三件套 json/md/text/chunks/outline **逐字节不变**；HTML/PDF e2e 产合法嵌套 bundle；clippy 0。**对照 OKF SPEC §9 核实**：`type` 必填非空、`index.md`/`log.md` 保留(仅根 index.md 可带 `okf_version`)、绝对 bundle 链接——均符合。
> **设计落点(回写)**：① slug **剥前导节号 + 截断 64 字符**（致密 PDF 的误检长"标题"会撑爆文件名,截断 + `NN-` 前缀保唯一）；② 根 index.md `source`/标题用**basename**(非全路径)保跨机确定性；③ 批量用 `<rel-fullname>-okf/`(单文件用 `<stem>-okf/`,前者重防撞、后者重可读)。
> **已知局限(诚实)**：bundle 质量受**上游标题检测质量**限制——致密双栏学术 PDF 的几何误检标题会产生噪声 concept(既有问题,非 OKF 引入;带书签处经 T2 已纠正;根治属结构树 Phase B 版面模型)。clean 文档(lorem/HTML/带书签报告)产物干净。
>
> 关联：[roadmap.md](../roadmap.md) module 4（结构树）、[refer/document-structure-tree-for-agentic-retrieval.md](../refer/document-structure-tree-for-agentic-retrieval.md) §6.2（把树暴露成检索面）、[agent-integration.md](../agent-integration.md)。
>
> 上游规范：Google Cloud **OKF v0.1**（2026-06-12 发布）。spec / 参考实现 / 样例 bundle 在 [`GoogleCloudPlatform/knowledge-catalog`](https://github.com/GoogleCloudPlatform/knowledge-catalog/tree/main/okf)（[SPEC.md](https://github.com/GoogleCloudPlatform/knowledge-catalog/blob/main/okf/SPEC.md)、[官方 blog](https://cloud.google.com/blog/products/data-analytics/how-the-open-knowledge-format-can-improve-data-sharing/)）。

---

## 0. 一句话

把 docparse-rs 已有的**结构树（Section 树）+ 带 `section_id` 的 chunks** 序列化成一个 **OKF bundle**（一目录 markdown + YAML frontmatter），让任意 agent / 知识库**零适配、git 原生、可溯源**地消费 docparse 的解析产物——而不是逼下游自己拼 JSON chunks 喂向量库。这是结构树方向的"**标准化产物层**"。

**关键判断（与结构树 Phase A 同源）**：OKF 导出**不需要改 IR、不需要新模型、不需要新抽取**。Section 树（[outline.rs](../../crates/docparse-core/src/outline.rs)）给骨架（title/level/page/bbox/children），chunks（[chunk.rs](../../crates/docparse-core/src/chunk.rs)，每 chunk 带 `section_id`）给血肉；OKF 导出＝在**输出层 join 这两者**。当前 Section 节点是 **heading-only**（无正文），OKF 正是补上"每节直属内容"的**强制函数**——而这个 join 在 emit 期完成，**`-f json` 等既有输出字节不变**（延续 outline 模块"树是按需派生、不入 IR"的不变量）。

---

## 1. 需求三件套

| 维度 | 内容 |
|---|---|
| **目标用户** | 用 docparse-rs 做 RAG / agentic ingestion 的开发者、agent 平台。痛点：现产物是 docparse 自定义的 `-f chunks` / `-f outline` JSON，每个下游都要写适配；缺一个**厂商中立、人和 agent 都能直接读、可 git diff、可溯源**的交付格式。 |
| **使用场景** | 解析后的**导出/交付**这一步触发。前置：文档已能解析出结构树（clean 数字 PDF / DOCX / HTML 命中率高）。典型：`docparse report.pdf -f okf -o report-bundle/` → 把 bundle `git add` 进知识库仓库，任意 OKF-aware agent 直接 mount。低频操作、高复用价值（导一次，长期被多 agent 读）。**替代**："自己把 chunks 灌向量库 + 手维护元数据 wiki"的临时管线。 |
| **产品定位关联** | roadmap module 4「数字原生文档结构树——要赢」的产物出口；refer §6.2「把 outline/get_section 暴露成 agent 检索面」的**离线持久化对应物**（MCP 是在线导航，OKF 是离线交付，二者同一棵树两个出口）。OKF 的"确定性 + 可溯源 bbox + 零依赖单二进制"恰好踩中现有 OSS（llmsherpa/Docling 都重依赖、起服务）的空位。 |

---

## 2. 范围

### 做什么（producer / 导出）

1. **任意已支持格式 → OKF bundle**：PDF / DOCX / HTML / XLSX / PPTX / MD / … 任何 `impl DocumentParser` 的格式，解析出 `Document` 后统一走同一 emitter（在 `core`，格式无关）。
2. **CLI**：`-f okf` 写**目录**（`-o` 给则用之，否则自动派生 `<stem>-okf/`，见 §5.1）；批量 `--out-dir` 下每文件一个子 bundle。
3. **bundle 内容**：concept `.md`（一 Section 一文件，frontmatter + markdown body）、目录嵌套镜像树、`index.md`（每目录清单 + 根带 `okf_version`）。
4. **确定性 + 可溯源**：同文档同源文件 → **字节一致**的 bundle；每 concept 的 `resource` 带 page+bbox 可定位回原文。
5. **OKF v0.1 conformance**：满足 SPEC §9（每非保留 `.md` 有可解析 frontmatter、有非空 `type`；保留文件名结构正确）。

### 不做什么（明确边界）

| 不做 | 为什么 |
|---|---|
| **OKF import / consumer**（读 OKF bundle 回 IR） | docparse 是结构**生产者**，不是知识库消费端。列为 Phase 3 可选，非本计划承诺。 |
| **enrichment agent**（Google `src/enrichment_agent` 那套 LLM 富化 description/关系） | 需 LLM，属信封外增强，违确定性核心。我们只产**确定**骨架，富化交给下游。 |
| **viz.html 可视化** | 锦上添花，非交付核心。 |
| **跨文档合并成单一大 bundle** | Phase 1 每文档一 bundle，语义清晰；多文档合并是知识库编排，另议。 |
| **语义聚合树（RAPTOR）做 concept** | refer §6.3 信封外，正交，不进主流程。 |
| **log.md 变更历史** | 一次性解析无真实历史，不臆造（守 §不静默兜底）。Phase 2 再评估是否值得生成单条 Creation。 |

---

## 3. 数据模型：Section 树 ↔ OKF 映射

### 3.1 现状（已有，不动）

- `outline::Section { id, title, level, page, bbox, children }`，`build(doc)` 派生，**heading-only**。
- `chunk::Chunk { id, kind, text, page, bbox, heading_path, section_id, char_len }`，每 chunk 已带 `section_id` 指回树（cross-module test 保证 id 方案一致）。

### 3.2 映射规则

| OKF 概念 | docparse 来源 | 规则 |
|---|---|---|
| 一个 concept `.md` | 一个 `Section` 节点 | 文件名 `NN-<slug(title)>.md`，`NN` = `section_id`（保证唯一 + 稳定排序，且天然容纳非 ASCII / 重名标题） |
| 目录嵌套 | `Section.children` | 有子节的 Section → 同名目录，自身落该目录的 `index.md`？→ **决策见 §5.1**（推荐：每 Section 一文件，子节放同名子目录，父节文件与子目录平级） |
| frontmatter `type` | 节点角色 | 根 = `Document`；标题节点 = `Section`；（Phase 2 可选）升格的表 = `Table`、图 = `Figure` |
| `title` | `Section.title` | 直接取；根用源文件名 |
| `description` | 该节正文首句（截断） | 可选；取不到则省略（spec 为 recommended 非必填） |
| `resource` | `page` + `bbox` | citable URI：`<base><source-basename>#page=<n>&bbox=<x0>,<y0>,<x1>,<y1>`，`base` 默认空（用 basename，保跨机确定性），`--okf-resource-base <uri>` 可前缀 |
| `timestamp` | **源文件 mtime**（ISO 8601 UTC） | **不用 wall clock**（见 §5.2 不变量）；取不到 mtime 则省略 |
| 扩展键 `page`/`bbox`/`level`/`section_id` | 节点字段 | spec 允许任意扩展键、消费方须容忍 → 直接塞，方便 docparse-aware 工具回查 |
| markdown body | `chunk_document(doc)` 中 `section_id == 节点 id` 的 chunks | **只挂直属内容**（子节内容在子文件，不重复）；表用 GitHub pipe（复用 `ChunkOptions.table_markdown`）；图引用 + bbox |
| 子节交叉链接 | `Section.children` | body 末尾 / index.md 用 bundle 根绝对路径 `[子节标题](/NN-.../MM-....md)`（SPEC 推荐绝对链接） |
| `index.md` | 每目录 | 列子 concept（链接 + 各自 description）；**根 index.md** 额外带 `okf_version: "0.1"` + 文档级元数据（source / page 数 / section 数） |

### 3.3 产物示例

`docparse 1901.03003.pdf -f okf -o paper/` →

```
paper/
├── index.md                      # okf_version: "0.1" + 文档元数据 + 顶层 section 清单
├── 01-introduction.md            # type: Section, body = §1 直属正文
├── 02-related-work.md
├── 03-methods.md                 # 有子节 → 同时存在 03-methods/ 目录
├── 03-methods/
│   ├── index.md
│   ├── 04-model.md
│   └── 05-training.md
└── 06-conclusion.md
```

无标题文档 → `index.md` + 单个把全文挂 root 的 concept（优雅退化，不报错）。

---

## 4. API / 落点

| 改动 | 落点 |
|---|---|
| **新 emitter 模块** | `crates/docparse-core/src/okf.rs`：`pub struct Bundle { files: Vec<(PathBuf, String)> }`；`pub fn build(doc: &Document, opts: &OkfOptions) -> Bundle`（join outline + chunk）；`Bundle::write_to(dir)`、（Phase 2）`Bundle::to_tar() -> Vec<u8>`。纯函数、确定性、`core` 不碰 PDF 库。 |
| **CLI Format** | [main.rs:705](../../crates/docparse-cli/src/main.rs#L705) `enum Format` 加 `Okf`；[main.rs:1043](../../crates/docparse-cli/src/main.rs#L1043) 分发：`Okf` 写**目录**——`-o` 给了用之，否则**自动派生** `<源文件 stem>-okf/`（见 §5.1），并向 stderr 打印落点。`OkfOptions`（resource-base、table 格式）从 `Cli` 取。 |
| **批量** | 批量路径（`--out-dir`）下 `-f okf` → 每文件落 `<out-dir>/<原名>/`（一文件一子 bundle）。复用现有批量编排。 |
| **mtime 注入** | CLI 在调用 emitter 前读源文件 mtime 传入 `OkfOptions`（`core` 不做 IO；MCP/REST 同样从 path 取）。 |
| **MCP / REST**（Phase 2） | MCP tool `export_okf`（返回文件清单或 tar base64）、REST `format=okf`（返回确定性 tar）。共用 `okf::build`。当前 [mcp.rs](../../crates/docparse-cli/src/mcp.rs) 已有 `outline` tool，加一个同构。 |

---

## 5. 关键设计决策

> 决策 2/3/4 已定（用户 2026-06-19 确认按推荐）；决策 1 经调研后定为"自动派生目录"（下）。

### 5.1 输出机制：自动派生目录（已定，调研结论）
**背景**：OKF bundle 是**目录**不是 stream，与 docparse 其它 `-f` 走 stdout 不同。**调研结论**：OKF v0.1 **没有**单文件容器约定——SPEC 不定义 `.okf`/manifest/tarball 格式；官方参考实现就是写目录（`enrichment_agent enrich --out ./bundles/<name>`），blog 说的 "ships as a tarball" 只是"自己 `tar` 那个目录",非格式约定。所以友好化的方向不是发明单文件格式,而是**消除"必须给 `-o`"的摩擦**。

**决策**：`-f okf` 写目录,落点规则——
- `-o <dir>` 给了 → 用之。
- **省略 `-o` → 自动派生 `<源文件 stem>-okf/`** 写进当前目录(如 `report.pdf` → `report-okf/`),并向 **stderr** 打印实际落点。对齐 `git clone` / `cargo new` / `unzip` 的"从输入名派生目录"惯例,也对齐 docparse 批量的 `<原名>.<后缀>` 命名精神。**不报错、不需记 flag**。
- 目标目录已存在且非空 → 报错(防误覆盖),除非显式 `-o` 指向它或加 `--force`。
- 批量 `--out-dir <d>` 下 → 每文件落 `<d>/<stem>-okf/`(一文件一子 bundle)。
- (Phase 2)管道场景:`--okf-tar` → 确定性 tar 到 stdout,供 `| tar x` / 上传。

> 比"强制 `-o` 否则报错"更友好:零必填 flag、可发现(stderr 提示)、可覆盖。单文件需求交给 Phase 2 的 `--okf-tar`,不污染默认路径。

### 5.2 父节点 文件 vs 目录的关系 —— 取 A（已定，按推荐）
- **A**：有子节的 Section 既出 `NN-slug.md`（自身正文）又出 `NN-slug/` 目录（放子节 + 子 `index.md`），二者**平级同名**。直观、子树自包含。
- B：父节正文直接写进 `NN-slug/index.md`（但 SPEC 说 index.md 通常无 frontmatter、是清单）→ 与 spec 语义冲突，不取。

### 5.3 确定性 timestamp（已定，硬不变量）
OKF `timestamp` 用 **源文件 mtime**，**绝不用 wall clock**。理由：roadmap §1 身份约束「同文档同输出、可复现」。wall clock 会让两次导出字节不同，破坏 `git diff` 价值与 §6 验收 #3。取不到 mtime → 省略该字段（spec 允许）。⚠️ 工作流/脚本里禁止用 `Date::now`。

### 5.4 resource URI 可移植性（已定，按推荐）
默认 `resource` 用**源文件 basename**（非绝对路径）+ `#page=&bbox=`，保证**跨机字节一致**。需要可点击定位的用户用 `--okf-resource-base file:///abs/dir/` 显式前缀。URI scheme 是 docparse 约定（非 OKF 标准化），在模块 doc + 用户文档写明。

### 5.5 表/图：inline vs 升格 concept（Phase 分界）
Phase 1 **inline** 进所属 section body（表 = GitHub pipe，图 = 引用 + bbox）。Phase 2 可选把大表/图**升格为独立 `type: Table`/`Figure` concept 文件**，便于 agent 直接 link 一张表——但增加文件数，按需求再做。

---

## 6. 验收标准（每条 ≥1 TC，见 testcases）

1. **结构忠实**：clean 论文/报告 → bundle 目录嵌套**镜像** outline 树；concept 文件数 = `section_count()`（+ 根）。
2. **conformance**：每非保留 `.md` 的 frontmatter 可被 YAML 解析、`type` 非空（OKF §9）；根 `index.md` 含 `okf_version: "0.1"`。
3. **确定性**：同源文件（同 mtime）两次导出 → **逐字节一致**（`diff -r` 空）；不含任何 wall-clock 时间。
4. **可溯源**：每 concept `resource` 含 `page` 与 `bbox`，能定位回原文对应标题/区域。
5. **不重复**：body 只含**直属**内容；子节正文不出现在父文件（抽样校验无重复段落）。
6. **优雅退化**：无标题文档 → 合法 bundle（root + 单 concept），不 panic、不空目录。
7. **既有输出不变**：`-f json/text/markdown/chunks/outline` 对三件套（lorem/bialetti/1901.03003）**字节不变**（OKF 是纯叠加）。
8. **输出落点**：`-f okf` 省略 `-o` → 自动写 `<stem>-okf/` 并 stderr 提示；目标已存在且非空 → 报错防覆盖（除非 `-o`/`--force`），非 panic。
9. **跨样例**：三件套 + 一个 DOCX + 一个 HTML 各产出通过 #2 校验的 bundle。

## 7. 测试用例（纲要，详见 docs/testcases/okf-export.md）

- **单测（okf.rs）**：构造小 `Document`（复用 outline.rs 的 `text_el`/`doc` helper）→ `build` → 断言文件路径集合、frontmatter 含必填 type、body join 正确、嵌套目录结构、确定性（同输入两次 `build` 结果相等）。
- **conformance 自校验**：写一个最小 Rust 校验器（解析 frontmatter + 检查 §9 三条），对产物断言（无现成 Rust OKF validator）。
- **CLI 集成**：`-f okf -o tmp/` 跑通；缺 `-o` 报错;`diff -r` 两次运行。
- **跨样例回归**：§1 三件套 + DOCX/HTML 产 bundle 过 conformance。

## 8. 用户使用例子

```bash
# 单文档 → 自动派生 report-okf/（无需记 -o），stderr 提示落点
docparse report.pdf -f okf
# → wrote OKF bundle to report-okf/ (7 concepts)
git -C report-okf add . && git commit -m "knowledge: report"   # bundle 可版本化

# 显式落点
docparse report.pdf -f okf -o ./knowledge/report/

# 自定可定位 resource 前缀
docparse report.pdf -f okf --okf-resource-base file:///data/docs/

# 批量整库 → 每文件一子 bundle <stem>-okf/
docparse ./papers -r -f okf --out-dir knowledge/

# 目标已存在且非空 → 报错防覆盖
docparse report.pdf -f okf      # error: report-okf/ exists and is not empty; use -o or --force
```

## 9. Phase 划分

| Phase | 内容 | 独立价值 |
|---|---|---|
| **1（信封内核心）** | `core/okf.rs` emitter（join outline+chunk、frontmatter、index.md、确定性 mtime、目录嵌套）+ CLI `-f okf -o dir` + 批量 + 单测 + conformance 自校验 + 跨样例回归。**完整可交付**。 | ✅ 能产出合法、确定、可溯源的 bundle |
| **2（分发 + agent 接入）** ✅ 2026-06-19 | 确定性 tar 到 stdout（`Bundle::to_tar` + CLI `--okf-tar`）；MCP `export_okf` tool + REST `format=okf`（application/x-tar）。表/图升格 concept 未做（可选，按需）。 | ✅ agent 在线取 bundle / 管道化 |
| **3（可选 consumer）** | OKF bundle 作**输入格式**（新 crate `docparse-okf` `impl DocumentParser`，读 md+frontmatter 回 IR）。**非本计划承诺**。 | 闭环：OKF ↔ IR 双向 |

## 10. 风险与开放问题

1. **OKF v0.1 是 moving target**（spec 自陈"起点非成品"）：绑定风险。缓解——产物极薄（md+yaml）、迁移成本低、`okf_version` 声明隔离；只跟 v0.x 不追未发布字段。
2. **文件数膨胀**：深树 → 很多小文件。缓解——可选 `--okf-max-depth N` 把更深叶子并入父 body（复用 `Section::pruned` 思路）。Phase 1 先不做，验收 #1 观察真实文档深度再定。
3. **slug 冲突 / 非 ASCII**：`NN-` 前缀（=section_id）已保证唯一与稳定排序；slug 仅为可读性，冲突无害。
4. **resource 跨机一致**：默认 basename（§5.3）规避;绝对路径仅 opt-in。
5. **description 取值**：截断正文首句可能不达意。Phase 1 取不到就省略（不臆造），不调 LLM。
6. **空/坏页**：解析失败页返回空 Page（既有不变量）→ 对应 section body 为空 concept，合法不 panic。

## 11. 实施前 checklist

- [ ] 本 plan review 通过（方向 / §5 决策 / 不做什么）
- [ ] 拉 Google 官方 sample bundle（GA4 / StackOverflow / Bitcoin）对照真实 frontmatter 用法，校准 §3.2 字段
- [ ] 派生 `docs/testcases/okf-export.md`（验收 §6 每条 →TC）
- [ ] 实施 Phase 1（源码 + 测试同 PR）→ 跑单测 + 跨样例 → testresults → 回写本 plan 完成情况 → 模块 doc → review → commit/push
