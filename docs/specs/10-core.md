# spec · fastsearch-core

> 模块 #1，依赖：无。阶段 P0/P1。上游：[产品设计 §3.4/§3.5](../plans/2026-06-24-产品设计文档.md)、[模块拆分](00-模块拆分.md)。
> 状态：**已完成 v1**（见 §8 迭代记录）。

## 1. 目的与范围

`fastsearch-core` 是纯逻辑地基，**不依赖任何后端**（无 Tantivy/Postgres/向量库）。提供：

- **文档/chunk 数据模型**（对齐 docparse chunk schema + ACL + 向量元数据）。
- **查询 AST**（检索请求的结构化表示）。
- **过滤 AST + 求值器**（可嵌套 AND/OR/NOT，对一行元数据求值）。
- **融合算法**（RRF / 分数归一化 / 加权凸组合）。
- **引用模型**（citation_id 编解码）。
- **后端 trait 定义**（VectorBackend/Embedder/Reranker/TextIndex 的签名，由别的 crate 实现——本 crate 只定义不实现）。
- **错误类型**。

**不做**：任何 I/O、任何具体后端实现、嵌入计算、网络。

## 2. 数据结构

### 2.1 Chunk（文档模型）

```rust
pub struct Chunk {
    pub doc_id: String,
    pub chunk_id: u64,
    pub kind: ChunkKind,          // Paragraph|Table|Image|Heading|ListItem|Code
    pub text: String,
    pub page: u32,
    pub bbox: BBox,               // {x0,y0,x1,y1} f32
    pub heading_path: Vec<String>,
    pub section_id: u64,
    pub char_len: u32,
    pub image_meta: Option<ImageMeta>,
    pub tenant: Option<String>,
    pub acl: Vec<String>,         // 默认 ["public"]
}
```
- `GlobalId`：`(collection, doc_id, chunk_id)` 的稳定标识；`fn global_id(&self, collection) -> GlobalId`。
- `ChunkKind`：serde 用 snake_case（与 docparse 一致：paragraph/table/image/heading/list_item/code）。

### 2.2 查询 AST

```rust
pub struct SearchRequest {
    pub query: String,
    pub mode: SearchMode,         // Keyword|Vector|Hybrid
    pub fusion: Fusion,
    pub filter: Option<Filter>,
    pub vector: Option<Vec<f32>>, // 外部提供向量；None 则需 embedder
    pub embedder: Option<String>,
    pub candidates: usize,        // 宽召回数，默认 150
    pub top_k: usize,             // 最终返回数，默认 20
    pub rerank: Option<RerankSpec>,
    pub auto_merge: bool,
    pub highlight: bool,
    pub explain: bool,
}
```
- 默认值：mode=Hybrid，fusion=RRF{60}，candidates=150，top_k=20。
- `validate() -> Result<(), CoreError>`：top_k>0、candidates>=top_k、semantic_ratio∈[0,1]、rank_constant>0。

### 2.3 过滤 AST

```rust
pub enum Filter {
    And(Vec<Filter>), Or(Vec<Filter>), Not(Box<Filter>),
    Eq(String, FieldValue), Ne(String, FieldValue),
    Gt(String, FieldValue), Gte(String, FieldValue),
    Lt(String, FieldValue), Lte(String, FieldValue),
    In(String, Vec<FieldValue>),
    Exists(String),
    HeadingPrefix(Vec<String>),   // heading_path 前缀匹配
}
pub enum FieldValue { Str(String), Int(i64), Float(f64), Bool(bool) }
```
- 求值：`Filter::eval(&self, row: &dyn FieldSource) -> bool`，`FieldSource` 抽象一行的字段取值（便于 core 不绑定具体存储）。
- 数值比较：Int/Float 可互比；类型不匹配返回 false（不 panic）。
- `HeadingPrefix(p)`：当 row 的 heading_path 以 p 为前缀时 true。
- ACL 强制注入辅助：`Filter::and_acl(self, acl_filter) -> Filter`。

### 2.4 融合

