# MM6-signer — 资产网关「控制面 + 两档签名 URL」设计

> 状态：**设计（未实施）**｜日期：2026-06-28｜上游：[后续开发计划-三条线](2026-06-28-后续开发计划-三条线.md)（第②条线 MM6-signer 的细化）、[MM2c-bytes+MM6-inline 媒资服务设计](2026-06-27-MM2c-bytes与MM6-inline媒资服务设计.md)、[多模态功能设计](2026-06-25-多模态功能设计与开发计划.md)。
>
> **本文目标**：把"搜索命中的图，前端按 id 取到 URL 直接渲染"落成与**项目定位/不变量一致**的设计。核心是把 `/v1/asset` 从"直吐字节"升级为**控制面**（做 ACL 决策 + 按档签发**短时、可直接用的签名 URL**），让浏览器 `<img src>` 开箱即用、同时不破 ACL、不让引擎当 CDN、纯 PG 也能显图。触 ACL 边界，按 [DEV_SPEC §0 复杂需求](../../AI_AGENT_DEV_SPEC.md) 先写计划。
>
> **代码现状是真源**：签名为设计意图，落地以代码为准并回写。未真机验证项标 `待运行验证`/`gated`。

---

## 1. 需求三件套

- **要做什么**：搜索命中带 `citation_id` + 媒资元信息（已有）；前端**按 id 换一个可直接放进 `<img src>` 的 URL**，把图渲染出来。URL 必须经 ACL、短时、不可伪造。
- **为什么**：当前 `GET /v1/asset/{cid}` 靠请求头 API Key 鉴权 → 浏览器 `<img src>` **带不了 `Authorization` 头**，无法直接渲染。且大媒资若让引擎代理字节 = 把无状态派生层绑成服务热路径（违定位）。需要一个既守 ACL、又让前端直接用、又不让引擎当 CDN 的解法。
- **验收**：① 授权身份解析一个图 `citation_id` → 拿到**短时签名 URL**；② 该 URL 直接放进 `<img src>`（**不带 Bearer 头**）能取到图字节 + 正确 `Content-Type`；③ URL 过期后取字节 → 403/404；④ 篡改 token / 换 cid → 403/404（不可伪造、不可挪用）；⑤ 越权身份解析 → 拿不到 URL（404，不暴露存在性）；⑥ **inline 档全程零对象存储**（守"一个 PG 即可跑"）。

---

## 2. 范围与不做什么

**纳入**：
- 资产网关「控制面」化：`/v1/asset/{cid}`（authed）按档**签发短时 URL**；新增**批量解析** `POST /v1/assets/resolve`（一次换 N 个 URL，省往返）。
- **inline 档**：HMAC 签名 token URL，指回 fastsearch 自身的**token 门控字节端点**（新增 `GET /v1/asset/{cid}/bytes?exp=..&sig=..`）。零对象存储。
- **object 档**：落地 `ObjectSigner` 真实现（S3 兼容 presign 类），302 到对象存储签名 URL，字节**前端↔对象存储直连**、不过引擎。
- 搜索响应**保持不变**（id + media 元信息，不加 URL/字节）。

**不做（本期，标注但不展开）**：
- 搜索响应里烘焙长效 URL → **不做**：URL 短时、随 ACL 实时判定，只在解析时签发（见 D-A）。
- 对象存储字节由引擎代理/缓存 → **不做**：违定位（引擎不当 CDN）。
- token 撤销列表 / 有状态会话 → **不做**：靠短 TTL 控制暴露窗口（无状态，守多副本）。
- 真 S3/MinIO 端到端验证 → `gated`（对象存储资源）；inline 档本环境可完整验证。

---

## 3. 设计决策（拍板）

