# MM2c-bytes + MM6 inline 媒资服务 — 设计计划

> 状态：**已实施**（MM2c-bytes / MM6-inline / MM6-secure，2026-06-27，收口三绿 + Docker pgvector 真机验证）｜真签名 URL(MM6-signer) / Range(MM6-range) 仍 `gated`（对象存储）。日期：2026-06-27｜上游：[多模态功能设计与开发计划](2026-06-25-多模态功能设计与开发计划.md)（MM2c/MM6）、[职责划分](2026-06-25-多模态职责划分-docparse与fastsearch.md)、[devlog](../devlog/2026-06-27-多模态完善.md)（落地回写见 §5/§6/§7）。
>
> **本文目标**：把"无 caption 文档图的字节端到端服务"（inline 字节 → PG 真源 → `/v1/asset` 网关吐字节）落成可施工设计。这是 MM2c-time 之后剩下的、**触及架构边界**（PG 真源 vs 引擎派生、sync↔async、新数据出口）的部分，按 [DEV_SPEC §0 复杂需求](../../AI_AGENT_DEV_SPEC.md) 先写计划再做。
>
> **代码现状是真源**：签名为设计意图，落地以代码为准并回写。未真机验证项标 `待运行验证`/`gated`。
>
> ### ⚑ 落地回写（2026-06-27，实施后核对代码修正本计划）
> - **关键修正**：计划原设"CLI 把 docparse `data_base64` 解码进 `media_bytes`"（§4.1/§5/§8）。**核对代码后此前提不成立**：CLI `cmd_index`/`cmd_ingest` 经 `engine.ingest()` **只写本地 Tantivy 索引、不写 PG**（[engine lib.rs](../../crates/fastsearch-engine/src/lib.rs) `ingest()→text.upsert()`），且 `media_bytes` 是 transient 字段、不进派生索引——**CLI 根本不是 PG 写入者，无字节可携带**。字节生产者是 **`PgStore::upsert_doc`**（库/外部 PG 写入路径，已绑 `media_bytes` 第 13 参）。"CLI 直写 PG 真源"属另一独立命令特性（非 base64 依赖问题），本期不做。故下文 §4.1/§5/§8 中"CLI 携带 base64"已作废，以本框为准。
> - **已落地**：E1–E6 全部实现并验证（见 §3 表与 §4 各 crate）。MM2c-bytes（`upsert_doc`→`media_bytes`→`fetch_media_bytes`）、MM6-inline（engine `source_pg` + `resolve_citation` Inline→InlineBytes + server `/v1/asset` 吐字节 + HTTP E2E）、MM6-secure（无签名器 Object→404 不泄露裸 key）均收口三绿 + Docker 真机。
> - **§4.2 CDC 决策已定**：**CDC 不搬字节**（复制流只搬指针，`media_bytes` 仅供网关按需直查），减小复制流。
> - **未落地**：MM6-signer（真 S3 presign）、MM6-range（HTTP Range）仍 `gated`（对象存储）。

---

## 1. 需求三件套

- **要做什么**：让"文档内嵌图（无 caption/小裁图）"的**字节**能被检索命中后**内联展示**。链路：docparse `--image-embed`(base64) → CLI ingest 携带字节 → PG `media_bytes bytea` 真源（随逻辑复制走）→ `GET /v1/asset/{cid}` 经 ACL 校验后吐图字节。
- **为什么**：场景 A 的核心是"答案里把图显示出来"。MM2c-time 做了时间维，但 `AssetPointer::Inline` 当前 `resolve_citation` 返回 `None`（字节在 PG 但无人取）——inline 路是死的。这块打通，纯文档（图都是小裁图）场景"一个 PG 即可跑"的内联展示才成立，**无需对象存储**。
- **验收**（✅ 全部达成；①修正：字节生产者是 PG 写入路径 `upsert_doc`，非 CLI——见回写框）：① 经 `PgStore::upsert_doc` 写入带 `media_bytes` 的图 chunk → PG `media_bytes` 有字节，`fetch_media_bytes` 取回一致；② 授权身份 `GET /v1/asset/{cid}` 返回该图字节 + 正确 `Content-Type`；③ 越权身份 404（不暴露存在性）；④ 无字节/无媒资/不可见均 404；⑤ 全程不依赖对象存储。

