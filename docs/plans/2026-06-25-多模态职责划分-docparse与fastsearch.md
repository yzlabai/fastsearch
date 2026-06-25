# 多模态职责划分：docparse ↔ fastsearch（整体架构）

> 状态：草案 v1（架构分析）｜日期：2026-06-25｜上游：[多模态需求分析](2026-06-25-多模态数据支持-需求分析.md)、[多模态功能设计与开发计划](2026-06-25-多模态功能设计与开发计划.md)。
>
> **本文目标**：从 localkb 整体架构出发，回答"多模态预处理的哪些功能应该放到 **docparse-rs**（解析上游），哪些留在 **fastsearch**（检索下游）"，并指出两者必须**锁步协调**的契约。
>
> 范围：`/Users/ale/works2026/localai/localkb/docparse-rs`（解析）与本仓 `fastsearch`（检索）。结论以两边代码为真源。

---

## 0. 关键发现（重要，改变设计前提）

1. **两个项目已经共享同一套 `Chunk` schema**。docparse 的 [`docparse-core/src/chunk.rs`](../../../docparse-rs/crates/docparse-core/src/chunk.rs) 输出的 `Chunk{id,kind,text,page,bbox,heading_path,section_id,char_len, image:Option<ImageMeta>}` 与本仓 `core::Chunk` **字段几乎逐一对应**。localkb 的架构本来就是"**docparse 解析→chunks（上游）｜fastsearch 索引→检索（下游）**"，多模态不新增这条边界，只是沿它扩展。
2. **场景 A 的"图渲染依赖"其实已被 docparse 满足**（修正功能设计 §10 的 🔴）。docparse 已能导出图字节——`--image-embed`(base64 inline) / `--image-dir`(文件)——并做**4 级 caption 绑定**（`vlm:<model>` > `layout-caption` > `caption-line` > `alt`）。→ **fastsearch 绝不该自己装 PDF 渲染器**，直接消费 docparse 导出的字节/caption 即可。
3. **docparse 已把 SRT/VTT 解析成"带时间码的文本"**。即"**已有字幕/转录**的媒体"docparse 已部分覆盖（场景 C/D 的一部分）——但当前 `Chunk` schema **没有时间字段**，时间码落在哪需确认（见 §3 锁步项）。
4. **docparse 有成熟的"可插拔增强器"架构**：确定性零模型核心 + opt-in 神经增强（OCR/layout/表格用进程内 tract ONNX；VLM 走 OpenAI 兼容 HTTP）。**ASR、视频抽帧完全契合这个模式**（像 VLM 一样挂 HTTP 模型），属上游天然延伸。
5. docparse **刻意不做**：ASR、视频、写库、嵌入、下游集成（输出 JSON/文件/HTTP，无状态、确定性、可按内容哈希缓存）。

**一句话**：docparse 是"**把任意来源模态规整成可溯源 chunk**"的统一归一化器；fastsearch 是"**索引 / 嵌入 / 检索 / ACL / 服务 / 媒资托管**"。多模态预处理（提取、转录、抽帧、描述、时间码、坐标）**几乎都属 docparse**；fastsearch 只做索引侧与服务侧。

---

## 1. 职责划分总表

