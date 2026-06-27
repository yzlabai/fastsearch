# MM2c-bytes + MM6 inline 媒资服务 — 设计计划

> 状态：**设计（未实施）**｜日期：2026-06-27｜上游：[多模态功能设计与开发计划](2026-06-25-多模态功能设计与开发计划.md)（MM2c/MM6）、[职责划分](2026-06-25-多模态职责划分-docparse与fastsearch.md)、[devlog](../devlog/2026-06-27-多模态完善.md)。
>
> **本文目标**：把"无 caption 文档图的字节端到端服务"（docparse 导出 inline 字节 → PG 真源 → `/v1/asset` 网关吐字节）落成可施工设计。这是 MM2c-time 之后剩下的、**触及架构边界**（PG 真源 vs 引擎派生、sync↔async、新数据出口）的部分，按 [DEV_SPEC §0 复杂需求](../../AI_AGENT_DEV_SPEC.md) 先写计划再做。
>
> **代码现状是真源**：签名为设计意图，落地以代码为准并回写。未真机验证项标 `待运行验证`/`gated`。

---

## 1. 需求三件套

- **要做什么**：让"文档内嵌图（无 caption/小裁图）"的**字节**能被检索命中后**内联展示**。链路：docparse `--image-embed`(base64) → CLI ingest 携带字节 → PG `media_bytes bytea` 真源（随逻辑复制走）→ `GET /v1/asset/{cid}` 经 ACL 校验后吐图字节。
- **为什么**：场景 A 的核心是"答案里把图显示出来"。MM2c-time 做了时间维，但 `AssetPointer::Inline` 当前 `resolve_citation` 返回 `None`（字节在 PG 但无人取）——inline 路是死的。这块打通，纯文档（图都是小裁图）场景"一个 PG 即可跑"的内联展示才成立，**无需对象存储**。
- **验收**：① CLI 灌入带 `data_base64` 的图 chunk → PG `media_bytes` 有字节；② 授权身份 `GET /v1/asset/{cid}` 返回该图字节 + 正确 `Content-Type`；③ 越权身份 404（不暴露存在性）；④ 无字节/无媒资/不可见均 404；⑤ 全程不依赖对象存储。

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
- sync `row_to_chunk`（[replication.rs](../../crates/fastsearch-sync/src/replication.rs)）：CDC 解码携带 `media_bytes`（bytea 文本/二进制解码——pgoutput 文本协议下 bytea 是 `\x..` hex，需解码；评估是否值得让 CDC 搬小图字节，或 CDC 只搬指针、字节由网关按需从 PG 拉 → **倾向后者**：CDC 不搬字节，`media_bytes` 仅供网关直查，减小复制流）。**待定，落地时定**。

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

```bash
# 1) docparse 导出带 inline 字节的 chunks（小裁图 base64）
docparse report.pdf --image-embed --out chunks.json

# 2) 灌入（字节落 PG media_bytes 真源）
fastsearch index --data ./data --collection kb --doc-id report.pdf chunks.json

# 3) 检索命中图 chunk → 答案层拿 citation_id 取字节内联展示
curl -H "Authorization: Bearer <key>" http://localhost:8642/v1/asset/kb:report.pdf:42 -o fig.png
#   → 200 image/png 字节；越权身份 → 404
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

## 7. 验收标准

- §6 单测 + Docker 集成全绿；收口三绿（fmt/clippy -D warnings/test --workspace）。
- ACL 越权用例（inline 字节）硬门禁通过；`Object` 无签名器不泄露裸 key。
- DDL 改动与 golden 同 commit；devlog 记录 + 回写 12-pg/14-engine/19-server spec + 看板。
- 诚实记账：真签名 URL / Range 标 `下一迭代`/`gated`，CDC 是否搬字节的决策落地后回写。

---

## 8. 里程碑拆分（一项一 commit、独立收口）

| 序 | 项 | crate | 资源 | 验证 |
|---|---|---|---|---|
| MM2c-bytes | `Chunk.media_bytes` 通道 + PG `media_bytes` 列 + `fetch_media_bytes` + CLI 携带 docparse base64 | core,pg,cli | 本环境 + Docker | 单测往返 + Docker 字节存取一致 |
| MM6-inline | engine `source_pg` + `resolve_citation` Inline→InlineBytes（block_in_place）；server 装配 + Content-Type + 越权用例 | engine,server | 本环境 + Docker（multi-thread） | 端到端取字节 + 越权 404 |
| MM6-secure | `ObjectSigner` trait + 无签名器 Object→404（不泄露裸 key）安全兜底 | engine,server | 本环境 | 单测：无签名器不返回 uri |
| MM6-signer | 真对象存储签名 URL（S3 presign 类） | engine | gated（对象存储） | 待运行验证 |
| MM6-range | HTTP Range（对象存储大媒资 seek） | server | gated（对象存储） | 待运行验证 |

**执行序**：MM2c-bytes（地基，本环境+Docker 完整验证）→ MM6-inline（端到端，Docker）→ MM6-secure（安全兜底，本环境）；MM6-signer/range 待对象存储。

---

## 9. 守的不变量 + 风险

- **#1 托管 PG 可移植**：`media_bytes bytea` 是原生类型，不引任何扩展 ✓。
- **#2 PG 真源 / 引擎派生**：字节存 PG 真源、引擎派生层不持字节、网关从真源取（E1/E3）✓。
- **#3 ACL 不可绕过**：字节出口经 `resolve_citation` 强制 ACL；无签名器不泄露裸 key（E6）✓——本期最大安全面。
- **#5 预过滤**：不涉过滤路径。
- **风险**：① CDC 是否搬字节未定（§4.2）——倾向不搬，落地确认；② `block_in_place` 要求 multi-thread runtime（B6 已有约束，文档强调）；③ `media_bytes` 大字段进 PG 的 TOAST/复制放大——仅限 KB 级小图，超阈值应走 Object（D1），需在 ingest 加大小阈值校验（落地补）。
