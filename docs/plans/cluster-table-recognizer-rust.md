# 实现设计 · 用 Rust 移植 veraPDF 聚类表格识别器（P1）

> 日期：2026-06-09 · 这是 [refer/opendataloader-verapdf-analysis.md](../refer/opendataloader-verapdf-analysis.md) 的**落地实现设计**：把 veraPDF-wcag-algs 的无框聚类表格识别器（`ClusterTableConsumer`/`TableRecognitionArea`/`TableRecognizer`）**独立用 Rust 重写**进 docparse-rs，目标把表格检出覆盖从 3 拉到 ODL 级（~13）。
>
> 许可：参考 veraPDF **算法**、独立实现、模块 `//!` 注明对应类；**不拷贝 GPL 代码**（CLAUDE.md §5）。
>
> 度量：每步用 `scripts/eval/compare_odl.py`（ODL 同台，确定性可达）+ `compare_docling.py` + 三件套零误判回归。

---

## 1. 目标与范围

| | |
|---|---|
| **要解决** | 现有 `detect_borderless_tables` 是"gap 阈值对齐"版，只抓最规则的表；学术/不规则表（短/空/右对齐/多值/多行单元格、无空白 gutter）漏检 → 表格召回 3 vs ODL 13 |
| **怎么解** | 移植**表头锚定 + 内容吸引级联 + 自适应行距 + 流式 restNodes 回收**的聚类识别器——这是覆盖高的根因，非阈值 |
| **P1a** | 区域状态机 + 单表识别 + validate，跑通**一张**学术表，零误判 |
| **P1b** | restNodes 回收（一页多表）+ 弱 cluster 吸引拉满覆盖 |
| **不在范围** | 有框 span 精度（P2）、标题升级（P3）、列表（P5）。本设计只做无框聚类检测 |

---

## 2. 与现有代码的接口（复用什么、加什么）

现有（`crates/docparse-core/src/`）：
- `ir.rs`：`TextChunk{text,bbox:BBox{x0,y0,x1,y1},font_size,font:Option<String>,page,confidence,bold}`、`Cell{text,bbox}`、`Table{bbox,page,rows:Vec<Vec<Cell>>}`、`Element::Table`。
- `table.rs`：`Segment`、`detect_tables`(有框)、`detect_ruled_tables`(booktabs)、`detect_borderless_tables`(旧 gap 版)、`build_rows`/`Row`/`Seg`（行内切 cell，可复用）、`cluster`(一维聚类)、`is_numeric_cell`。
- `layout.rs`：`reconstruct_lines(&[&TextChunk])`（几何词重建，单元格文本用它）。
- `interpreter.rs`（pdf crate）：发射 `TextChunk` + `Vec<Segment>`，按 **有框→ruled→borderless** 顺序检测、push `Element::Table`。

**改动落点**：
1. 新模块 `core::table_cluster`（约 600–900 行），导出 `detect_cluster_tables(chunks: &[&TextChunk]) -> Vec<Table>`。
2. `interpreter.rs` 检测顺序改为：**有框 → ruled → cluster → borderless(兜底)**（cluster 覆盖后，borderless 仅作兜底或退役）。各检测器互斥（exclude 已检出 bbox），与现有 `excl` 累积一致。
3. `ir::Table` 暂不动（无 span，多行单元格塞进 `Cell.text`）；P2 再加 `Cell{row_span,col_span}`。
4. 复用：`BBox` 几何、`layout::reconstruct_lines`（单元格文本）、`is_numeric_cell`（可选门控）。

> **关键架构区别记牢**：veraPDF 在 tag 树上跑、阅读顺序外包；我方在**几何 chunk** 上跑、XY-cut 自管阅读顺序。所以我方 token 流的"reading order"用现有 `reading_order()`（或简单 top→bottom,left→right 排序）喂入即可——**不引入 veraPDF 的 StaticContainers/tag 依赖**。

---

## 3. 核心数据结构（arena 索引，避开 Rc/RefCell）