| # | 决策点 | 选定方案 | 理由 / 取舍 |
|---|---|---|---|
| D-A | URL 放搜索响应还是单独解析 | **单独解析**（搜索只回 id+media；URL 在 resolve 时签发） | URL 短时 + 随 ACL 实时判定，不能进可缓存的搜索结果。搜索=可缓存目录、解析=实时授权，两面分离 |
| D-B | 两档怎么取字节 | **inline=签名 token URL 指回自身字节端点；object=302 对象存储签名 URL** | 守 D1 分级 + 不变量 #1（inline 零对象存储）/ 定位（object 字节不过引擎，可 CDN） |
| D-C | ACL 在哪强制 | **签发时强制**：拿到合法 URL 的前提是在 resolve 时过了 ACL | token/presign 即"已授权凭证"（同 S3 presigned 语义）。字节端点只验签、不再查 ACL（合法 token 只能由过 ACL 者签出）→ #3 不破 |
| D-D | inline token 形态 | **HMAC-SHA256(`cid` + `exp`)，密钥取 env/config**；URL 携 `exp`+`sig` | 无状态（密钥多副本共享）、不可伪造、绑定确切 cid（不可挪用别的资产）、短 TTL（默认 300s）。无新增持久化 |
| D-E | ACL 在 TTL 窗口内变更 | **容忍**（短 TTL 限定暴露窗口），文档标注 | 撤销列表=有状态、违无状态。TTL 默认 300s，可配；权衡：极端场景接受最长 TTL 的滞后 |
| D-F | 引擎 vs 服务分层 | **引擎 resolve 只做 ACL+定位（不建 HTTP URL、不为签 URL 取字节）；服务建 inline token URL + 持签名密钥/base_url + 跑字节端点** | HTTP URL/密钥/base_url 是服务关注点；引擎保持无 HTTP 依赖。object 签名经已有 `ObjectSigner`（引擎注入） |
| D-G | 旧 `GET /v1/asset/{cid}` 直吐字节是否保留 | **保留**（authed，server-to-server / 答案层用）；浏览器走 resolve→签名 URL | 后向兼容；现有 MM6-inline 直吐路径不破（答案层 LLM 取字节仍可用） |

---

## 4. 数据流（两步法）

```
① 搜索（已有，不变）
   POST /v1/search → 命中[] 每条带 { citation_id, media:{asset.kind, media_type, region, time}, ... }
                     —— 仅元信息，无 URL/字节，可缓存

② 解析 + 渲染（本期新增「控制面」）
   POST /v1/assets/resolve { ids:[cid,...] }   （authed: Bearer）
        └→ 服务端：principal→acl_for→engine.resolve_citation(cid, acl)  ←★ ACL 强制点
           按档签发：
             inline → { type:"inline", url:"/v1/asset/{cid}/bytes?exp=..&sig=..", expires_s }
             object → { type:"object", url:"<对象存储 presigned>", expires_s }   (302 亦可)
             doc    → { type:"doc_render", doc_id, page, bbox }   (无字节，跳原文)
   前端：<img src="{url}">   ←— inline/object 都是直接可用、免 Bearer 头

   inline 字节面（token 门控，无 Bearer）：
   GET /v1/asset/{cid}/bytes?exp=..&sig=..
        └→ 验 sig=HMAC(cid|exp) 且未过期 → 取 PG media_bytes → 200 + Content-Type
           （验签即授权，不再查 ACL；篡改/过期/换 cid → 403）
   object 字节面：前端↔对象存储直连（不过 fastsearch）
```

---

## 5. 改造清单（按 crate，自底向上）

