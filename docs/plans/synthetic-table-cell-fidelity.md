# 开发计划 · 合成布局后端的表格/单元格保真（2026-06-17）

> 触发：审查轻量格式后端时发现，结构化格式（HTML/DOCX/XLSX…）在落到合成布局（`docparse_core::synth`）时，丢掉了 IR 本就支持的表格结构信息——**合并单元格的 span** 与 **XLSX 日期单元格**。这些是"数据已在源里、IR 也能表达、但合成路径静默丢弃"的保真缺口，不是模型天花板。
> 背景：紧接 md/html **列表标记** 修复（见同期 commit）之后，同一主题的下一块——把合成后端的结构忠实度补齐到 IR 能力。
> 关联战略：[roadmap.md](../roadmap.md) "速度快、质量好" 中的"质量好"；记分牌②一致度（born-digital 表结构 TEDS_X）。
>
> **状态（2026-06-17）：P1–P4 全部实施并验证，零偏离计划。** 全 workspace test 通过、clippy `--all-targets` 零 warning、回归三件套（lorem/bialetti/1901）不变；HTML colspan/rowspan 与 DOCX gridSpan/vMerge 端到端落到 IR 扁平网格（JSON 验证 anchor span + 复制覆盖位）、XLSX 日期出 ISO（serial 1→`1900-01-01`、1.5→`1900-01-01 12:00:00`、duration 回退数值）。
> - **新增测试**：core synth 4（TC-01/02/03 + bbox 加宽）、HTML 1（TC-04）、DOCX 4（TC-05/06/07 + e2e）、XLSX 1（TC-08）。
> - **两处"坑"已坐实并绕过**：① docx-rs 0.4.20 的 `gridSpan`/`vMerge` 字段私有无 getter → 经其公开 `Serialize` 契约 `serde_json::to_value(&cell.property)` 读（形状被 docx-rs 自身单测钉死）；② calamine `Display` 写裸 serial → 开 `dates` feature 用 `as_datetime()`（chrono 已在 lock，正确处理 1900 闰年）。
> - **无新外部依赖**：docparse-docx 加 `serde_json.workspace=true`（已是 workspace dep）；calamine 加 `dates` feature（chrono 已在依赖图）。
>
> **追加 P5（2026-06-17）：PPTX 表格 span 也落地**（原列"不做"，本次顺手补齐，见 §10）。span 主题现覆盖全部产表后端（HTML/DOCX/PPTX）。

## 1. 需求分析三件套

| 维度 | 内容 |
|---|---|
| 目标用户 | 用 docparse 抽取 **HTML/DOCX 报表表格**（含合并表头/合并行）与 **XLSX 含日期列** 的 RAG/数据工程用户 |
| 使用场景 | `docparse report.docx -f json\|markdown\|chunks`、`page.html`、`book.xlsx`。低频但高价值：合并表头是真实报表的常态，错位会让整张表的列对不齐；日期列出"45000"这种序列号直接不可用 |
| 产品定位关联 | 记分牌②"一致度 vs ODL/Docling"的表结构维度（TEDS_X）；CLAUDE.md §3 不变量"不静默吞数据" |

## 2. 范围 / 不做什么

**做**：

1. 合成布局新增 **span 感知的建表 API**，把"稀疏 span 网格"展开成 IR 约定的**扁平网格**（锚点带 span+`merged=false`、被覆盖位置物化+`merged=true`+复制锚点文本）。
2. **HTML** `<td colspan rowspan>` / `<th>` → 扁平 span 网格。
3. **DOCX** `gridSpan`（横并）+ `vMerge`（纵并）→ 扁平 span 网格。
4. **XLSX** 日期单元格 → ISO 日期字符串（不再是 Excel 序列号）。

**不做（本计划外，记为后续）**：

- **DOCX 列表/编号**（`numbering.xml`）：属"列表"主题（md/html 列表修复的 DOCX 续作），独立计划。
- ~~**PPTX 表格 span**~~ → **已落地（追加 P5，见 §10）**。
- **重构 `docparse-ocr::table_model::parse_html_table` 复用本计划的 core expander**：可去重，但会引入 ocr→core 的耦合且动到已验收的表模型路径；零风险起见本计划只在 core 新增、不动 ocr。注释里互相指认即可。
- XLSX **duration**（`[h]:mm:ss`）单元格：保持现状回退到数值（`as_f64`），不在本次美化（罕见，且无错位风险）。

## 3. 数据模型（IR 既有约定，本计划只生产、不改 IR）