veraPDF 用可变的 cluster 图（互相引用、`id=null` tombstone）。Rust 用 **`Vec` arena + `usize` 索引 + tombstone**，所有操作取 `&mut [TableCluster]` + 索引，干净绕开借用检查。

```rust
//! core::table_cluster — borderless table recognition.
//! Algorithm referenced from veraPDF-wcag-algs `ClusterTableConsumer` /
//! `TableRecognitionArea` / `TableRecognizer` / `Table`; independently
//! reimplemented (no GPL code).

use crate::ir::{BBox, Cell, Table, TextChunk};

type ClusterId = usize;

/// One visual line's worth of content inside a cluster (≈ veraPDF TableTokenRow).
struct TokenRow<'a> {
    chunks: Vec<&'a TextChunk>, // source chunks on this baseline (one cell-line)
    bbox: BBox,
    font_size: f32,             // MAX over chunks (veraPDF convention)
    base_line: f32,             // baseline ≈ bbox.y0 for LR text; MIN over members
    row_number: Option<i32>,    // assigned in TableRecognizer
}

/// Nearest-neighbour gap on one side (≈ veraPDF TableClusterGap).
#[derive(Clone, Copy)]
struct ClusterGap {
    link: Option<ClusterId>,
    gap: f32, // signed (can be negative when overlapping) — DO NOT clamp
}

/// A column-cell stack of token rows (≈ veraPDF TableCluster).
struct TableCluster<'a> {
    id: Option<ClusterId>,        // None = tombstoned (merged away)
    header: Option<ClusterId>,    // the header column this belongs to; self if it IS a header
    col_number: Option<i32>,
    rows: Vec<TokenRow<'a>>,
    bbox: BBox,
    font_size: f32,               // max
    base_line: f32,               // min (lowest row so far)
    min_left_gap: ClusterGap,
    min_right_gap: ClusterGap,
}

/// The streaming state machine (≈ veraPDF TableRecognitionArea).
struct RecognitionArea<'a> {
    headers: Vec<TableCluster<'a>>,        // header band (phase A result)
    clusters: Vec<TableCluster<'a>>,       // body clusters (phase B)
    bbox: Option<BBox>,
    base_line: f32,                        // running min baseline (lowest row)
    headers_base_line: f32,
    has_complete_headers: bool,
    is_complete: bool,
    is_valid: bool,
    adaptive_next_line_tol: f32,           // starts at NEXT_LINE_TOLERANCE_FACTOR; learns row pitch
    page: usize,
}
```

> 实测：把 cluster 图放进一个 `Vec<TableCluster>` arena（`RecognitionArea.clusters` 即 arena），`ClusterId = usize` 是其下标；合并时被吞的 cluster `id=None`（tombstone）、行转移给吸收方；`actual_clusters()` = `iter().filter(|c| c.id.is_some())`。`min_*_gap.link` 存 `ClusterId`。**不用 Rc/RefCell**。

---

## 4. 几何 / 概率原语（按字号归一）

单独 `mod prob`（或 table_cluster 内）。veraPDF 所有阈值是 `max(fontSize)` 的分数——照搬。

