# devlog 2026-07-27 — RegionReader 接缝 + VLM 区域识别后端（OvisOCR2 接入）

> 上游：[调研](../plans/2026-07-27-OvisOCR2调研与适配评估.md)、[设计 spec](../plans/2026-07-27-OvisOCR2接入需求分析与功能设计.md)（门控与验收标准在 §7）。
> 涉及两侧：`vendor/docparse`（接缝 + 后端 + 四张脸）与 `crates/fastsearch-cli`（`parse-vlm` 摄取档）。
> commit：`774bd0f`（主体）、`72488df`（四张脸 + docparse 文档收口）、本次（服务面共享池）。

## 做什么 / 为什么

[OvisOCR2](https://huggingface.co/ATH-MaaS/OvisOCR2)（0.8B 端到端页面解析，Apache-2.0）打中 docparse 记分牌上
**两个已判定在案的短板**：① 学术难表 e2e 0.52 —— status.md Phase 6 明确判为「UniRec-0.1B 固定 960×1408
输入的真实天花板」，真杠杆只剩「更大/多分辨率表模型」；② 整页转写（G8d）原本写着「Qwen2.5-VL 7B 起步、
32B 才到生产线」，而 OvisOCR2 只有 1.71GB 权重 —— 这条从「太贵所以只能规划」变成可落地。

**没有做的是「整页端到端」**。它正文不带坐标（只给图区归一化 bbox），而 `core::Chunk.bbox` 是非 `Option`
必填、`resolve_citation → {page,bbox}` 深链高亮是本项目差异化的根。整页模式只能编造坐标，且 docparse
`Chunk` 连 `source` 字段都没有（IR 上的 `TextChunk.source` 在 `chunk_document` 时就丢了），**连"这条没坐标"
都诚实表达不了**。所以走**区域级**：版面/表格检测继续提供几何，模型只负责「读」。

## 关键决策一：唯一要抽象的是「读」那一步

读代码时发现 `refine_tables` 与 `transcribe_pages` 做的是同一件事：

```
crop_region(...) → model.recognize(crop) -> String → looks_degenerate 守卫 → 解释 + source 标注 + 失败保底
```

差别只在最后一步的「解释」（HTML→`Table` / 文本→`TextChunk`）。所以接缝只需要一行签名：

```rust
// docparse-core/src/region_reader.rs（零新依赖）
pub trait RegionReader: Sync {
    fn read(&self, img: RegionImage<'_>, max_tokens: usize) -> Result<String>;
    fn read_batch(&self, imgs: &[RegionImage<'_>], max_tokens: usize) -> Vec<Result<String>>;
    fn source_tag(&self) -> String;
}
```

trait 放 `core` 而非 `ocr`：否则轻量的 `docparse-vlm` 要反依赖重 tract 的 `docparse-ocr`。现在依赖图单向
（`core ← ocr`、`core ← vlm`），两个后端互不相识，装配交给 CLI/服务面。

## 关键决策二：并发归后端，不归编排

同一个调用点，两个后端的正确答案相反 —— `UniRec` 是进程内 tract 推理、已经跑满核，并发只会自己抢自己；
`VlmRegionReader` 是网络等待，不并发就浪费掉「服务化模型」一半的价值（vLLM 的批处理）。

所以 `read_batch` 带**默认串行实现**，HTTP 后端覆写为有界并发（专用 rayon 池，`MAX_INFLIGHT=4`）。
两个编排点写一样的代码，各后端自负其责。

## 顺带修掉的既有缺陷

- **`VlmClient` 请求体此前根本不发 `max_tokens` / `temperature`**（只有 `{model, messages}`）。识别档必须
  封顶：文档模型在难输入上会退化成复读，`looks_degenerate` 只能**事后**拒收、救不回已经烧掉的时延。
  现固定 `temperature: 0`（识别任务采样无收益，且抬高复读概率），`max_tokens` 由调用方下发。
- **`MAX_IMAGE_SIDE = 1024` 硬编码**（为「图区裁剪描述」定的）。提为 `VlmConfig.max_image_side`，识别档
  2048 —— 1024 上限会把动态分辨率模型的优势掐掉，**正好重演 Phase 6 记录的固定分辨率天花板**，那样测出
  来的会是参数而不是模型。

## 怎么做

**docparse 侧**
- `core`：新增 `region_reader{RegionImage, RegionReader}`。
- `ocr`：`UniRec` 实现 trait（`read_batch` 用默认串行）；`refine_tables`/`transcribe_pages` 改吃
  `&dyn RegionReader`，输出逐字节不变；把重构真正触及的部分抽成纯函数 `refined_rows` /
  `build_replacement`（退化门 + chunk 组装 + `source` 标注）。
- `vlm`：新增 `region::VlmRegionReader`（`for_text` / `for_table` 两种 prompt）。**表格走 HTML 而非 TSV**：
  `parse_html_table` 已能展开 `rowspan`/`colspan`，而 TSV 路要求「合并格重复值」、拿不到拓扑 —— HTML
  信息量严格更大且复用现成解析器。旧 TSV 路保留（兼容任意通用 VLM）。
- `cli`：`--table-vlm` / `--transcribe-vlm`，与对应内嵌档 clap 互斥。

**四张脸**（第二次提交补的，见下「返工」）
- `EnhanceOpts` 加 `table_vlm` + `EnhanceOpts::validate()`：CLI 用 clap `conflicts_with` 禁止叠加表格后端，
  **MCP/REST 表达不了** —— 约束必须跟着能力一起过面，否则服务面能进到 CLI 进不去的状态。
- `EnhanceState` 用 `OnceLock` 缓存 reader。理由与缓存 UniRec 不同：**reader 持有那个有界请求池**，
  按请求建则线程按请求生灭，且 `MAX_INFLIGHT` 静默失效 —— N 个并发请求各拿一份预算，合起来照样打爆服务。

**fastsearch 侧**
- feature `parse-vlm` + `apply_vlm`：env 门控（`_URL`+`_MODEL` 必需，`FASTSEARCH_LAYOUT_MODEL` 设了才开
  转写，`_MAX_PAGES` 默认 50）。**能力按配置浮现**，与 `apply_ocr`/`apply_tables` 同形。
- 顺序 VLM 在 UniRec 之前，UniRec 跳过已被 VLM 精修的表 → 两者可同配（VLM 优先 + UniRec 兜底），
  不重复推理也不互相覆盖。

## 怎么验证

- **重构零回归**：docparse 全绿（26→33→306）；三件套跨样例回归逐字不变。
- **顺序契约有变异测试守护**：`transcribe_pages` 把 `read_batch` 结果**按位置** zip 回区域，乱序会把文本
  静默挂到别的 bbox 上（引用全错、无报错）。stub 解 PNG IHDR 回显尺寸 + 三张不同尺寸图 + 严格按位断言；
  注入 `v.reverse()` 确认测试会失败。
- **mock 端到端**（stub OpenAI 服务 + 合成 ruled-table PDF）：`source="table:vlm:*"`、stub 的
  `rowspan=2`/`colspan=2` 被展开成 `[[Anno,Ricavi,Ricavi],[Anno,2024,2025],[Totale,1.234,5.678]]`、
  请求体含 `max_tokens=2000`/`temperature=0.0`/PNG data-URL。
- **四张脸真跑**：MCP `tools/list` 两个工具都露出 `table_vlm`、`parse_document` 调用产出正确；REST
  OpenAPI 有参数、`/parse` 跑通；两面传两个表格后端都返回同一句错误。
- **共享池**：REST 连发 6 次请求，进程线程数 19(idle)→42(首次)→**恒定 42**，不随请求增长。
- **降级**：服务不可达时 stderr 报错、保留确定性结果、退出码 0；未配 env 时 `apply_vlm` 恒等（`VLM:` 输出 0 行）。

## 返工记录（三轮 review 各揪出一批）

| 轮次 | 问题 | 教训 |
|---|---|---|
| 1 | `max_tokens` 契约在 VLM 实现里被 `_` 吃掉；门 3 的「≤5s/区域」与「≤30s/页」在 >6 区域时不可能同时成立；双精修优先级未定义 | 抄既有编排时把**串行时代的门槛**一起抄了过来 |
| 2 | 批量顺序契约无测试（stub 用计数器 + 结果 `sort()`，结构上测不出乱序） | **能跑通 ≠ 测到位**：危险的从来不是会报错的路径。e2e 走的是单表，恰好绕开唯一会静默出错的地方 |
| 3 | `--table-vlm` 只有 CLI 能用（破「四接口一份输出」）；docparse 自己的 capabilities/status/落点表/CHANGELOG 一处没动 | 把 vendor 里那半个项目当成了「不是我的地盘」—— 它是同一个仓、同一次提交，且这条就写在我自己 spec 的 §9 第 7 步里 |
| 4 | 服务面按请求建 reader，绕过 `EnhanceState` 的 server-lifetime 设计，`MAX_INFLIGHT` 在最需要它的地方失效 | 「照着相邻代码写」不够 —— 相邻的 `VlmClient` 确实是按请求建的，但它没持有并发预算 |

为防重犯，docparse `CLAUDE.md §2` 落点表加了一行「加"能力"给四张脸」，写明只加 CLI 就破了四接口一致。

## 状态（诚实记账）

**代码全部落地、mock 全绿；真实模型的四道门一道没跑**（需 vLLM + GPU）：

| 门 | 判据 | 状态 |
|---|---|---|
| 0 · 形态假设 | 区域级用页级模型是 OOD，引用的 96.58 等成绩**一条都不适用**于区域裁图 | ⏳ 未跑（~1h 手工，零代码） |
| 1 · 坐标 | 产出 chunk 的 bbox 全部来自真实检测几何 | ⏳ |
| 2 · 质量 | 相对当次重测的 UniRec 基线 +0.10 TEDS_X | ⏳ |
| 3 · 速度 | 并发下 ≤30s/页 | ⏳ |

**门 0 不过则 v1 形态不成立**，第 3 步起的代码作废、要转向「整页模式 + schema 锁步」。但第 1–2 步
（接缝 + `VlmClient` 参数修正）两条路都要，不白做。

运行验证的具体做法见 [spec §7.5 运行手册](../plans/2026-07-27-OvisOCR2接入需求分析与功能设计.md)。

---

## 第二阶段（同日晚）：并到 upstream、研究怎么真跑、把 e2e 变成可复现的

### 并到 upstream，顺带改变了 v2 的结论

push 被拒——远端多了 PR #3（外部贡献者，通用 chunk 管理协议）且**动了同一个 `ingest.rs`**。先看清对方
改了什么（只在 `from_docparse_chunk` 加两个新字段，与我不同 hunk）再 rebase，干净通过，重跑全套
（341 绿）才 push。

**意外收获**：PR #3 给 `core::Chunk` 加了 `metadata`（调用方透传，不进索引）与 `searchable`。这直接
推翻了我判「整页模式」死刑的理由之一——原话是"fastsearch `Chunk.bbox` 非 `Option` → 无法诚实表达
'这条没有页内坐标'"，而 `metadata` 就是 `source`/`coord_level` 的现成落点。阻塞从**两侧 schema 锁步**
收敛为**单侧一件事**：让 docparse 的 `Chunk` 把 `source` 活过 `chunk_document`。已回写 spec §8。

教训：判一条路"死"的时候，那个理由的**保质期**取决于别人也在动的代码。

### 研究怎么真跑四道门（spec §7.5）

spec §4 写的 `vllm serve`（CUDA）**在本机不可用**：M5 Max / 128 GB / 无 N 卡。列了四条替代路径
（[vllm-mlx](https://github.com/waybarrios/vllm-mlx) 显式支持多模态，首选但 Qwen3.5 新架构有风险；
vllm-metal 覆盖更窄；vLLM CPU 源码构建慢但够用；远程 GPU）。

**真正有用的是一个分解**：四道门的硬件要求根本不同。门 0/1/2 问的是"模型输出什么 / 我方怎么装配 /
准不准"，任何能出结果的端点都行；只有**门 3（速度）必须在部署目标上跑**——Mac CPU 档的数字没有意义。
所以不必等 GPU 才能开始，而最该先跑、决定 v1 立论是否成立的门 0，恰好完全不依赖 GPU。

门 2 也不用从零写：`scripts/eval/omnidocbench/` 已有 `e2e_table_eval.py`（TEDS_X）与
`compare_layout_backends.sh`——一个"同页、同打分器、唯一变量=后端"的 A/B 骨架，正是门 2 要的形状。
缺的只是把变量换成 `--table-model` ↔ `--table-vlm` 的兄弟脚本。这样"相对当次重测基线取增量"的口径
由**脚本结构**保证，不靠人记得住。

### 补上四处（本轮 review 的产出）

**1. mock e2e 此前不可复现** —— 这是最该修的。整个特性最强的证据（CLI→服务→IR、rowspan 还原、请求
形状）只存在于我这次会话的 scratchpad 里，跑完即失。单测覆盖了各个零件，唯独**接线**没有任何东西守着。
现补 `vendor/docparse/scripts/vlm_stub_e2e.py`：stdlib-only、无 GPU/模型/网络，自建 stub 端点 + 合成
ruled-table PDF，5 秒跑完 15 条断言（含"服务挂了要降级且不冒领 `source` 标注"）。

写它的过程本身又抓到一个认知错误：第一版用 `srv.shutdown()` 模拟"服务不可用"，结果**挂了 120 秒**——
`shutdown()` 只停 serve 循环，监听 socket 还开着，连接被接受后黑洞掉，一直等到客户端 120s 超时。
要 `server_close()` 才是端口拒绝。由此发现一条该记的已知限制：**"端口拒绝"是立即降级，"黑洞式挂起"
是每个区域各等满 120s**，两者代价差一个数量级。已写进 17-cli.md 已知限制。

**2–3. 又漏了两处 spec 回写**，且是**上一轮同型错误的重演**：上轮漏的是 docparse 的
capabilities/status，这轮漏的是 fastsearch 的 **`docs/specs/17-cli.md`**（模块权威 spec，AI_AGENT_DEV_SPEC
§0 第 5 步点名要回写）和 **`vlm-service-driven-capabilities.md`**（我自己 spec §9 第 7 步列着的）。
17-cli.md 里那句"VLM 自然图语义描述 = `parse-vlm` 下一迭代"甚至**已经变成假话**——`parse-vlm` 存在了，
且含义不是自然图描述。

**模式**：我倾向于更新"正在看的叙述性文档"（README 类、摄取指南、CLAUDE.md），而漏掉"按模块/按主题的
权威文档"（specs/、refer/）。前者是我写代码时会打开的，后者不是。两轮各栽一次，说明这不是偶然。

**4. 无遗留 TODO/FIXME**（`git diff 74621cf..HEAD` 全量扫过），代码面干净。

### 仍然开着的

| 项 | 状态 |
|---|---|
| 四道门 | **一道没跑**，需 vLLM 端点；门 0 最优先且不需 GPU |
| `compare_table_backends.sh`（门 2 的 A/B） | 已在 §7.5 定形，未写 |
| 2e 图内嵌表 recall / 2c 图表→表格 / 2b 页型判官 | 未做，管线可复用 |
| `--transcribe-vlm` 上服务面 | CLI-only（与 `--transcribe-model` 一致，非破例） |
| 整页模式 v2 | 阻塞已缩小为 docparse 单侧加 `source` 字段 |
