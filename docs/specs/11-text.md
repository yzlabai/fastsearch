# spec · fastsearch-text

> 模块 #2，依赖：fastsearch-core。阶段 P1。上游：[产品设计 §3.2](../plans/2026-06-24-产品设计文档.md)、[模块拆分](00-模块拆分.md)。
> 状态：**已落地**（BM25 + 分词 + 过滤 + ACL + 高亮 + 分面 + **k1/b 自定义打分生效** + 短语/前缀；见 §8）。

## 1. 目的与范围

引擎侧**派生全文索引**，基于 Tantivy。提供：

- 从集合配置构建 Tantivy schema（text 主字段 + heading_path 加权 + fast fields 过滤 + stored 引用）。
- BM25 打分，**暴露 k1/b**（beat ParadeDB 的不可调）。
- 分词器抽象：默认（unicode/英文）+ **中文 jieba**；icu/lindera 列为后续迭代。
- upsert/delete（按 global_id、按 doc_id 批量）、commit。
- search：查询字符串 + 过滤（fast fields）→ top-k `(global_id, bm25_score)`。
- 高亮、分面：列为本模块后续迭代（先打通 BM25+过滤+k1/b）。

**不做**：向量、嵌入、Postgres、CDC（别的模块）。

## 2. 公开接口

```rust
pub struct TextIndexConfig {
    pub k1: f32,                 // 默认 1.2
    pub b: f32,                  // 默认 0.75
    pub tokenizer: TokenizerKind,// Default | Jieba
    pub heading_boost: f32,      // heading_path 字段权重，默认 2.0
}
pub enum TokenizerKind { Default, Jieba }

pub struct TextIndex { /* Tantivy Index + schema 字段句柄 */ }

impl TextIndex {
    pub fn create_in_ram(cfg: TextIndexConfig) -> Result<Self>;
    pub fn open_or_create(dir: &Path, cfg: TextIndexConfig) -> Result<Self>;
    pub fn upsert(&mut self, collection: &str, chunk: &Chunk) -> Result<()>;
    pub fn delete_by_global_id(&mut self, gid: &GlobalId) -> Result<()>;
    pub fn delete_by_doc(&mut self, collection: &str, doc_id: &str) -> Result<()>;
    pub fn commit(&mut self) -> Result<()>;
    pub fn search(&self, query: &str, filter: Option<&Filter>, k: usize) -> Result<Vec<TextHit>>;
}
pub struct TextHit { pub id: GlobalId, pub score: f32, pub citation: Citation }
```

- `global_id` 在索引内以字符串 `collection:doc_id:chunk_id`（= citation_id）作主键字段（STRING, indexed+stored），便于 upsert（先 delete_term 再 add）与删除。
- 错误类型 `TextError`（含 Tantivy 错误包装 + core 错误）。

## 3. Tantivy schema

| 字段 | 类型 | 选项 |
|---|---|---|
| `gid` | STRING | INDEXED + STORED（主键/删除项/回指） |
| `collection` | STRING | INDEXED + FAST（过滤/删除域） |
| `doc_id` | STRING | INDEXED + FAST（按 doc 删除、过滤） |
| `text` | TEXT | indexed(positions)，用配置分词器，参与 BM25 |
| `heading` | TEXT | indexed，分词器同上，检索期 boost |
| `kind` | STRING | FAST（过滤/分面） |
| `modality` | STRING | FAST + STORED（多模态过滤/分面；由 `kind` 派生后落字段，MM3/MM4） |
| `page` | U64 | FAST（范围过滤） |
| `section_id` | U64 | FAST |
| `chunk_id` | U64 | STORED（组装 citation） |
| `tenant` | STRING | FAST（ACL） |
| `acl` | STRING(多值) | FAST（ACL） |
| `heading_path` | STORED(JSON/多值) | 组装 citation |
| `bbox` | STORED(JSON) | 组装 citation |

- BM25：用 `tantivy::query::QueryParser` over [text, heading]，heading 加 boost；Similarity 用 Tantivy BM25，k1/b 通过 schema 的 `TextFieldIndexing` + 自定义 `Bm25` 参数（Tantivy 0.26 支持设定）。若版本不支持直接设 k1/b，则记录为已知限制并在 spec 标注，后续迭代用自定义 Weight。

## 4. 过滤映射

- core `Filter` → Tantivy `BooleanQuery` + 对 fast field 的 `RangeQuery`/`TermQuery`：
  - Eq/Ne/In → Term/Boolean。
  - Gt/Gte/Lt/Lte（page/section_id 数值）→ RangeQuery。
  - HeadingPrefix → 暂用 stored heading_path 后过滤（或对 heading 文本 phrase-prefix；先后过滤，标注迭代）。
  - `modality` → Term over `modality` fast field（MM4）；后过滤侧 `FieldSource::get("modality")` 由 `kind` 派生（`Modality::of_kind_str`），与 vector 侧取值一致 → 两端过滤同构、SUPERSET+精确后过滤不变量 #5 自然成立。
  - ACL：`AclFilter` → `(tenant=...) AND (acl IN allowed ∪ {public})`，作为强制子查询 AND 入主查询。

## 5. 行为规约