```rust
mod c { // constants ← veraPDF TableUtils / Table (named, tunable)
    pub const WIDTH_TOLERANCE: f32 = 0.33;        // x-containment slack × min fontSize
    pub const NEXT_LINE_TOLERANCE: f32 = 1.05;    // header-band vertical tol + adapt mult
    pub const NEXT_LINE_MAX_TOLERANCE: f32 = 1.5; // hard cap when extending header to new line
    pub const ONE_LINE_TOLERANCE: f32 = 0.9;      // "same line" baseline diff; row bucketing
    pub const TABLE_GAP: f32 = 3.0;               // vertical gap (× fontSize) that ends table/header
    pub const NEXT_TOKEN_LENGTH: f32 = 1.2;       // two-sided horizontal overhang that ends table
    pub const MERGE_PROB_THRESHOLD: f32 = 0.75;
    pub const HEADERS_PROB_THRESHOLD: f32 = 0.75;
    pub const TABLE_PROB_THRESHOLD: f32 = 0.75;
    pub const ROW_WIDTH: f32 = 1.2;               // row "height" in validation
    pub const INTER_TABLE_GAP: f32 = 1.8;         // gap multiple separating one table from next
    pub const WHITE_SPACE_FACTOR: f32 = 0.25;
}

/// Linear/uniform probability ramp (≈ getUniformProbability): 1 inside [a,b],
/// linearly →0 over `width` beyond, clamped [0,1].
fn uniform_prob(interval: (f32, f32), x: f32, width: f32) -> f32 { /* ... */ }

/// Same-line merge probability (≈ ChunksMergeUtils.toLineMergeProbability,
/// is_table=true path). For P1a a defensible MVP: char-spacing gate via
/// whitespace-aware gap + normal-line ramp `1 - 2·Δbaseline - 0.033·Δfontsize`.
fn line_merge_prob(a: &TokenRow, b_first: &TextChunk) -> f32 { /* ... */ }

// x-relations on bboxes, normalized by min/max fontSize as each call site needs:
fn is_containing(outer: &BBox, inner: &BBox, font: f32) -> bool;    // inner x ⊂ outer ± 0.33·font
fn are_center_overlapping(a: &BBox, b: &BBox, font: f32) -> bool;
fn are_overlapping(a: &BBox, b: &BBox) -> bool;
```

> P1a 可把 `line_merge_prob` 的字距项近似为：gap（扣首尾空格宽）/`max(font)` 过 `uniform_prob((0,0.67), ·, 0.33)`，baseline 项过 `1-2·|Δbaseline|/max(font)`。够用，后续按需补上/下标救援。

---

## 5. 算法实现（逐阶段，对照 veraPDF 方法）

### 5.1 `RecognitionArea`（流式状态机）

```rust
impl<'a> RecognitionArea<'a> {
    /// ≈ addTokenToRecognitionArea. Returns nothing; sets is_complete/is_valid.
    fn add_token(&mut self, tok: Token<'a>) {
        if tok.page != self.page { self.is_complete = true; return; }
        if !self.has_complete_headers {
            if self.belongs_to_headers_area(&tok) { self.expand_headers(tok); }
            else {
                self.headers_base_line = self.base_line;
                if self.check_headers() { self.has_complete_headers = true; self.add_cluster(tok); }
                else { self.is_complete = true; }
            }
        } else {
            self.add_cluster(tok);
        }
    }

    // Phase A — header band
    fn belongs_to_headers_area(&self, t: &Token) -> bool; // not >adaptive_tol·font below baseline, not >TABLE_GAP·font above top
    fn expand_headers(&mut self, t: Token);               // expand_header / join_headers; LEARNS adaptive_next_line_tol = lineSpacing·1.05
    fn check_headers(&self) -> bool;                      // ≥2 headers, vertical-alignment prob > 0.75

    // Phase B — body
    fn add_cluster(&mut self, t: Token) {
        // reject (set is_complete) if ANY:
        //   baseline drop > TABLE_GAP·font   |  token above headers_base_line
        //   border attached && token outside |  min(left_overhang,right_overhang) > NEXT_TOKEN_LENGTH·font
        // else: push single-row TableCluster, union bbox, lower base_line, is_valid = true
    }
}
```

`Token` = `TextChunk` 引用，或（可选）一个预成的多行 cluster（来自单列多行段落）。P1a 先只喂单 chunk token。

### 5.2 `TableRecognizer`（五阶段，操作 arena）

```rust
/// ≈ TableRecognizer.recognize(): area → Option<Table> (+ rest tokens to recycle).
fn recognize(area: RecognitionArea) -> (Option<Table>, Vec<RecycledToken>) {
    let mut cl = Arena::from(area);            // headers + body into one Vec arena
    setup_row_and_col_numbers(&mut cl);        // explode→single-line, bucket rowNumber @0.9em, header→colNumber L→R
    calculate_initial_columns(&mut cl);        // single containing header → its column; ambiguous → pending
    merge_weak_clusters(&mut cl);              // weighted nearest-header attraction cascade (0.0001/0.001/0.01/0.1/1.0)
    merge_clusters_by_min_gaps(&mut cl);       // mutual-nearest-neighbour + locally-minimal gap → glue column fragments
    let (table, rest) = construct_table(cl);   // every cluster has header+col? build grid; updateTableRows cut/merge
    match table {
        Some(t) if t.validation_score() >= c::TABLE_PROB_THRESHOLD => (Some(t), rest),
        _ => (None, rest),
    }
}
```

