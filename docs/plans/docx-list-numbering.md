# 开发计划 · DOCX 列表/编号保真（2026-06-17）

> 触发：md/html 列表标记修复 + 合成表格 span 之后，**DOCX 列表仍丢结构**——带 `w:numPr` 的项目段落落成普通段落（无 `•`/`N.` 标记、无 `LI` tag），下游当散文。DOCX 是最常见的办公格式，这是"列表"主题里最后一块大缺口。
> 背景：[plans/synthetic-table-cell-fidelity.md](synthetic-table-cell-fidelity.md)（同期表格 span）、md/html 列表修复 commit。复用既有 `PageBuilder::list_item`（`•`/`N.` + `LI` tag）。
>
> **状态（2026-06-17）：已实施并验证，零偏离计划。** 全 workspace test 通过、clippy `--all-targets` 零 warning、回归三件套不变。
> - **落点**：[docx/lib.rs](../../crates/docparse-docx/src/lib.rs) 抽 `document_pages(&Docx)`、新增 `list_marker`/`resolve_level`/`level_start`、Paragraph 臂按 `size<=12 && list_marker` 命中走 `list_item`。无新依赖。
> - **新增测试 3**：TC-01（bullet/ordered/计数器/非列表→None）、TC-02（嵌套深级重启）、TC-03（e2e `document_pages` 出 `LI`-tagged `1./2.` + 普通段落不变）。
> - **真文件验证**（用户本地 .docx，只读、本地）：之前列表项全落成普通段落（0 标记），现 "限定之溪" 检出 **59 bullets + 4 ordered**、"以缺解易" **46 + 12**——`read_docx`→numbering.xml→解析链→标记端到端通。
> - **已标注近似**（§2 不做）：有序统一 `N.`（不渲 a/b/i 字形、不解析 lvlText 模板）；计数器用"进入浅级重置深级"常见语义，不处理 lvlRestart/startOverride。

## 1. 需求三件套

| 维度 | 内容 |
|---|---|
| 目标用户 | 抽取 Word 报告/手册（含项目符号列表、多级编号列表）做 RAG/转 Markdown 的用户 |
| 使用场景 | `docparse report.docx -f markdown\|text\|chunks\|json`。列表是 Word 正文常态；丢标记后编号步骤/要点读起来像连续散文，RAG 切块也丢层级 |
| 产品定位关联 | 与 md/html/adoc/tex 列表行为对齐（一致的 `•`/`N.` + `LI` tag）；CLAUDE.md §3"不静默吞数据" |

## 2. 范围 / 不做什么

**做**：

1. 带 `w:numPr` 的正文段落 → `PageBuilder::list_item`，标记按编号定义解析：`bullet` → `• `，有序（decimal/lowerLetter/…）→ `N. `（统一十进制，与 md/html 一致），`none` → 无标记（仍打 `LI` tag）。
2. 多级编号**计数器**：按 `(numId, ilvl)` 维护；进入某级时重置其更深级（嵌套重启）；有序首项用该级 `start` 值。
3. 解析链：段落 `numPr`(numId+ilvl) → `Numbering`(numId→abstractNumId) → `AbstractNumbering` → `Level`(format/start)。

**不做（记为后续/已标注近似）**：

- **lvlText 模板**（`%1.%2`、前缀文本）：有序统一渲染 `N.`（与 md/html 一致），不解析多占位符模板。
- **有序字形**（lowerLetter→a/b、upperRoman→I/II）：统一十进制 `N.`（与 md/html 一致）。**标注**：字形近似。
- **lvlRestart / startOverride / 跨中断重启**：用"进入浅级重置深级 + 不随非列表段落重置"的常见语义；罕见覆盖项不处理。
- **缩进可视化**：扁平（不加缩进前缀），与 md/html/adoc 一致；ilvl 只用于计数器作用域。
- **编号 headings**：heading 样式（size>12）即使带 numPr 仍按标题渲染，不当列表。

## 3. 数据模型 / API（全部既有，本计划不改 IR/synth）

- 复用 [`PageBuilder::list_item(text, size)`](../../crates/docparse-core/src/synth.rs)（`LI` tag + `•`/`N.` 已被 md/html/adoc/tex 验证）。
- 计数器：`HashMap<(usize /*numId*/, usize /*ilvl*/), u64>`，`document_pages` 内持有，逐段推进。

## 4. 技术可行性（docx-rs 0.4.20，已坐实全公开）

| 读什么 | 路径 | 可读性 |
|---|---|---|
| 段落是否列表项 + numId/ilvl | `p.property.numbering_property: Option<NumberingProperty>`，`np.id: Option<NumberingId{pub id}>`、`np.level: Option<IndentLevel{pub val}>` | **公开字段** |
| numId→abstractNumId | `docx.numberings.numberings: Vec<Numbering{pub id, pub abstract_num_id}>` | **公开** |
| abstract→levels | `docx.numberings.abstract_nums: Vec<AbstractNumbering{pub id, pub levels: Vec<Level>}>` | **公开** |
| level 格式/起始 | `Level{pub format: NumberFormat{pub val:String}, pub start: Start}` | format **公开**；`Start.val` 私有但 `Serialize`=裸 int，`serde_json::to_value` 读 |
| 编号定义是否填充 | `read_docx` 有 `reader/numberings.rs` + `reader/numbering_property.rs` | **读 numbering.xml 入模型** |

**无新依赖**：serde_json 已在 docparse-docx（表格 span 引入）；仅用于读 `Start`。

## 5. 实施落点

| 改哪 | 改什么 |
|---|---|
| [docx/lib.rs](../../crates/docparse-docx/src/lib.rs) | 抽 `document_pages(&Docx)->Vec<Page>`（parse_bytes 调用，便于测试）；Paragraph 臂：`size<=12` 且 `list_marker(..)` 命中 → `list_item(marker+text)`，否则 `paragraph`。新增 `list_marker`/`resolve_level`/`level_start`。导入 `Numberings, Level, Page` |

## 6. 验收标准 + TC（每条 ≥1 TC，unit）

| # | 验收标准 | TC |
|---|---|---|
| A1 | bullet 级 → `• `；decimal 级 → `N. `；计数器随项递增；非列表段落 → None | TC-01 `list_marker` 直测 |
| A2 | 多级：进入浅级重置深级，再入深级从 start 重启 | TC-02 |
| A3 | e2e：`document_pages` 把列表段落出成 `LI`-tagged `1. /2. `，普通段落仍是 `paragraph` | TC-03 |
| A4 | 全 workspace test + clippy 零 warning；表格 span/三件套不回归 | TC-04 CI |

## 7. 风险

| 风险 | 缓解 |
|---|---|
| numPr 有但编号定义缺失/不可解析 | 仍当列表项，退化为 `• `（numPr 已证明是列表项，不丢 list-ness） |
| 计数器跨复杂中断/lvlRestart 不符 Word | 主路径（简单 bullet/有序/基础嵌套）正确；覆盖项标注不处理，不 panic |
| 字形近似（a/b/i 渲染成 1/2） | 与 md/html 一致；标注为已知近似 |

## 8. 结论

读取链全公开、无新依赖、复用已验证的 `list_item`，主路径清晰。唯一需细心的是多级计数器（进入浅级重置深级）。按 §5 一处落点实施，TC 随码同 PR。