---

## 2. 范围与不做什么

**纳入**：
- `Chunk` inline 字节通道（写侧）；PG `media_bytes bytea` 列 + 按 gid 取字节；CLI 携带 docparse `data_base64`；`resolve_citation` Inline → `InlineBytes`（从 PG 真源取）；`/v1/asset` 吐字节 + `Content-Type`。

**不做（本期，标注但不展开）**：
- **对象存储签名 URL（真签名）**：`AssetPointer::Object` 的安全短签 URL 需对象存储客户端（S3 presign 类）→ 列 **下一迭代 / gated**；本期仅做"**无签名器时不泄露裸 key**"的安全兜底（见 §4.4）。
- **HTTP Range / 音视频 seek**：inline 是小图、不需要 seek；Range 主要服务对象存储代理的大媒资 → 随对象存储一并做（下一迭代）。
- **大媒资入 PG**：D1 已定大媒资走对象存储；`media_bytes` 只承载 KB 级小裁图（守逻辑复制不放大 WAL）。

---

## 3. 设计决策（拍板）

| # | 决策点 | 选定方案 | 理由 / 取舍 |
|---|---|---|---|
| E1 | inline 字节存哪 | **PG `media_bytes bytea NULL`**（真源），随逻辑复制走（D1 小图） | 字节是**不可重派生的权威内容**——不能只在引擎派生层（违反"派生可重建"）。存 PG 真源 = 崩溃可恢复、CDC 自动同步 |
| E2 | `Chunk` 字节字段放哪 | **`Chunk.media_bytes: Option<Vec<u8>>`**，`#[serde(default, skip_serializing_if=Option::is_none)]` | 不放进 `media jsonb`（base64 膨胀 + 进 Citation 上链）。独立 transient 字段映射独立 bytea 列；Citation 不带字节（只带"如何取"） |
| E3 | 网关从哪取字节 | **引擎经真源 PG 句柄取**（不存进派生索引） | 守"PG真源/引擎派生"：派生 Tantivy/向量索引不持字节。引擎已有 `vector_pg: Arc<PgStore>`（B6）；泛化/复用为真源读句柄 |
| E4 | 真源句柄来源 | **新增 `Engine::set_source_store(Arc<PgStore>)`**（与 `vector_pg` 解耦；B6 开启时可指向同一 `PgStore`） | `vector_pg` 语义是"向量在 PG 跑 ANN"，与"取媒资字节"是两件事。独立句柄更清晰；server 装配时注入。无真源句柄 → Inline 返回 None（降级，文档标注） |
| E5 | sync↔async 桥 | `resolve_citation` 内对 Inline 用 **`block_in_place` + `block_on`**（同 B6 `vector_search`），要求 multi-thread runtime | ACL 校验 + 取字节集中在引擎一处（不可绕过），网关保持薄。代价：要求 multi-thread runtime（B6 已有此约束） |
| E6 | 无签名器时 Object 行为 | **不返回裸 uri**：无 `ObjectSigner` 配置 → `Object` 返回 `None`（404），**绝不 302 到裸 key** | 守不变量 #3（绝不暴露裸 key）。当前代码 `SignedUrl{url:uri}` 直传裸 uri 是隐患，本期顺手修掉（安全兜底，本环境可验证） |

---

## 4. 改造清单（按 crate，自底向上）

### 4.1 core — [model.rs](../../crates/fastsearch-core/src/model.rs)
- `Chunk` 加 `media_bytes: Option<Vec<u8>>`（E2，serde skip）。审计 `Chunk` 字面量构造点（pg/sync/cli/engine/各测试 helper）补 `media_bytes: None`。
- 不动 `Citation`（字节不进引用；引用只带 `media` 的"如何取"）。