```rust
pub enum Fusion {
    Rrf { rank_constant: f64 },                 // 默认 60
    Normalized { semantic_ratio: f64 },         // min-max 归一化后加权
    Weighted { alpha: f64 },                     // alpha*dense + (1-alpha)*sparse（已归一化）
}
pub struct Scored { pub id: GlobalId, pub score: f64 }
```
- `fuse(keyword: &[Scored], semantic: &[Scored], fusion: &Fusion) -> Vec<Scored>`：合并两路、按融合分降序、稳定 tie-break（同分按 id 升序，保证确定性）。
- RRF：`Σ 1/(k+rank)`，rank 从 1 起。
- Normalized：各路 min-max 到 [0,1]（单元素或全同值时归 1.0），`semantic_ratio*sem + (1-ratio)*kw`。
- 一路为空时退化为另一路。

### 2.5 引用

```rust
pub struct Citation { pub collection, doc_id: String, pub chunk_id: u64,
                      pub page: u32, pub bbox: BBox, pub heading_path: Vec<String>, pub section_id: u64 }
```
- `citation_id`：`"{collection}:{doc_id}:{chunk_id}"`；`fn parse(&str) -> Result<(collection,doc_id,chunk_id)>`（doc_id 可含 `:`？→ 用反向解析：首段=collection、末段=chunk_id、中间=doc_id）。

### 2.6 后端 trait（只定义）

```rust
pub trait FieldSource { fn get(&self, field: &str) -> Option<FieldValue>;
                        fn heading_path(&self) -> &[String]; fn acl(&self) -> &[String]; }
```
（VectorBackend/Embedder/Reranker/TextIndex 的 trait 也在此声明签名，供各 crate 实现；本阶段先放 FieldSource + 与融合/过滤相关的，重后端 trait 可随各模块 spec 落地时补。）

### 2.7 错误

```rust
pub enum CoreError { InvalidRequest(String), InvalidCitation(String), InvalidFilter(String) }
```
用 `thiserror`。

## 3. 行为规约（要点）

- **确定性**：fuse 排序稳定、tie-break 确定；filter 求值无副作用。
- **健壮**：类型不匹配/字段缺失返回 false 或 None，不 panic。
- **serde**：Chunk/SearchRequest/Filter 全可 (de)serialize；枚举 snake_case；与 docparse chunk 字段名一致（kind/page/bbox/heading_path/section_id/char_len）。

## 4. 依赖

`serde`、`serde_json`、`thiserror`。无其他。

## 5. 测试用例（单测，与代码同 crate）

1. **ChunkKind serde**：round-trip，snake_case，未知值报错。
2. **global_id / citation_id**：编码 + 反向解析（含 doc_id 带特殊字符）。
3. **Filter::eval**：
   - Eq/Ne/数值比较（Int vs Float 互比）、类型不匹配=false。
   - And/Or/Not 嵌套；空 And=true、空 Or=false。
   - In、Exists、HeadingPrefix（命中/不命中/空前缀）。
   - and_acl 注入后越权行被过滤。
4. **fuse**：
   - RRF 已知输入算出已知分数（手算对照）。
   - Normalized：单路、全同分、正常分布。
   - 一路空退化；tie-break 确定（同分按 id）。
   - 确定性：打乱输入顺序结果一致。
5. **SearchRequest::validate**：非法 top_k/candidates/ratio 报错。

## 6. 验收标准

- `cargo test -p fastsearch-core` 全绿；`cargo clippy -p fastsearch-core` 零 warning。
- §5 全部用例覆盖；fuse/filter 确定性有测试佐证。
- Chunk schema 字段与 docparse [chunk.json](../../../docparse-rs/schemas/chunk.json) 对齐（kind 取值、字段名）。

## 7. 状态

- [x] v1 实现完成，单测全绿，clippy 零 warning。

## 8. 迭代记录

- 2026-06-24 v1：首版，数据模型 + 查询/过滤 AST + 融合 + 引用 + 错误，单测覆盖 §5。