| 能力 | 归属 | 现状 | 说明 |
|---|---|---|---|
| 文档/图片解析、阅读序、大纲、分块 | **docparse** | ✅ 已有 | 12+ 格式 → 统一 IR/chunk |
| **图提取**（PDF XObject/DOCX/PPTX/HTML） | **docparse** | ✅ 已有 | `kind=image` 一等 chunk（≥1% 页面积） |
| **图字节导出**（base64 inline / 文件） | **docparse** | ✅ 已有 | `--image-embed`/`--image-dir` → 喂 fastsearch 的 `AssetPointer::{Inline,Object}` |
| **图 caption / 描述**（VLM/layout/文本/alt 4 级） | **docparse** | ✅ 已有 | caption 即 chunk 的 `text` 表示，供 BM25/文本向量召回 |
| **OCR / 版式 / 表格** | **docparse** | ✅ 已有（opt-in） | 进程内 ONNX；fastsearch 不碰 |
| **SRT/VTT 字幕 → 带时间码文本** | **docparse** | ✅ 已有（时间字段待补） | "已有字幕的媒体"上游即可出 chunk |
| **ASR**（音频 → 带时间戳转录 chunk） | **docparse**（新增增强器） | ❌ 待建 | 像 VLM 一样挂 Whisper/HTTP；产时间码文本 chunk |
| **视频场景分割 + 关键帧抽取**（+ASR） | **docparse**（新增，重） | ❌ 待建 | 出"关键帧图 chunk + 转录 chunk"，带时间区间 |
| **统一 chunk schema 扩展**（时间区间/asset/audio·video kind） | **两边锁步** | ❌ 待建 | §3，唯一硬协调点 |
| —— 以下留在 fastsearch —— | | | |
| **嵌入**（文本/图/跨模态/多向量） | **fastsearch** | 部分（文本经 HTTP） | 必须与**查询期 embedder 同空间**、换模型要重建索引——属检索侧，不能上游做 |
| **媒资字节托管 / 对象存储 / 短签 URL** | **fastsearch** | ❌ 待建 | docparse 无状态；持久/托管是检索侧 |
| **ACL 媒资网关 `/v1/asset`** | **fastsearch** | ❌ 待建 | ACL 来自认证身份、服务端注入，docparse 不涉权限 |
| **向量索引 / ANN / MaxSim / 融合 / rerank** | **fastsearch** | 部分 | 检索引擎核心 |
| **PG 真源写入 + CDC + 模态过滤检索** | **fastsearch** | 部分 | docparse 不写库 |

---

## 2. 建议：推到（或确认留在）docparse 的功能

### 2.1 已在 docparse，fastsearch 直接消费（不要重造）
- **图字节 + caption + bbox**：fastsearch 的 `MediaRef.asset` 用 docparse 的 `data_base64`(→`Inline`)/`file`(→`Object`)，`text` 用 docparse 的 caption+上下文。**fastsearch 不做裁图、不做 caption、不装渲染器**（修正功能设计 §10 渲染依赖：依赖 docparse 导出，不自建）。
- **已有字幕媒体（SRT/VTT）**：docparse 已出文本，fastsearch 只索引。

### 2.2 新增到 docparse（契合其增强器架构）
- **ASR 增强器**（音频）：新 input 类型/enhancer，挂外部 Whisper（HTTP，复刻现有 VLM 的 HTTP 模式），产出 `kind=audio` + `text=转录` + 时间区间的 chunk。**理由**：与现有"born-digital 核心 + 可插拔 HTTP 模型"边界一致；产物 schema 兼容；fastsearch 拿到的就是普通可溯源 chunk。
- **视频处理**（重）：场景分割(TransNetV2 类) + 关键帧 + 轨道 ASR，产出"关键帧图 chunk（带时间区间）+ 转录 chunk"。**理由同上**；视频解复用是重依赖，建议独立 `docparse-media` crate、opt-in，**不污染 docparse 确定性核心**（与其 OCR/VLM 隔离策略一致）。

> 取舍：ASR/视频会给 docparse 引入音视频依赖。docparse 的原则是"外接不内化"——这两者按 HTTP 外接（ASR/VLM 类）或独立 opt-in crate（视频解复用），**主管线仍零模型确定性**。这与 docparse 现有边界自洽。

### 2.3 明确**不要**推给 docparse（留 fastsearch）
- **检索用嵌入**：docparse 的 VLM 只为 caption；**检索向量必须与查询期 embedder 同空间、且换模型要触发 fastsearch 侧索引重建**——这是检索侧关注点，放上游会割裂空间一致性。
- **媒资托管 / 对象存储 / 短签 URL / ACL 网关**：docparse 无状态、不涉权限；持久化与 ACL 是 fastsearch/服务侧。
- **写 PG / CDC**：docparse 输出 transient JSON；落 PG 真源是 fastsearch 摄取侧（现有 `fastsearch index` 吃 chunks.json 的那条路）。

---

## 3. 唯一硬协调点：锁步扩展共享 chunk schema