- **upsert 幂等**：同 gid 重复 upsert = 覆盖（先 delete_term(gid) 再 add_document）。
- **delete_by_doc**：删除 `(collection,doc_id)` 全部 chunk（对应 doc_id 级替换）。
- **健壮**：查询解析失败返回空结果或显式错误，不 panic；commit 失败上报。
- **确定性**：同分 tie-break 按 gid（与 core fuse 一致），保证可复现。

## 6. 依赖

`fastsearch-core`、`tantivy`、`jieba-rs`（中文）、`serde_json`（stored JSON）、`thiserror`、`tempfile`(dev)。

## 7. 测试用例

1. 建索引 + upsert 3 个 chunk + commit + search → 命中数/顺序正确，返回 citation（page/bbox/heading_path）完整。
2. BM25 排序：含查询词更多/更相关的 chunk 排前。
3. k1/b 生效（或：若版本限制，测试默认打分合理 + 标注）。
4. 中文 jieba 分词：中文查询能命中中文 chunk（"毛利率" 命中含该词的 chunk）。
5. 过滤：`kind=table`、`page>=10` 生效；ACL 强制过滤把越权 chunk 挡掉。
6. upsert 覆盖：同 gid 改文本后重查，旧文本不命中、新文本命中。
7. delete_by_doc：删除后该 doc 全部 chunk 不再命中。
8. 确定性：同库同查询重复结果一致。

## 8. 验收标准与状态

- [x] `cargo test -p fastsearch-text` 全绿（含 BM25 k1/b 生效 +3 测试）；clippy 零 warning；fmt 净。
- [x] §7 用例覆盖：建索引/citation、BM25 排序、中文 jieba、kind+page 过滤、ACL 阻断越权、upsert 覆盖、delete_by_doc、确定性。

**已知限制 / 下一迭代（诚实记账）：**
- ✅ **k1/b 已真生效**（2026-06-27，A11）：Tantivy 0.26 把 BM25 的 `k1=1.2/b=0.75` 写死在 `Bm25Weight`（`const K1/B`）、不暴露入口，故采用**匹配与排序解耦**——Tantivy 负责"哪些 doc 命中"（boolean/短语/前缀/filter/ACL 不变），[`bm25::score_candidates`](../../crates/fastsearch-text/src/bm25.rs) 用配置 `k1/b` 对候选**自算 BM25 重排**（公式与 Tantivy 对齐：`Σ boost·idf·tf(k1+1)/(tf+k1(1-b+b·dl/avgdl))`，`dl`=量化 fieldnorm 同源）。**仅当 `k1/b` 偏离默认才启用**（默认走原生路径，golden 门禁零影响）。server 经 `FASTSEARCH_BM25_K1`/`FASTSEARCH_BM25_B` 配置。已知边界（诚实记账）：候选窗口约束（同 `search_after`）、短语邻近加成不建模、纯前缀命中回退原生分。+3 单测（b 长度归一翻转 / k1 改分 / ≈默认对齐原生）。"beat ParadeDB 的 k1/b 可调"达成。
- ✅ **高亮（snippet）已实现**（2026-06-25）：`search(..., highlight)` 用 Tantivy `SnippetGenerator` 产出 HTML 片段（命中词包 `<b>`），text 字段加 STORED；engine/server 已透出，活服务验证通过。
- ✅ **分面（facets）已实现**（2026-06-25，在 engine 层）：`engine.search_with_facets` 按 `req.facets`（当前 `kind`/`doc_id`）在候选集上计数、确定性排序；server 响应含 `facets`，活服务验证通过。byte 位置高亮、更多分面字段/直方图、icu/lindera 分词、HeadingPrefix 索引侧前缀 → 后续迭代。
- 预过滤目前对 page/section_id/kind/doc_id/tenant/ACL 是真索引侧过滤；**`Ne`/`Not` 已升级为索引侧精确补集**（2026-06-25）：内层能精确翻译时取 `MustNot(精确查询)`（= 精确补集，仍是合法 SUPERSET，post-filter 兜底；见 `query_build::exact_translate`）；`Exists`/`HeadingPrefix` 及不可精确翻译的内层仍退化 AllQuery + 后过滤。

**迭代记录：**
- 2026-06-27 回写多模态（MM3/MM4，代码已实现）：schema 加 `modality` fast field（STRING, FAST+STORED；由 `kind` 派生落字段）；`modality` 作 `Filter` 字段两端下推（§4）；`text=""`（媒资无文本）路径审计——空串不产 term、仍可经 `modality` fast field 真预过滤召回（测试 `empty_text_and_modality_fast_field`）；caption/转录复用 `text` 字段，高亮对转录命中句适用。设计见 [多模态功能设计与开发计划](../plans/2026-06-25-多模态功能设计与开发计划.md)。
- 2026-06-24 v1（完成）：schema + BM25 + jieba + 过滤 + ACL 强制 + upsert/delete + 确定性。10 测试绿。
- 2026-06-25：`Ne`/`Not` 索引侧精确补集翻译（`exact_translate`/`complement`），+1 端到端测试（Ne/Not/Not(And)/Not(Exists 退化）。
- 2026-06-25：**查询能力补全**——短语 `"a b"` 与邻近 `"a b"~N` 经 Tantivy parser 已支持（加回归测试确认）；新增**末词前缀** `term*`（search-as-you-type）：`build_text_query` 识别末词 `*`、用 `RegexQuery` 在 text 字段前缀匹配（前词照常 parse、Should 合并），含引号时不启用。+1 测试（短语/slop/前缀/多词前缀/无匹配）。
