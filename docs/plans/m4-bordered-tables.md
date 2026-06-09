# Plan · M4 有框表格检测（TEDS 入口）

> 上层见 [beating-docling.md](beating-docling.md) M4。最大、最难、最有价值。**只做有规则线的表格**（born-digital 常有 ruling lines，可确定性求解），对照 `veraPDF-wcag-algs` 的 `TableBorderConsumer`/`ClusterTableConsumer`——**只参考算法、独立实现、标注出处**（CLAUDE.md §5）。无框/合并单元格显式留给 M7 外接。

## 数据流

```
内容流路径操作符 (m l re c S f B…)  ──interpreter──▶  Vec<Segment>（CTM 变换后、轴对齐 h/v 线段）
text chunks + segments            ──core::table::detect_tables──▶  Vec<Table>
Table 元素入 Page；落在表内的 text chunk 在输出层跳过（避免重复）
```

## 落点

| 文件 | 改动 |
|---|---|
| `pdf/interpreter.rs` | 跟踪当前路径（current point + subpath），处理 `m`/`l`/`re`/`c`(端点)/paint(`S s f F B b`)；按 CTM 变换，收集轴对齐线段；`interpret` 返回 `{elements, segments}` |
| `core/table.rs`（新） | `Segment`、`detect_tables(chunks, segments) -> Vec<Table>`：聚类 h 线 y / v 线 x → 网格 → 单元格 → 文本归位；保守阈值（≥2 行 ≥2 列、共 bbox）防图形误判 |
| `core/ir.rs` | `Element::Table(Table{bbox,page,rows:Vec<Vec<Cell>>})`、`Cell{text,bbox}` |
| `pdf/lib.rs` | interpret 后调 `detect_tables`，push Table 元素 |
| `core/output.rs` + `layout.rs` | Markdown 渲染管道表格；行重建跳过落在表 bbox 内的 chunk（去重）|

## 验收

- bialetti 财报表格被识别为 `Table`，Markdown 输出为管道表格；行列与原表对应（TEDS 起步，目测 + 后续评测）。
- 非表格页（lorem 正文、图形页）**不误判**为表格。
- 三件套 + 2408 文本零回归（表外文本不变）；确定性；clippy 零 warning；`table` 单测（合成线段→网格）。
- **不做**：合并单元格 row/col span（MVP 留 TODO）；无框表格；嵌套表。

## 风险

- 误判：图/公式的散线被当表格 → 用"成网格 + 最小行列数 + 线段共框"严格门控。
- 去重：表内文本既是 chunk 又进单元格 → 输出层按 bbox 跳过，避免两次出现。
- 合并单元格：MVP 不处理，按 1×1 网格归位；显式 TODO。