各子函数的精确逻辑（含 `update_min_gap` 取**每邻居平均 gap**、`is_weak_cluster` 沿 min-gap 链走找最近 headered 邻居、`pick_compact_rows` 用学到的 body 行距切尾行进 `rest`）见分析文档 §2.5 与 veraPDF 对应方法，逐行重写。

### 5.3 `validation_score` + `check_table`

```rust
// ≈ Table.validate
fn validation_score(rows: &[Vec<Cell>], font: f32) -> f32 {
    if rows.len() < 2 || ncols < 2 || (rows.len()==2 && ncols==2 && filled < 4) { return 0.0; }
    // maxIntersection over body cells: 1 - (prevRowBaseLine - cellBaseLine)/(font·ROW_WIDTH)
    (1.0 - max_intersection).max(0.0)
}
// ≈ ClusterTableConsumer.checkTable: every row ≥2 filled cells; columns L/R monotonic; rows T/B monotonic.
```

### 5.4 驱动循环（≈ `ClusterTableConsumer.accept` + restNodes 回收）

```rust
pub fn detect_cluster_tables(chunks: &[&TextChunk]) -> Vec<Table> {
    let mut queue: VecDeque<Token> = chunks_in_reading_order(chunks); // reuse reading_order()
    let mut tables = Vec::new();
    let mut area = RecognitionArea::new(/* page of first token */);
    while let Some(tok) = queue.pop_front() {
        area.add_token(tok);
        if area.is_complete {
            if area.is_valid {
                let (table, rest) = recognize(std::mem::take(&mut area).into_inner());
                if let Some(t) = table { tables.push(t); }
                for r in rest.into_iter().rev() { queue.push_front(r); } // recycle (P1b)
            }
            area = RecognitionArea::new(/* page of tok */);
            queue.push_front(tok); // re-feed the breaking token
        }
    }
    // flush trailing area
    tables
}
```

P1a：先不回收 `rest`（一页一表也能跑通一张学术表）；P1b：开回收，一页多表。

---

## 6. Rust 特有处理（借用检查 / arena）

| veraPDF 做法 | Rust 等价 |
|---|---|
| cluster 互引用、`id=null` tombstone | `Vec<TableCluster>` arena；`id: Option<ClusterId>`；`actual_clusters()` 过滤 |
| `minGap.link` 指向 cluster | `ClusterGap{link: Option<ClusterId>}`（下标，非引用）|
| 合并：被吞 cluster 行转移、置 null | `let rows = std::mem::take(&mut cl[victim].rows); cl[keep].rows.extend(rows); cl[victim].id = None;` |
| 每阶段后重排（up→bottom / left→right）| `cl.sort_by(...)` **稳定排序**，且重排后**重建 id↔下标映射**或改用稳定 key（用 `base_line/center_x` 比较，别依赖下标顺序）|
| gap 可为负 | 用 `f32` 有符号比较，不 `max(0.0)` |
| fontSize=max / baseLine=min | `TokenRow`/`Cluster` 构造与 `add` 时维护；各比较点按调用语义选 `min`/`max`（如 `is_containing` 用 min，行距用**下一行**的 font）|

> **借用检查最干净的写法**：所有阶段函数签名 `fn stage(cl: &mut Vec<TableCluster>)`，内部只用 `ClusterId` 下标取 `cl[i]`；需要同时读写两个 cluster 时用 `split_at_mut` 或先取出值再写回。不要在 cluster 里放 `&mut` 引用。

---

## 7. 集成、去重与输出