### 4.2 pg — [sql.rs](../../crates/fastsearch-pg/src/sql.rs) / [lib.rs](../../crates/fastsearch-pg/src/lib.rs)
- DDL 加 `media_bytes bytea`（MM2c-bytes，承接 MM2c-time 同段注释）。
- `ChunkRow` 加 `media_bytes: Option<Vec<u8>>`；`from_chunk`/`to_chunk` 往返；INSERT 参数 16→17、fetch SELECT 携带；**DDL/SQL 形态 golden 同 commit 更新**（DEV_SPEC §3）。
- **新方法** `PgStore::fetch_media_bytes(collection, doc_id, chunk_id) -> Result<Option<Vec<u8>>>`：`SELECT media_bytes WHERE pk`（E3）。
- sync `row_to_chunk`（[replication.rs](../../crates/fastsearch-sync/src/replication.rs)）：~~CDC 解码携带 `media_bytes`~~ → **已定：CDC 不搬字节**（`row_to_chunk` 设 `media_bytes: None`），复制流只搬指针、字节由网关按需从 PG `media_bytes` 直查，减小复制流。

### 4.3 engine — [lib.rs](../../crates/fastsearch-engine/src/lib.rs)
- 字段 `source_pg: Option<Arc<PgStore>>` + `set_source_store`（E4）。
- `resolve_citation` 的 `AssetPointer::Inline` 分支：有 `source_pg` → `block_in_place` 取 `fetch_media_bytes` → `Some(InlineBytes(bytes))`（仍先过 ACL，再取字节）；无句柄/无字节 → `None`（E5）。
- `AssetPointer::Object`：有 `ObjectSigner` → 签名 URL；无 → `None`（E6）。`ObjectSigner` trait 先定义、默认实现为"无"（真签名 gated）。

### 4.4 server — [lib.rs](../../crates/fastsearch-server/src/lib.rs)
- 装配：从 env/config 建 `PgStore` 时 `engine.set_source_store(store)`（与 B6 `set_pg_vector` 并列；可同一 store）。
- `/v1/asset` 的 `InlineBytes` 已能吐字节（现有），补 `Content-Type`（来自 `ResolvedAsset.media_type`）。
- `Object` 无签名器 → 404（不再 302 裸 uri）。
- 越权用例扩展：`asset_inline_acl_not_bypassable`（带字节的 inline 同样不可绕过）。

---

## 5. 用户使用例子

> ⚠️ 落地修正（见顶部回写框）：**字节生产者是 PG 写入路径 `PgStore::upsert_doc`，不是 CLI**。CLI `index`/`ingest` 只写本地 Tantivy 索引、不写 PG，故无法把字节落进 PG `media_bytes`。下例以"库/外部写入 PG 真源 + server 网关读"为准。

```rust
// 1) 写入 PG 真源（库/外部写入路径）：带 inline 字节的图 chunk → upsert_doc → media_bytes bytea
let chunk = Chunk { /* kind=Image, media: MediaRef{ asset: Inline, .. }, */
                    media_bytes: Some(png_bytes), .. };
pg_store.upsert_doc("kb", "report.pdf", &[chunk]).await?;   // 字节落 PG media_bytes
```

```bash
# 2) server 装配 source_pg（DATABASE_URL）后，检索命中图 chunk → 答案层拿 citation_id 取字节内联展示
curl -H "Authorization: Bearer <key>" http://localhost:8642/v1/asset/kb:report.pdf:42 -o fig.png
#   → 200 image/png 字节（engine resolve_citation Inline→从 PG media_bytes 直查）；越权身份 → 404
```

---

## 6. 测试用例

**单测（本环境必过）**：
- core：`Chunk` serde 含 `media_bytes` skip；构造点编译通过。
- pg：DDL golden 含 `media_bytes bytea`；`ChunkRow` 字节往返；INSERT 17 参；`fetch_media_bytes` SQL 形态。
- engine：`resolve_citation` Inline 无 `source_pg` → None；Object 无签名器 → None（不泄露 uri）。
- server：`asset_inline_acl_not_bypassable`（越权取 inline 字节 404）。