`ir::Cell`（[ir.rs:155](../../crates/docparse-core/src/ir.rs#L155)）已定义 span 语义，**扁平行主序**：

- 锚点（左上）：`row_span`/`col_span` ≥1，`merged=false`，`text=` 单元格文本；
- 被覆盖位置：`merged=true`，`text=` **复制的锚点文本**，`row_span=col_span=1`；
- `serde`：三字段都有默认（1/1/false）+ `skip_serializing_if`，1×1 单元格 JSON 不变。

权威参照实现：[`docparse-ocr::table_model::parse_html_table`](../../crates/docparse-ocr/src/table_model.rs#L122)（pending-rowspan 算法，已测）。本计划把同一算法泛化到 core，供合成后端共用。

## 4. API 设计（core 新增）

```rust
// crates/docparse-core/src/synth.rs（或新 grid 子模块）

/// 稀疏 span 网格里的一个源单元格（被覆盖位置省略，锚点自带 span）。
/// HTML/DOCX 后端产出它；`PageBuilder::table_spanned` 展开成 IR 扁平网格。
pub struct SpanCell { pub text: String, pub row_span: u32, pub col_span: u32 }

impl PageBuilder {
    /// 既有 API 保持不变：等价于全 1×1 的 table_spanned（csv/md/xlsx/pptx/tex/adoc 不动）。
    pub fn table(&mut self, rows: Vec<Vec<String>>, size: f32) { /* 包装 table_spanned */ }

    /// span 感知建表：稀疏网格 →（内部 expand_spans）→ 扁平网格 + 合成 bbox。
    pub fn table_spanned(&mut self, rows: Vec<Vec<SpanCell>>, size: f32) { ... }
}

/// 稀疏→扁平：锚点保 span（merged=false）；被覆盖位置物化（复制文本，merged=true）。
/// 行尾补齐成矩形。镜像 parse_html_table 的 pending-rowspan 算法。
fn expand_spans(sparse: Vec<Vec<SpanCell>>) -> Vec<Vec<FlatCell>> { ... }
```

**几何**：沿用现有 `table` 的均匀网格（`col_w = 内容宽/ncols`，`row_h = 行高+0.4em`）。锚点 bbox 加宽到覆盖其 span 区域；被覆盖位置给本格的 1×1 矩形（合成几何，诚实近似）。

## 5. 技术可行性（已逐项坐实）

| 点 | 结论 | 证据 |
|---|---|---|
| HTML colspan/rowspan | scraper `Element::attr("colspan"/"rowspan")` 可读；省略被覆盖位置正是 expander 的输入约定 | parse_html_table 同款算法 |
| DOCX gridSpan/vMerge **可读** | docx-rs 0.4.20 字段私有**无 getter**，但 `TableCellProperty` 派生 `Serialize`（camelCase）：`serde_json::to_value(&cell.property)` → `gridSpan`=裸整数、`verticalMerge`=`"restart"`/`"continue"`/null | docx-rs 自带 `test_table_cell_prop_json` 钉死该 JSON 形状，不会静默漂移；shape 见 table_cell_property.rs:204 |
| DOCX vMerge 行跨度 | 纵并的跨度不在 Restart 上声明，须**扫描下方 Continue 计数**；预处理时按 grid 列对齐，遇 Continue 给上方锚点 `row_span += 1` 并省略该格，再喂同一 expander | — |
| XLSX 日期 **当前是 bug** | calamine `ExcelDateTime` 的 `Display` 写裸 `value`（f64 序列号），故 `Data::DateTime(dt)=>format!("{dt}")` 输出"45000"类数字 | datatype.rs:771 |
| XLSX 修复 **零新依赖** | calamine `dates` feature 开启即得 `as_datetime()`/`is_datetime()`/`is_duration()`，且**正确处理 Excel 1900 闰年坑**（`if f>=60.0 {f} else {f+1.0}`）；chrono 已在 Cargo.lock | datatype.rs:743；Cargo.lock chrono 0.4.45 |
| serde_json 可用 | 已是 `[workspace.dependencies]`；docparse-docx 加 `serde_json.workspace=true` | Cargo.toml:36 |

**关键诚实点**：docx-rs 经 serde-value 读属性，是用其**公开的 Serialize 契约**（且被其自身测试钉死），非内部 hack；单 `to_value`/单元格，表规模可忽略。

## 6. Phase 拆分（每个独立可交付、独立测试）

| Phase | 内容 | 落点 | 风险 |
|---|---|---|---|
| **P1** | core：`SpanCell` + `expand_spans` + `PageBuilder::table_spanned`；`table` 改为薄包装 | [synth.rs](../../crates/docparse-core/src/synth.rs) | 低（算法已验证；保旧 API 行为） |
| **P2** | HTML：`<td/th colspan rowspan>` → `table_spanned` | [html/lib.rs](../../crates/docparse-html/src/lib.rs) | 低（scraper attr + 复用 expander） |
| **P3** | DOCX：serde-value 读 `gridSpan`/`vMerge` → 归一化 → `table_spanned` | [docx/lib.rs](../../crates/docparse-docx/src/lib.rs)、docx/Cargo.toml | 中（vMerge 跨行对齐最绕，含防御性孤儿 Continue） |
| **P4** | XLSX：开 `dates` feature，`Data::DateTime` → ISO（datetime）/数值回退（duration） | [xlsx/lib.rs](../../crates/docparse-xlsx/src/lib.rs)、Cargo.toml calamine features | 低 |

**实施顺序**：P1 → P2（先用低风险的 HTML 验证 expander）→ P3（最绕，建在已验证的 expander 上）→ P4（独立小修）。

## 7. 验收标准 + 测试用例（每条 ≥1 TC，unit）

| # | 验收标准 | TC | 测试位置 |
|---|---|---|---|
| A1 | `expand_spans` 全 1×1 输入 == 原 `table` 网格（不回归） | TC-01 | core synth tests |
| A2 | colspan：锚点 `col_span=k,merged=false`；右侧 k-1 格 `merged=true`+复制文本 | TC-02 | core synth tests |
| A3 | rowspan：锚点 `row_span=k`；下方同列 k-1 行 `merged=true` | TC-03 | core synth tests |
| A4 | HTML `<th colspan=2>` + `<td rowspan=2>` 端到端进 `Table.rows`，span/merged 正确 | TC-04 | html tests |
| A5 | DOCX `gridSpan=2` → 锚点 col_span=2 + 1 覆盖位 | TC-05 | docx tests（构造带 property 的 TableCell） |
| A6 | DOCX `vMerge restart/continue` → 锚点 row_span=2 + 下行覆盖位 | TC-06 | docx tests |
| A7 | DOCX 孤儿 `continue`（上方无 restart）不 panic、退化为独立格 | TC-07 | docx tests |
| A8 | XLSX 整数日期 → `YYYY-MM-DD`；带时间 → `YYYY-MM-DD HH:MM:SS`；非日期数值不变 | TC-08 | xlsx tests（`ExcelDateTime::new` 造已知序列号） |
| A9 | 全 workspace test + clippy 零 warning；md/html/三件套不回归 | TC-09 | CI |

规则：偏离计划就回写本文件。TC 编号发布后不复用。

## 8. 风险与缓解

| 风险 | 缓解 |
|---|---|
| docx-rs serde JSON 形状随版本漂移 | 形状被 docx-rs 自身单测钉死；读用 `.get().and_then(as_u64/as_str)` 容错，缺字段回退 1×1；bump docx-rs 时 P3 单测即守门 |
| DOCX vMerge 跨行/跨 gridSpan 对齐边角（Continue 的 gridSpan ≠ Restart） | 按起始 grid 列对齐 + 孤儿 Continue 防御退化；不追求 100% 覆盖 Word 所有合并怪象，主路径正确 + 不 panic 即过线 |
| 开 calamine `dates` 改变解析行为 | `dates` 仅加 chrono 转换方法，不改 `Data` variant 与解析；P4 单测验证数值/日期两类单元格 |
| `expand_spans` 与 ocr `parse_html_table` 逻辑分叉 | 两处注释互指；本计划不动 ocr（零风险），去重列为后续 |

## 9. 结论

四块都是"数据在源里、IR 能表达、合成路径却丢弃"的保真缺口，**全可零新依赖落地**，且 span 主路径有 core 内已验证算法可复用。建议按 P1→P2→P3→P4 实施，每 Phase 测试随码同 PR；DOCX vMerge 是唯一需要细心的部分，建在已验证的 expander 上以降风险。

## 10. 追加 Phase 5：PPTX 表格 span（2026-06-17，已实施并验证）

**DrawingML 合并模型与 HTML/DOCX 都不同**：`a:tbl` 的网格在 XML 里**本就完整**——锚点 `<a:tc gridSpan="g" rowSpan="r">` 直接声明双向跨度（rowSpan 在锚点上，不像 DOCX 要数 Continue），被覆盖位置以 `<a:tc hMerge="1"/>`（横向被覆盖）/ `<a:tc vMerge="1"/>`（纵向被覆盖）显式占位。

**归一化**：遍历 `<a:tc>`，**跳过** `hMerge`/`vMerge` 为真的覆盖格，锚点出成 `SpanCell{ col_span=gridSpan, row_span=rowSpan }`（默认 1），喂 `b.table_spanned` → 同一 `expand_spans` 重建扁平网格。PPTX 比 DOCX 更简单（跨度声明在锚点、无须回填）。

**落点**：[pptx/lib.rs](../../crates/docparse-pptx/src/lib.rs) `parse_slide`：表状态 `Vec<Vec<String>>`→`Vec<Vec<SpanCell>>`，`<a:tc>` Start 读 4 属性（`attr_u32`/`attr_flag` 助手），End 非覆盖才 push，`<a:tbl>` End `table_spanned`。导入 `SpanCell`。

**验收 + TC**：TC-P5a gridSpan→col_span+覆盖位、TC-P5b rowSpan via vMerge→row_span+覆盖位、非合并表不回归（既有两测）。**状态**：全 workspace test + clippy 零 warning 通过；非合并 PPTX 表逐字不变。