interpreter（pdf crate）检测顺序：
```rust
let bordered = detect_tables(&text_refs, &segments, page);
let mut excl: Vec<BBox> = bordered.iter().map(|t| t.bbox).collect();
let ruled = detect_ruled_tables(&text_refs, &segments, &excl, page);
excl.extend(ruled.iter().map(|t| t.bbox));
// NEW: cluster recognizer on text not already in a detected table
let cluster_chunks: Vec<&TextChunk> = text_refs.iter().copied()
    .filter(|c| !excl.iter().any(|b| center_in(c, b))).collect();
let cluster = detect_cluster_tables(&cluster_chunks);
excl.extend(cluster.iter().map(|t| t.bbox));
// borderless(旧) 退为兜底或移除
elements.extend(bordered.into_iter().chain(ruled).chain(cluster).map(Element::Table));
```
输出层（`output.rs`/`chunk.rs`）已会把落在表 bbox 内的 chunk 排除出正文、把 `Element::Table` 渲染为管道表格——**无需改**。

---

## 8. 分期、验收、度量

| 阶段 | 交付 | 验收（harness）|
|---|---|---|
| **P1a-1** 原语+常量+数据结构 | `prob`/`c`/`TokenRow`/`Cluster`/`Area` + 单测（uniform_prob、line_merge_prob、is_containing）| 单测过 |
| **P1a-2** 区域状态机 | `add_token`/相 A/相 B + 合成单测（2 列表头+几行 body→区域）| 合成表识别 |
| **P1a-3** Recognizer + validate | 五阶段（先不含弱吸引可留桩）+ `validation_score` + `check_table` | 跑通**一张**真实学术表（如 2305-pg9），lorem/1901 正文**零误判** |
| **P1b-1** restNodes 回收 | 驱动循环开回收 | 一页多表；`compare_odl` 每文档表数接近 ODL |
| **P1b-2** 弱 cluster 吸引 + min-gap 合并 | `merge_weak_clusters`/`merge_clusters_by_min_gaps` 完整 | **召回 3→接近 13**、含表 TEDS 明显升 |

每步跑：`compare_odl.py`（主）、`compare_docling.py`、三件套 + 2408 零回归、确定性 20×、clippy 零 warning、单测全过。

---

## 9. 风险与陷阱（Rust 版，子 agent 标注）

1. **覆盖偏低先查列吸引**：`construct_table` 因"某 cluster 无 header+col"而 bail 是常见漏检——instrument `merge_weak_clusters`/`merge_clusters_by_min_gaps`，别调阈值。
2. **`update_min_gap` 的怪癖**：veraPDF 比较用**平均** gap 但存的是**求和**值——要么精确复制，要么统一用平均（注释说明偏离），否则合并次序漂移。
3. **mutual-nearest 必须双向 + 排除 header-into-header**，否则过度合并列。
4. **排序后下标失效**：每次 `sort` 后若仍按 `ClusterId` 索引旧位置会错位——重排后重建映射或只用几何 key 比较。
5. **restNodes 不回收 = 漏邻接表**（直接关系 3→13）。
6. **gap 负值、font=max/baseline=min** 的逐点对齐。
7. **误判防线**：cluster 表必须过 `validation_score≥0.75` + `check_table`（每行≥2 cell、行列单调）；回归必须确认 lorem/1901/2408 正文不成表（我方现有 borderless 的内容门控经验可作二次保险）。

---

## 10. 如何帮到本项目（收益）

- **表格召回 3→ODL 级（~13）**：`compare_odl`/`compare_docling` 的最大确定性差距——直接量化兑现。
- **TEDS 升**：检出更多表 + 后续 P2 span 精度。
- **连带 NID/MHS**：表内文本不再混入正文（NID）、表头不再误判标题（MHS）——本会话已观察到此连带效应。
- **维持优势**：纯 Rust、确定性、零依赖、单二进制 <10ms——速度/部署仍超 ODL（JVM）与 Docling（神经）。
- **可复用资产**：`prob` 原语（按字号归一的合并概率）也能反哺段落/标题（P3/P4）。

> 结论：这份设计把 P1 拆成可逐步交付、每步可量化的 Rust 工程。**先 P1a 跑通一张表 + 零误判**，再 P1b 拉满覆盖。是把 docparse-rs 在表格维度推到 ODL 确定性水平的明确路径。