**集成（Docker pgvector，可验证——字节存储不依赖对象存储）**：
- ingest 带 `data_base64` → PG `media_bytes` 有字节 → `fetch_media_bytes` 取回一致。
- 端到端：写 PG → `/v1/asset` 授权取回字节、越权 404。

**gated（标注，不在本期跑）**：对象存储真签名 URL、Range seek。

---

## 7. 验收标准（✅ 全部达成，2026-06-27）

- ✅ §6 单测 + Docker 集成全绿；收口三绿（fmt/clippy -D warnings/test --workspace 30 套件）。
- ✅ ACL 越权用例（inline 字节）硬门禁通过（`mm6_inline_serves_bytes_from_source_pg` 越权 None + server `asset_inline_bytes_e2e` 越权 404）；`Object` 无签名器不泄露裸 key（MM6-secure 单测）。
- ✅ DDL 改动与 golden 同 commit；devlog 记录（§5/§6/§7）+ 回写 12-pg/14-engine/19-server/17-cli spec + 看板。
- ✅ 诚实记账：真签名 URL(MM6-signer) / Range(MM6-range) 标 `gated`；CDC 不搬字节决策已落地回写（§4.2）。

---

## 8. 里程碑拆分（一项一 commit、独立收口）

| 序 | 项 | crate | 资源 | 状态 | 验证 |
|---|---|---|---|---|---|
| MM2c-bytes | `Chunk.media_bytes` 通道 + PG `media_bytes` 列 + `fetch_media_bytes`；字节经 `upsert_doc` 落 PG（~~CLI 携带 base64~~ 作废：CLI 不写 PG，见回写框） | core,pg | ✅ 完成 | 单测往返 + Docker `integration_media_bytes_roundtrip` 字节存取一致 |
| MM6-inline | engine `source_pg` + `resolve_citation` Inline→InlineBytes（block_in_place）；server 装配 + Content-Type + 越权用例 | engine,server | ✅ 完成 | Docker `mm6_inline_serves_bytes_from_source_pg` + server HTTP `asset_inline_bytes_e2e`（200+image/png / 越权 404 / 无 key 401） |
| MM6-secure | `ObjectSigner` trait + 无签名器 Object→404（不泄露裸 key）安全兜底 | engine,server | ✅ 完成 | 单测：无签名器不返回 uri / signed-url 不含裸 key |
| MM6-signer | 真对象存储签名 URL（S3 presign 类） | engine | ⏸ gated（对象存储） | 待运行验证 |
| MM6-range | HTTP Range（对象存储大媒资 seek） | server | ⏸ gated（对象存储） | 待运行验证 |

**执行序**：MM2c-bytes（地基，本环境+Docker 完整验证）→ MM6-inline（端到端，Docker）→ MM6-secure（安全兜底，本环境）均已完成；MM6-signer/range 待对象存储。

---

## 9. 守的不变量 + 风险

- **#1 托管 PG 可移植**：`media_bytes bytea` 是原生类型，不引任何扩展 ✓。
- **#2 PG 真源 / 引擎派生**：字节存 PG 真源、引擎派生层不持字节、网关从真源取（E1/E3）✓。
- **#3 ACL 不可绕过**：字节出口经 `resolve_citation` 强制 ACL；无签名器不泄露裸 key（E6）✓——本期最大安全面。
- **#5 预过滤**：不涉过滤路径。
- **风险**：① ~~CDC 是否搬字节未定~~ → **已定不搬**（§4.2，`row_to_chunk` 设 None）✓；② `block_in_place` 要求 multi-thread runtime（B6 已有约束，文档强调）——已落地并验证（集成测试用 multi-thread runtime）✓；③ `media_bytes` 大字段进 PG 的 TOAST/复制放大——仅限 KB 级小图，超阈值应走 Object（D1）。**注**：字节写入者是 `upsert_doc`（非 CLI），大小阈值校验应在写入侧 producer 落（当前未强约束，标 `下一迭代`）。
