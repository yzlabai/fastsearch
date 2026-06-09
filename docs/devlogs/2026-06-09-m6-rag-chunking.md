# Devlog · M6 RAG 切块 + chunk↔bbox 双向引用

> 日期：2026-06-09 · 里程碑：[plans/beating-docling.md](../plans/beating-docling.md) M6 · 状态：✅ 完成
> 结果：`core::chunk`——每 chunk 带 page+bbox+标题面包屑 + `locate()` 反查；纯 core 无新依赖；确定性 20/20

---

## 1. 目标

引用是相对 Docling 的杀手锏：Docling 有 RAG 生态但引用非全链路。把 **chunk ↔ 页码/bbox 双向定位**做成一等公民——agent/RAG 最想要、黑盒模型管线给不全的东西。

## 2. 实现

**前置重构**：给 `layout::Line`/`Block` 加 `page` + union `bbox`（切块/引用要回指源坐标）；抽 `layout::page_blocks(doc)`（重建+去页眉页脚+分段的完整管线），**output 与 chunk 共用**，保证序列化与切块对结构的认知一致（顺带去掉 output 里的重复逻辑）。

`core::chunk`：
- `Chunk{ id, kind(Heading/Paragraph/Table), text, page, bbox, heading_path, char_len }`，serde 可序列化。
- `chunk_document(doc)`：按页取 blocks + tables，**按 y 穿插排序**（顺带修了 M4 输出"表排文本后"的次序糙），维护**标题面包屑栈**（按字号 pop/push 得层级），段落累积到 `target_chars`(~800) 或遇标题/表格/换页即 flush；表格单独成块（TSV 文本）。
- **双向引用**：chunk→源 = `page`+`bbox`；`locate(chunks, page, x, y)` = 坐标→chunk（bbox 命中）。
- CLI 加 `-f chunks`（chunk JSON）。

## 3. 验证

- **1901**（论文）：70 chunks（67 段 + 3 标题），每块带 bbox 可回指。
- **bialetti**：2 table chunk（page+bbox，文本 TSV）+ 3 段。
- **标题面包屑**（HTML 例）：`Revenue`(h2) 在 `Quarterly Report`(h1) 下 → path=`[Quarterly Report]`；其下表格 → path=`[Quarterly Report, Revenue]`。多级嵌套正确。
- **反查**：`locate` 单测——表内点命中表 chunk、表外点不命中。
- **确定性**：chunks 20/20 逐字节一致。
- 跨 PDF/DOCX/HTML 统一（同一 IR → 同一切块）；clippy 零 warning；单测 core 20（+3 chunk）+ pdf 14 + html 3 + docx 1 = **38**。

## 4. 对记分牌（roadmap §6）

- **引用可定位率 100%**：每 chunk 带 page+bbox+provenance（M2 起的地基在此兑现）；`locate` 提供坐标→chunk。这是差异化记分牌的硬证据，Docling 结构上给不全。
- **RAG 可用性**：chunk 带标题面包屑（section 上下文），target_chars 控粒度，三格式统一。最小 RAG demo 的"检索结果高亮回原坐标"已具备数据基础。

## 5. 已知限制

- **段落跨页不合并**：换页即 flush（避免跨页 bbox 歧义）；长跨页段落会被切开。
- **target_chars 是软目标**：超大单段不再二次切分（按块边界）；未做句子级 split。
- **locate 返回首个命中**：重叠 bbox（罕见）只给第一个；未做最小包含框选择。
- 表格 chunk 文本用 TSV；未含合并单元格语义（M4 限制延续）。

## 6. 下一步

进入 **M7 质量评分驱动路由 + 外接 AI 边界**（最后一个近期里程碑）——把 M2 的 `quality` 评分接上"按页判难例 + 可插拔外接"的成本论点。