### 5.1 engine — [lib.rs](../../crates/fastsearch-engine/src/lib.rs)
- `resolve_citation(cid, acl)` 改为**只做 ACL + 定位、不取字节**：`Inline` 分支返回**新增的 `AssetFetch::InlineRef`**（无字节，仅表"该 cid 有 inline 字节可取"；不复用带 `Vec<u8>` 的 `InlineBytes`）。`Object`→`SignedUrl`、`DocRegion`→`DocRender` 不变。**两个消费方都据此分流**（见 5.2）：authed 直吐端点拿 `InlineRef` 后取字节、resolve 端点拿 `InlineRef` 后签 URL。这样 resolve 全程零取字节、两端逻辑统一。
- 字节端点支撑：新增 `Engine::fetch_inline_bytes(cid) -> Result<Option<(bytes, media_type)>>`——**不带 acl 参数**（调用方已授权：authed 端点先过了 `resolve_citation` 的 ACL；token 端点已验签=D-C），从 `source_pg` 真源直查（block_in_place，同 MM6-inline）。**文档强标**：此方法绕过 ACL，仅供"已授权"两端点内部调用，不得新挂别的入口。
- `AssetFetch::InlineBytes(Vec<u8>)` 变体：本设计后 `resolve_citation` 不再产出它（改产 `InlineRef`）；如无其它用法可一并删除（落地时定），避免死变体。
- `Object` 分支：维持经 `ObjectSigner::sign` → `SignedUrl`（已实现）；本期补一个**真 `ObjectSigner` 实现**（S3 兼容 presign）。
- `Object` 分支：维持经 `ObjectSigner::sign` → `SignedUrl`（已实现）；本期补一个**真 `ObjectSigner` 实现**（S3 兼容 presign）。

### 5.2 server — [lib.rs](../../crates/fastsearch-server/src/lib.rs) / main.rs
- **签名密钥 + base_url 装配**：env `FASTSEARCH_ASSET_SIGNING_KEY`（HMAC 密钥，多副本同值）+ `FASTSEARCH_ASSET_URL_TTL`（默认 300s）+ 自身 base path。无密钥 → inline 档退回"仅 authed 直吐字节"（D-G），不签 URL（诚实降级）。
- **新增** `POST /v1/assets/resolve`：authed → 对每个 id `acl_for`→`resolve_citation`→按档签发；越权/不存在的 id 在结果里**省略或标 404**（不暴露存在性）。返回 `[{citation_id, type, url?, expires_s?, doc_render?}]`。
- **新增** `GET /v1/asset/{cid}/bytes`：**不走 Bearer**；解析 `exp`+`sig` → 常量时间比对 `HMAC(cid|exp)` + 未过期 → `engine.fetch_inline_bytes(cid)` → 200 + `Content-Type`；失败 403（过期/篡改/换 cid）。仍受限流。
- `GET /v1/asset/{cid}`（旧，authed）：后向兼容（D-G）——`resolve_citation` 现返回 `InlineRef`，故本端点拿 `InlineRef` 后调 `engine.fetch_inline_bytes(cid)` 直吐字节（ACL 已由 resolve 校验）；`SignedUrl`→302、`DocRender`→JSON 不变。可选：配了签名密钥时，inline 档改 **302 到 `/bytes?token`**（让这个单链接也能直接喂 `<img src>`）。
- 越权用例扩展：`assets_resolve_acl_not_bypassable`、`asset_bytes_token_required`、`asset_bytes_tamper_rejected`、`asset_bytes_expired_rejected`。

### 5.3 OpenAPI / 契约
- `/openapi.json` 增 `POST /v1/assets/resolve` + `GET /v1/asset/{cid}/bytes` 描述；标注 token 形态与 TTL。

### 5.4 clients（可选，本期可不做）
- python/ts SDK 加 `resolve_assets(ids)` 便捷法（薄封装）；前端示例 README。

---

## 6. 用户使用例子

```bash
# ① 搜索（拿 citation_id + media 元信息）
curl -H "Authorization: Bearer $KEY" -d '{"query":"营收趋势图","mode":"hybrid"}' \
  http://localhost:8642/v1/search
#   → 命中含 { "citation_id":"kb:report.pdf:42", "media":{"asset":{"kind":"inline"},"media_type":"image/png"} }

# ② 批量解析成可直接用的短时 URL
curl -H "Authorization: Bearer $KEY" -d '{"ids":["kb:report.pdf:42"]}' \
  http://localhost:8642/v1/assets/resolve
#   → [{"citation_id":"kb:report.pdf:42","type":"inline",
#       "url":"/v1/asset/kb:report.pdf:42/bytes?exp=1719600000&sig=ab12…","expires_s":300}]
```
```html
<!-- ③ 前端直接渲染（URL 自带 token，无需 Bearer 头） -->
<img src="http://localhost:8642/v1/asset/kb:report.pdf:42/bytes?exp=1719600000&sig=ab12…" />
```

