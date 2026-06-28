# 多模态职责划分：docparse ↔ fastsearch（整体架构）

> 状态：架构分析 v1.1（**fastsearch 侧 M0 已落地**，2026-06-27 回写现状）｜日期：2026-06-25｜上游：[多模态需求分析](2026-06-25-多模态数据支持-需求分析.md)、[多模态功能设计与开发计划](2026-06-25-多模态功能设计与开发计划.md)、[多模态完善 devlog](../devlog/2026-06-27-多模态完善.md)。
> **现状**：fastsearch 侧的 schema 扩展 / 模态·时间过滤 / 媒资网关 / inline 字节均已落地并 Docker 验证（§1、§3）；剩余跨仓阻塞 = **docparse 侧补 `time` 字段 + `Audio/Video` kind**。
> **⭐ 更新（2026-06-27）：docparse 已 subtree 并入本仓 `vendor/docparse`（融合 Option B）**——"跨仓手工锁步"痛点**已结构性消解**：两仓同处一个 git 仓，schema 对齐由摄取适配器 [`from_docparse_chunk`](../../crates/fastsearch-cli/src/ingest.rs)（编译即报错的焊点）保证；`fastsearch ingest` 进程内解析 9 格式 + 扫描件 OCR。详见 [融合 devlog](../devlog/2026-06-27-docparse融合与多格式OCR摄取.md)。§3 的"跨仓评审"现退化为"单仓内改 `chunk.rs` + 适配器编译验证"。
>
> **本文目标**：从 localkb 整体架构出发，回答"多模态预处理的哪些功能应该放到 **docparse-rs**（解析上游），哪些留在 **fastsearch**（检索下游）"，并指出两者必须**锁步协调**的契约。
>
> 范围：解析(`vendor/docparse`，**已 subtree 并入本仓**) + 检索(`crates/*`)——**现为单一 git 仓**（融合 Option B，见 §0 更新）。结论以代码为真源。

---

## 0. 关键发现（重要，改变设计前提）

1. **两个项目已经共享同一套 `Chunk` schema**。docparse 的 [`docparse-core/src/chunk.rs`](../../../docparse-rs/crates/docparse-core/src/chunk.rs) 输出的 `Chunk{id,kind,text,page,bbox,heading_path,section_id,char_len, image:Option<ImageMeta>}` 与本仓 `core::Chunk` **字段几乎逐一对应**。localkb 的架构本来就是"**docparse 解析→chunks（上游）｜fastsearch 索引→检索（下游）**"，多模态不新增这条边界，只是沿它扩展。
2. **场景 A 的"图渲染依赖"其实已被 docparse 满足**（修正功能设计 §10 的 🔴）。docparse 已能导出图字节——`--image-embed`(base64 inline) / `--image-dir`(文件)——并做**4 级 caption 绑定**（`vlm:<model>` > `layout-caption` > `caption-line` > `alt`）。→ **fastsearch 绝不该自己装 PDF 渲染器**，直接消费 docparse 导出的字节/caption 即可。
3. **docparse 已把 SRT/VTT 解析成"带时间码的文本"**。即"**已有字幕/转录**的媒体"docparse 已部分覆盖（场景 C/D 的一部分）。**fastsearch 侧时间字段已落地**（`core` 的 `TimeSpan` 挂在 `MediaRef.time` / `Citation.time`，PG `time_start_ms/time_end_ms` 列 + 区间下推，MM1/MM2c-time）；**docparse 侧的时间码字段仍待对齐**到共享 schema（见 §3 锁步项）。
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
| **统一 chunk schema 扩展**（时间区间/asset/audio·video kind） | **两边锁步** | 🟡 fastsearch 侧 ✅ / docparse 待对齐 | fastsearch `core` 已加 `Audio/Video` kind + `TimeSpan` + `MediaRef/AssetPointer`（MM1）；**docparse 侧 `time` 字段仍待对齐**（§3 唯一硬协调点） |
| —— 以下留在 fastsearch —— | | | |
| **嵌入**（文本/图/跨模态/多向量） | **fastsearch** | 🟡 部分（文本经 HTTP） | 文本嵌入经 HTTP（Ollama/OpenAI 兼容）✅；**跨模态单向量（M1）/ ColPali 多向量（M2）gated** 在多模态模型服务。必须与查询期 embedder 同空间、换模型重建索引 |
| **媒资字节托管 / 对象存储 / 短签 URL** | **fastsearch** | 🟡 inline ✅ / 对象存储待建 | **inline 小图字节托管 ✅**（PG `media_bytes` 真源 + `fetch_media_bytes` 按需直查，MM2c-bytes）；对象存储 / 短签 URL **gated**（需对象存储） |
| **ACL 媒资网关 `/v1/asset`** | **fastsearch** | ✅ 已建（inline 通） | `resolve_citation` + `GET /v1/asset/{cid}`，ACL 服务端注入不可绕过（越权 404、不暴露存在性）；**inline 字节 ✅**（MM6-inline）、DocRender ✅；SignedUrl/Range 待对象存储 |
| **向量索引 / ANN / MaxSim / 融合 / rerank** | **fastsearch** | 🟡 主体 ✅ | BM25 + 向量（暴力 / HNSW+量化 / pgvector 直查）+ RRF/归一化融合 + LTR rerank ✅；**MaxSim（ColPali 多向量）gated** |
| **PG 真源写入 + CDC + 模态过滤检索** | **fastsearch** | ✅ 主体 | PG 真源写入 + 逻辑复制 CDC（幂等/LSN 续传）+ **模态过滤（两端 SUPERSET）+ 时间区间精确下推** ✅（MM2c-time/MM4/MM5） |

