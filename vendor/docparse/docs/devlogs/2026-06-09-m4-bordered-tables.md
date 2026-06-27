# Devlog · M4 有框表格检测（TEDS 入口）

> 日期：2026-06-09 · 里程碑：[plans/m4-bordered-tables.md](../plans/m4-bordered-tables.md) · 状态：✅ MVP 完成
> 结果：内容流矢量线段→网格→单元格；bialetti 财报识别为 2 张表，**图形页零误判**；确定性 30/30

---

## 1. 目标

语义层起步：从内容流的**矢量 ruling line** 确定性重建有框表格，输出结构化表格。对照 `veraPDF-wcag-algs` `TableBorderConsumer`，**只参考算法、独立实现**（CLAUDE.md §5）。无框/合并单元格不做（留 M7 外接）。

## 2. 实现

```
内容流路径操作符 → Vec<Segment> → 聚类 ruling → 网格 → 单元格 → 文本归位
```

| 文件 | 改动 |
|---|---|
| `pdf/interpreter.rs` | 新增路径状态（current point/subpath/path buffer）；处理 `m`/`l`/`re`/`c`/`v`/`y`/`h` 构造、`S s f F B b`(+星) 绘制时 flush、`n` 丢弃（跳过裁剪框）；按 CTM 变换收集线段 |
| `core/table.rs`（新） | `Segment` + `detect_tables`：h 线 y / v 线 x 聚类（SNAP=3pt）→ 网格；单元格文本**复用 `layout::reconstruct_lines`**（避免逐字形"C O N T O"、多行单元格按序）；3 单测 |
| `core/ir.rs` | `Element::Table(Table{bbox,page,rows:Vec<Vec<Cell>>})` |
| `core/output.rs` | Markdown 管道表格 / text 制表符行；行重建**排除落在表 bbox 内的 chunk**（去重）；`reconstruct_lines` 改收 `&[&TextChunk]` |
| `core/layout.rs` | `in_any(chunk, boxes)` 排除助手；`reconstruct_lines` 收 chunks |

## 3. 防误判（关键）

图/公式的散线绝不能当表格。硬门控：
- **≥3 条 h 线且 ≥3 条 v 线**（≥2×2 真网格）。
- **外框存在**：上/下 h 线跨 ≥80% 宽、左/右 v 线跨 ≥80% 高（拒散落 figure 线）。
- 裁剪框（`W n`）丢弃。

实测：lorem/1901/2408（含 227 `re`、上千 `l`/`S`）**全 0 表**；bialetti 正确 2 表（p1 25×5、p2 8×5）。

## 4. 验证

- **检出**：bialetti 财报 → 管道表格，单元格文本干净（`CONTO ECONOMICO | 2015 | 2016 |...`，数字正确，词正确重建）。
- **零误判**：lorem/1901/2408 = 0 表。
- **零回归**：无表样例文本不变（0 表→不过滤）；确定性 markdown 30/30；quality 仍工作。
- clippy 零 warning；单测 core 15（+3 table）+ pdf 14。

## 5. 已知限制（诚实标注）

- **单元格粗于视觉行**：bialetti 部分单元格含多个 line-item（如"1)2)3)4)"并一格），因为源 PDF 只在**段级**画线、非每行画线。有框检测忠实反映"画了哪些线"；真正逐行分隔属**无框/对齐表格**，超出 M4。
- **合并单元格**未处理（按 1×1 网格归位）——row/col span 留 TODO。
- **多表/页**：当前每页取全页线段并一个 region；多张分离表会并成一个 bbox。bialetti 每页单表故 OK；多表分离需 ruling 的连通分量，留 TODO。
- **表格定位**：输出层把表排在该页文本块之后（未按 y 严格穿插）——表为主的页 OK，混排页次序略糙。

## 6. 对记分牌（roadmap §6）

- **TEDS 起步**：born-digital 有框表格从"纯文本流"到"结构化网格 + Markdown 表格"。Docling 靠神经表格识别；我们在**有线**场景确定性求解、零依赖、可溯源（单元格带 bbox）。
- 顺带产出**列右缘**信号，后续可回头修 M3 多栏左列重排限制。

## 7. 下一步

进入 **M5 多格式（DOCX→HTML）**。注意：DOCX/HTML 需**新依赖**（解 zip+xml / HTML），按 CLAUDE.md §4 需先与用户确认依赖选型——到此**暂停征询**。
