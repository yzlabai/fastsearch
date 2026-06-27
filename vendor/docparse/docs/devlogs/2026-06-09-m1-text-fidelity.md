# Devlog · M1 数字文本保真（标准14度量 + Encoding/Differences/AGL + Tc/Tw/Tz）

> 日期：2026-06-09 · 里程碑：[plans/beating-docling.md](../plans/beating-docling.md) M1 · 状态：✅ 完成
> 结果：14 单测通过（+8）、clippy 零 warning、三件套+2408 零回归、修复 `rectification` 连字丢失

---

## 1. 目标

完成 M1（roadmap P1 文本部分）：让数字原生 PDF 的文本接近无损，作为"赢下 born-digital 文本"的地基。三个子任务：①简单字体 Encoding/Differences + AGL 解码；②标准 14 字体 AFM 宽度度量；③字距操作符 `Tc`/`Tw`/`Tz`。

## 2. 诊断先行（CLAUDE.md §5 标准顺序）

写临时 `examples/diag.rs` dump `1901.03003.pdf` 各字体属性，**纠正了一个根因误判**：

| 字体 | Subtype | ToUnicode | Widths | Encoding |
|---|---|---|---|---|
| F71/F72 NimbusRomNo9L（正文） | Type1 | **false** | true | dict + **Differences 含 fi/fl** |
| arXivStAmP Times-Roman（侧栏戳） | Type1 | false | **false** | 无（→Standard） |
| CMR/CMMI/CMSY（数学） | Type1 | false | true | 无（内建编码） |

**关键发现**：可见症状 `rectification`→`rectication`（"fi" 丢失）的根因是**解码**（正文字体无 ToUnicode、靠 Differences+AGL），**不是宽度**（这些字体有内嵌 Widths）。原计划把它归因于标准14宽度回退，是错的。标准14宽度回退只影响 `arXivStAmP` 这类无 Widths 的非内嵌字体。

→ 据此把 M1 重排为"先解码、后宽度"，二者共用 `code → glyph name` 这一步，故合为一个连贯实现。

## 3. 实现

新增三个模块 + 改两个：

| 文件 | 作用 |
|---|---|
| `encoding_tables.rs`（生成） | 三套 base 编码 `code→glyph name`：STANDARD（取自 Helvetica.afm 内建 C 码）、WINANSI/MACROMAN（cp1252/mac_roman codec + 反查 AGL，C0/DEL 置空匹配 PDF Annex D） |
| `encoding.rs` | AGL（`resources/glyphlist/AdobeGlyphList.txt` 运行时解析）`glyph name→Unicode`；f-连字拆 ASCII（`fi`→`fi`）；`uniXXXX`/`uXXXX` 算法名；Differences 叠加 base 表 |
| `stdmetrics.rs` | 14 个 Adobe Core-14 AFM（`resources/afm/*.afm`，verbatim 保留 Adobe notice）解析为 `glyph name→宽度`；BaseFont→标准名别名（Arial→Helvetica、Nimbus→对应、含 bold/italic 检测） |
| `font.rs` | `FontInfo` 加 `encoding`（简单字体 code→name）与 `afm_widths`；无 ToUnicode 走 encoding/AGL（替代 Latin-1）；无 Widths 时按字形名查 AFM；`decode` 返回 `Decoded{text,advance,glyphs,spaces}` |
| `interpreter.rs` | 加 `Tc`/`Tw`/`Tz` 状态与操作符；位移公式 `tx=(Σw·Tfs+Tc·glyphs+Tw·spaces)·Th`；TJ 调整乘 Th |

**数据来源与许可**：AFM/AGL 是 Adobe 发布的事实度量数据（AFM 的 MustRead.html 许可允许 verbatim 再分发，已随附），非 veraPDF 的 GPL 代码；解析器独立编写。base 编码表由脚本从 codec+AGL 生成，provenance 写在生成文件头（含 cp1252≈WinAnsi 的近似标注与 TODO）。符合 CLAUDE.md §5 许可边界。

## 4. 验证

- **单测 14 个**（+8）：AGL/连字/uni 名/Differences 叠加/AFM 已知宽度（Helvetica space=278, A=667）/别名解析/风格检测。
- **修复确认**：`1901.03003` 中 `rectification`×13、`Rectified` 正确解码（原为 `rectication`）；arXiv 戳读出 `arXiv:1901.03003v1`（标准14 AFM 路径生效）。
- **零回归**：lorem 387 / bialetti 3829 chunk **不变**（ToUnicode 路径未碰）；2408 14991 不变；1901 10730（+2，连字使原空文本块进入输出）；chinese_scan 0（扫描件，符合预期）；无 panic。
- **clippy 零 warning**（顺手修了 4 处既有 lint：cmap 索引循环、interpreter match guard、lib f32 cast、font 生命周期）。

## 5. 边界与遗留

- **标题/作者行词间粘连**（`MORAN:AMulti-Object...`）是**既有问题非本次回归**：标题字体有内嵌 Widths，本次只改了字符（修 fi）未改 advance；根因是 TJ 定位无空格字形时几何重建阈值漏判 → 属 **M3 版面/段落聚合**范畴。
- **CM 数学字体**（CMMI/CMSY）内建编码未解析，仍 Latin-1 兜底 → 乱码；需解析 Type1 字体程序内建 encoding，留待需要时。
- **TrueType 无 /Encoding 默认 Standard**（应为内建 cmap，常 ≈WinAnsi）已在 `simple_encoding` 标注 TODO；仅在无 ToUnicode 时影响，实务罕见。

## 6. 对记分牌的影响（roadmap §6）

- born-digital 文本正确率：连字/重音类缺陷修复，NimbusRom 这类 pdfTeX 正文字体（学术 PDF 主流）从"丢字"到"正确"。
- 确定性：纯查表/解析，无随机，逐字节稳定。
- 体积：内嵌 AFM(14)+AGL ≈ 数百 KB，远低于单文件 20MB 目标。

## 7. 下一步

进入 **M2 IR 脊梁**（版本化 + provenance + 评分骨架）——便宜、高杠杆，解锁后续溯源/切块/路由。