---

## 7. 测试用例

**单测（本环境必过）**：
- server：HMAC 签发/验签往返；过期拒绝；篡改 `sig`/换 `cid` 拒绝；常量时间比对。
- server：`/v1/assets/resolve` 越权 id 不返回 URL（404 语义、不暴露存在性）。
- engine：`fetch_inline_bytes` 无 source_pg→None；`AssetFetch::InlineRef` 解析分支。

**集成（Docker pgvector，本环境可验——inline 不依赖对象存储）**：
- 端到端：写 PG（带 media_bytes）→ resolve 拿 token URL → `/bytes?token` 取回字节一致、Content-Type 正确、越权拿不到 URL、过期/篡改 403。

**gated（标注，不在本期跑）**：object 档真 S3/MinIO presign（需对象存储）。

---

## 8. 验收标准

- §7 单测 + Docker inline 集成全绿；收口三绿（fmt/clippy -D warnings/test --workspace）。
- ACL 不可绕过硬门禁：合法 URL 必经 resolve 时 ACL；字节端点仅验签（不可伪造/挪用/过期复用）。
- 浏览器 `<img src>` 免 Bearer 头可渲染 inline 图；object 档 302 不泄露裸 key。
- 诚实记账：object 真 presign 标 `gated`；ACL TTL 窗口滞后取舍写入文档与 spec。
- 回写 14-engine / 19-server spec + 看板 + devlog。

---

## 9. 里程碑拆分（一项一 commit、独立收口）

| 序 | 项 | crate | 资源 | 验证 |
|---|---|---|---|---|
| S1 | engine `AssetFetch::InlineRef` + `fetch_inline_bytes`（无 acl，token 后用） | engine | 本环境+Docker | 单测 + Docker 取字节一致 |
| S2 | server inline 签名：HMAC token + `/v1/asset/{cid}/bytes` token 端点 + 装配密钥/TTL | server | 本环境 | 签发/验签/过期/篡改单测 |
| S3 | server `POST /v1/assets/resolve` 批量解析 + ACL 强制 + OpenAPI | server | 本环境+Docker | 越权不返回 URL + 端到端渲染 |
| S4 | object 档真 `ObjectSigner`（S3 兼容 presign） | engine | gated（对象存储） | 待运行验证（MinIO/S3） |
| S5 | clients SDK `resolve_assets` + 前端示例（可选） | clients | 本环境 | SDK 自测 |

**执行序**：S1（地基）→ S2（inline 签名，本环境完整验证）→ S3（批量解析 + ACL 门禁）→ S4（object，gated）；S5 可选。

---

## 10. 守的不变量 + 风险

- **#1 托管 PG 可移植**：inline 档 token URL 指回自身 + PG bytea，**零对象存储**即可显图 ✓。object 档才需对象存储（opt-in）。
- **#2 PG 真源 / 引擎派生**：字节仍只在 PG 真源（inline）或对象存储（object），引擎不持久、不缓存字节 ✓。
- **#3 ACL 不可绕过**：合法 URL 只能由 resolve 时过 ACL 者签出；token 不可伪造（HMAC）、绑 cid、短时 ✓。字节端点"验签即授权"是 presigned 标准语义，非绕过。
- **无状态 / 多副本**：HMAC 密钥 env 共享、签发/验签纯函数，无 per-副本状态 ✓。
- **定位（引擎不当 CDN）**：object 大字节前端↔对象存储直连、不过引擎；inline 仅 KB 级小图经引擎（D1 已限大小）✓。
- **风险**：① ACL 在 TTL 窗口内变更有最长 TTL 的滞后（D-E，短 TTL 控制；不引撤销列表）；② 签名密钥泄露=可伪造 inline URL → 密钥按 secret 管理、可轮换（轮换=旧 token 立即失效，文档标注）；③ inline 字节经引擎仍占内存——仅限 KB 级小图，超阈值应走 object（D1，ingest 侧大小阈值校验属 MM2c 风险③，落地补）；④ `media_bytes` 大字段进 PG 的 TOAST/复制放大——同上限 KB 级。