两边都要、且必须**同字段名/同语义**扩展（否则 docparse 出的 chunk 喂不进 fastsearch）：

| 字段 | docparse `chunk.rs` | fastsearch `core::model` | 备注 |
|---|---|---|---|
| 时间区间 | 新增 `time: Option<TimeSpan{start_ms,end_ms}>` | 同（功能设计 §2.2/§2.3） | SRT/VTT 时间码也迁到此字段（现状散在 text） |
| 模态 | `kind` 加 `Audio`/`Video` | 同（功能设计 §2.1） | 派生 `modality` 过滤字段 |
| 媒资指针 | 复用现有 `ImageMeta{file,data_base64,media_type}` 泛化为 `media` | `MediaRef{asset:AssetPointer,...}` | docparse 的 `data_base64→Inline`、`file→Object` |
| caption 来源 | 已有 `caption`/`caption_source` | 已有，对齐 | fastsearch 当前 `ImageMeta` 缺 `data_base64`，需补齐对齐 |

**行动**：在动 fastsearch MM1 之前，先与 docparse 团队/仓库敲定这张表（一次 schema 评审，两仓同 PR 周期落地）。这是多模态唯一的跨仓阻塞项。

---

## 4. 整体数据流（目标态）

```
来源（PDF/图/音/视频/字幕）
        │
        ▼  docparse-rs（解析 + 提取 + 转录/抽帧 + caption + 坐标/时间码）
   统一 Chunk{kind,text,page,bbox,time?,media?,heading_path,section_id,...}
   （+ 导出图字节：base64 / 文件）
        │  JSON / 文件 / HTTP（无状态、确定性、可缓存）
        ▼  fastsearch 摄取（fastsearch index / 写 PG 真源 + 媒资分层落字节）
   Postgres 真源（chunk 行 + 小图 bytea + 向量；大媒资→对象存储 URI）
        │  逻辑复制 CDC
        ▼  fastsearch 引擎（按模态选 embedder → 派生 BM25/向量/MaxSim 索引）
   检索：ACL 注入 → keyword∥跨模态向量 → 融合 → rerank → top-K
        │
        ▼  /v1/search（含以图搜图）+ /v1/asset（ACL 媒资网关 + 深链/Range）
   答案层（外部 LLM）：内联展示图 / 音视频深链回放
```

边界清晰：**docparse 产"内容与坐标/时间"，fastsearch 产"索引、向量、权限、服务"**。两者只在 chunk schema 这一处契约耦合。

---

## 5. 对功能设计文档的回写

- **修正 [功能设计 §10 渲染依赖 🔴](2026-06-25-多模态功能设计与开发计划.md)**：场景 A 的图字节**由 docparse 导出**（`--image-embed`/`--image-dir`），fastsearch 不自建 PDF 渲染器；`DocRegion` 仅作"无字节时跳转原文页"的兜底。该风险从"可能需引入渲染依赖"降级为"确认 docparse 导出开关已开"。
- **D3 决策强化**：转录/抽帧/caption **明确归 docparse**（ASR/视频为其新增 opt-in 增强器），fastsearch 只消费——已与功能设计 D3 一致，本文给出代码级依据。
- **新增跨仓阻塞项**：§3 的 schema 锁步评审，应作为 MM1 的前置（功能设计 §8 执行序里 MM1 之前插入"docparse schema 对齐"）。

---

## 6. 小结

- 多模态预处理**绝大部分天然属 docparse**，且其中**图提取/字节导出/caption/字幕时间码已经现成**——fastsearch 侧能省掉裁图、caption、渲染器一大块。
- 真正要新建的上游能力只有 **ASR** 与 **视频抽帧**，且都契合 docparse 现有"可插拔 HTTP/ONNX 增强器"架构，按"外接不内化"挂上去即可。
- fastsearch 专注**索引/嵌入/检索/ACL/媒资托管/服务**，不越界做解析与转录。
- 两仓唯一硬耦合 = **共享 chunk schema 的锁步扩展**（§3）；这一处对齐好，整条多模态链路的职责边界就干净了。