> **现状以代码为真源**，状态截至 2026-06-27（见 [多模态完善 devlog](../devlog/2026-06-27-多模态完善.md) 完成情况表）。fastsearch 侧的 M0 多模态（schema/过滤/媒资网关/inline 字节）已落地并 Docker 验证；剩余 gated 项（对象存储、跨模态/ColPali 模型）已诚实标注。
>
> **图像检索现状速览（caption-as-text，无 CLIP）**：图能被检到靠的是 docparse 上游给的 **caption/描述文本**（4 级绑定）落进 chunk 的 `text`，走 **BM25 倒排 + 文本向量 ANN** 召回；图字节本身只用于展示（inline `media_bytes` / `/v1/asset` 网关），**不参与打分**。引擎侧 `Embedder` trait 仍只吃 `&[String]`（`EmbedKind=Query|Passage`），**无视觉/跨模态嵌入、未集成 CLIP/SigLIP/ColPali**。真正的视觉向量（以图搜图、page-image 多向量）= **M1（跨模态单向量，MM8/9/10）/ M2（ColPali MaxSim，MM11+）**，均 `gated` 待多模态 HTTP 模型服务（接入落点见 [功能设计 §4.1/§5.3](2026-06-25-多模态功能设计与开发计划.md)）。

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

| 字段 | docparse `chunk.rs` | fastsearch `core::model`（**已实现 MM1**） | 备注 |
|---|---|---|---|
| 时间区间 | ⏳ **待加** `time: Option<TimeSpan{start_ms,end_ms}>` | ✅ `TimeSpan` 挂 `MediaRef.time` / `Citation.time` | SRT/VTT 时间码待从 text 迁到此字段（**docparse 侧待对齐**） |
| 模态 | ⏳ `kind` **待加** `Audio`/`Video` | ✅ `ChunkKind::{Audio,Video}` + `modality()` 派生 | fastsearch 过滤已用（两端 SUPERSET 下推） |
| 媒资指针 | ✅ 现有 `ImageMeta{file,data_base64,media_type}` | ✅ `MediaRef{asset:AssetPointer,...}`；CLI `map_image` 适配 | docparse `data_base64→Inline`、`file→Object`、皆无→`DocRegion` |
| inline 字节 | ✅ `data_base64`（base64） | ✅ `Chunk.media_bytes`（PG `media_bytes` 真源，MM2c-bytes） | 字节经 `PgStore::upsert_doc` 落 PG（库/外部写入路径）；**`/v1/index`（含 CLI 客户端）写引擎派生索引、非 PG 真源**，故 inline 字节由 `upsert_doc` 类真源写入者落、不经此路 |
| caption 来源 | ✅ `caption`/`caption_source` | ✅ 对齐（caption 进 `text`、`caption_source` 进 `MediaRef`） | — |

**现状（2026-06-27）**：**fastsearch 侧 schema 扩展已 MM1 落地**（非 MM1 前置阻塞——该假设已过时）。剩余唯一硬协调点 = **docparse 侧 `chunk.rs` 补 `time` 字段 + `Audio/Video` kind**，使其输出的 chunk 能直接喂 fastsearch（一次 docparse 侧 schema 评审，两仓字段名/语义对齐）。

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
- **跨仓阻塞项（现状更新）**：§3 的 schema 锁步**fastsearch 侧已先行落地**（MM1：`Audio/Video` kind、`TimeSpan`、`MediaRef`），未阻塞 fastsearch 进度。剩余 = **docparse 侧补 `time` 字段 + `Audio/Video` kind**，使其 chunk 直接喂得进 fastsearch（一次 docparse 侧 schema 评审）。

---

## 6. 小结

- 多模态预处理**绝大部分天然属 docparse**，且其中**图提取/字节导出/caption/字幕时间码已经现成**——fastsearch 侧能省掉裁图、caption、渲染器一大块。
- 真正要新建的上游能力只有 **ASR** 与 **视频抽帧**，且都契合 docparse 现有"可插拔 HTTP/ONNX 增强器"架构，按"外接不内化"挂上去即可。
- fastsearch 专注**索引/嵌入/检索/ACL/媒资托管/服务**，不越界做解析与转录。
- 两仓唯一硬耦合 = **共享 chunk schema 的锁步扩展**（§3）；这一处对齐好，整条多模态链路的职责边界就干净了。
