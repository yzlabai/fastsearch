# 深入分析 · OpenDataLoader + veraPDF-wcag-algs：算法、架构与移植路线

> 日期：2026-06-09 · 目的：把 ODL（Java/veraPDF）的确定性结构抽取算法**吃透**，定位 docparse-rs 与之的差距，给出**许可干净**（参考算法、独立重写、标注出处）的移植路线。
>
> 数据驱动的依据：`compare_odl.py` 同台显示——**文本/阅读顺序/标题已基本追平 ODL**（clean 文档 0.99），唯一确定性可达的大差距是**表格检出覆盖**（2203：ODL 13 vs 我方 3）。本文回答"为什么"与"怎么补"。
>
> 源码：`../opendataloader-pdf`（ODL 处理层）+ `../opendataloader-pdf/reference/verapdf/veraPDF-wcag-algs`（核心算法）。许可：veraPDF 是 GPLv3+/MPLv2，本项目 Apache-2.0——**只参考算法、独立实现、注明出处**（CLAUDE.md §5）。

---

## 0. 一句话结论

ODL = **内容流解释器（取定位 chunk）** → **veraPDF-wcag-algs 语义中间层** → **序列化**。docparse-rs **已拥有两端**（解释器 + XY-cut 阅读顺序 + JSON/MD/Text 输出），**缺的是中间语义层**——尤其是 veraPDF 的**聚类表格识别器**（`TableRecognitionArea`/`TableRecognizer`）。这正是表格覆盖 3 vs 13 的根因，也是下一步**最高杠杆**项。

---

## 1. ODL 流水线架构（移植要照搬的"阶段顺序"）

ODL 是**两阶段**：`extractContents`（PDF→结构化的"页索引内容列表"）→ `generateOutputs`（序列化）。**整个文档模型就是一份 `List<List<IObject>>`（页→有序内容）**，每个阶段**就地改写**这份列表。没有单独的"语义树"对象——树是隐式的（`TableBorder` 含 rows/cells；`PDFList` 含 items），JSON 的 `kids` 递归就是走这些容器。

**阶段顺序（关键，依赖前一阶段对列表的改写）**：

```
1. 内容流解释 → 定位 chunk（TextChunk/ImageChunk/LineChunk/LineArtChunk）   [ChunkParser, veraPDF-parser]
2. 有框表格预处理 → TableBordersCollection                                  [LinesPreprocessingConsumer.findTableBorders]  ← 在任何分页处理之前
── 逐页并行 ──
3. 内容过滤（去重/去微小/去页外/去背景/拆空格/替换字符率）                 [ContentFilterProcessor]
4. （可选）隐藏文本检测（需渲染，串行）                                      [ContrastRatioConsumer]
── 整文档串行 ──
5. （可选）无框表格聚类 → 转 TableBorder 并入 collection                     [ClusterTableConsumer]  ★ 覆盖引擎
── 逐页并行 ──
6. 表格 chunk 归位到 cell + 结构归一 + 递归结构化单元格内容                  [TableBorderProcessor + TableStructureNormalizer]
7. 文本行重建（chunk→TextLine，重插空格）                                    [TextLineProcessor ← ChunksMergeUtils]
── 跨页串行 ──
8. 页眉页脚检测                                                              [HeaderFooterProcessor]
9. 列表检测                                                                  [ListProcessor ← ListLabelsUtils]
── 逐页并行 ──
10. 段落合并（对齐启发式）                                                   [ParagraphProcessor]
11. 标题提升（probability>0.75 → SemanticHeading）                          [HeadingProcessor ← NodeUtils.headingProbability]
── 跨页串行 ──
12. setIDs（cross-link 用）→ 标题分级 detectHeadingsLevels → 嵌套 level     [HeadingProcessor/LevelProcessor]
13. 邻接合并（跨页列表/表格续表）                                            [checkNeighborLists/checkNeighborTables]
14. 阅读顺序 XY-Cut++（逐页并行）                                            [XYCutPlusPlusSorter]  ← 我方已有等价
15. 序列化 kids 树                                                          [JsonWriter + 各 Serializer]
```

**载入式不变量**：① 表格必须在文本行**之前**形成（这样单元格文本从正文流移除）；② 页眉页脚/列表是跨页且在段落**之前**；③ `setIDs` 必在 caption/level 之前。**并行/串行边界**：逐页阶段（过滤/表格归位/文本行/段落标题）可并行；跨页阶段（聚类表格/页眉页脚/列表/邻接/分级）串行。我方现有逐页 rayon 模型与并行那半吻合。

**重要架构判断**：ODL **阅读顺序用 XY-Cut++**（`XYCutPlusPlusSorter`），而**不是** veraPDF 的树 DFS。veraPDF 自身把阅读顺序外包给 PDF tag 树；ODL 在无 tag 时用 XY-cut。**→ 我方 XY-cut 是对的，别退回树 DFS。**

---

## 2. veraPDF-wcag-algs 核心算法逐个拆

### 2.1 文本重建与合并概率层（我方已部分采纳，可再精化）

veraPDF **不做几何排序**——它在已排序的节点树上跑**合并概率分类器**：给定两个相邻片段，判"同行/同段/同列？"。所有阈值都**按字号归一（em 单位）**，这是稳健性的关键。

| 机制 | veraPDF | 常量 | 我方现状 |
|---|---|---|---|
| 同行合并 | `toLineMergeProbability` = 字距概率 × 正常行概率(线性 `1-2·Δbaseline-0.033·Δfontsize`)；上/下标救援（quadratic）| 阈值 0.75；`{2,0.033}` | 我方按几何 gap，无概率模型 |
| 词内/词间空格 | 测 gap 前**扣除首尾空格字形宽**；空格字形 > `0.21em` 强制切词；行内拼接 gap > `0.17em` 才插空格 | `WHITE_SPACE_FACTOR=0.25`/`SPLIT_THRESHOLD_FACTOR=0.21`/`TEXT_LINE_SPACE_RATIO=0.17` | 我方词距阈值 **0.15em**（已 harness 调优，与其同量级）|
| 去断字 | 行尾连字符 `{-,—,soft-hyphen}` 删除后拼接，否则插空格 | `formatLineEnd` | 我方**已做**（软连字符去断字，含跨块）|
| 段落合并 | leading 一致（区间 `[0.7,1.5]em`，且 ≤ 块内既有最大行距 ×1.3）× 缩进一致（左/右/中**任一**对齐，~0.1em）| `DIFFERENT_LINES_PARAM=1.3` | 我方仅"触达列右缘 fill_x + 间距"，更粗 |
| 多栏 | **局部成对**判列（无全局 gutter 模型）| — | 我方 XY-cut 全局分列，**更强** |

**可精化项**（低风险、提文本质量）：① 测 gap 前扣空格字形宽；② 段落合并加 leading-rhythm + 左/右/中对齐三选一（替代单一 fill_x）——这能修我方"多栏左列不重排"与部分误并。但**别引入 veraPDF 的全局状态/tag 依赖**，只取常量与判定函数。

### 2.2 阅读顺序：保持我方 XY-cut

veraPDF 阅读顺序 = 树 DFS（信任 tag 树），**无几何恢复**。我方 XY-cut 正是它缺的那半。`compare_odl` 数据印证：clean 文档我方 NID 0.99 ≈ ODL（ODL 也用 XY-cut++）。**结论：XY-cut 不动**；可借鉴 `toColumnsMergeProbability` 的"成对列判定"做 XY-cut 模糊时的 tie-breaker。

### 2.3 标题检测（我方已做基础版，可升级到 neighbor-relative）

veraPDF 的核心思想：**标题是相对邻居定义的，不是相对全局正文字号**。`headingProbability = min(prob_vs_prev, prob_vs_next)`，两个邻居都得"像正文"。信号加权求和、单一阈值 0.75：

- 字重 Δ（同字体 ±0.55/0.15）、字号阶梯 vs 邻居 **max 字号**（`{0.55,0.4,0.5,0.15}`）、颜色变 +0.1、全大写不对称 +0.25/-0.2、额外上下留白 +0.2、起始数字 +0.15、整行/行首 +0.3/-0.1、下个节点跨页 -0.5；
- 乘以**行数衰减** `max(0,1-0.0291·(lines-1)²)`（标题应短）；跨 >3 块的组不算；
- `HEADING_SIZE_FACTOR=1.5` 只是"字号 > 1.5×邻居"的**加分项**，非硬门；
- 先按 `hasSameStyle`（字重 0.05/字号 0.08/同大小写）做**风格分段**，只测与邻居不同的组。

**标题分级（关键，veraPDF 留空，ODL 自己算）**：`HeadingProcessor.detectHeadingsLevels` 把所有已检出标题按 `TextStyle`（字体+字号+字重+色）排序桶化，风格变一次 level+1；无色标题按字号最近匹配。ODL 还加了 `fontSizeRarityBoost`/`fontWeightRarityBoost`（字号/字重越**罕见**越像标题）。

**我方升级路径**：从"字号>1.25×众数 OR 编号 OR 全大写 OR bold" → ① neighbor-relative 加权分（min(prev,next)，权重照搬）；② 用 max 字号而非众数；③ 字号/字重用**长度加权众数**；④ **加标题分级**（按检出标题的风格桶 + 编号深度 `\d+(\.\d+)*` 点数，二者冲突时数字标题信编号）；⑤ 罕见度加分。当前 MHS 0.61（vs Docling）/0.58（vs ODL），升级空间明确。

### 2.4 列表检测（我方**完全缺失**）

veraPDF 三层：① **逐兄弟 label 分类**（试所有方案取并集：无序符号集 + arabic `\d+`含前导零/宽度跟踪 + ASCII/Unicode roman 含规范性 round-trip + 双射 base-26/重复字母 alpha + circled/korean 码点表）；**显式拒绝 `\d+\.\d+` 单行项与裸数字项**（让节号→数字标题、页码不进列表）；② **流式取连续 run**（cursor：number/prefix/start，要求 next==prev+increment、同 prefix、大小写一致，≥2 项成 interval）；③ **几何精化提交**（run 中若夹着高分标题/NOTE/跨页/左缘分列则切；按左缩进分嵌套层；items 占父节点 ≥ 阈值才提交；首子 LIST_LABEL 余 LIST_BODY）。

**我方**：列表当前是普通段落。补列表能提结构质量（DOCX/HTML 也受益），但对 Docling/ODL 的 NID/TEDS/MHS 三指标影响有限——**优先级低于表格**。

### 2.5 表格检测 ★（最高杠杆，3 vs 13 的根因）

两个独立检测器，同一表示（`TableBorder` 网格）：

#### A. 有框表格（`LinesPreprocessingConsumer` + `TableBorderBuilder` + `TableBorder`）
**不是"找闭合矩形"**，而是：① 把 ruling line 按**图连通**聚成 builder（H×V 相交/近顶点）；② 顶点按 x/y **半径容差聚类**成网格坐标；③ **span 从分隔线的有无推断**（cell 初始 maximal span，遇分隔线切为 1，再累计合并单元格）；④ **删冗余行列**（无 cell 起源于其中的行/列是多余 ruling 的伪迹）；⑤ DataLoader 模式**放松外框要求**（允许开边表）。→ 因此能抓合并单元格、部分线、错位/虚线、只画内线的表。我方有框检测要求闭合矩形 + 外框，**漏掉这些**。

#### B. 无框表格 — 聚类识别器（**覆盖引擎，要重点移植**）

**流式状态机**思想：不是"收集全部文本再找表"，而是**把 token 流喂进一个生长的 `TableRecognitionArea`**，逐 token 判"还属于当前正在积累的表吗？"；一旦 token **打破**区域（`isComplete`），就把区域识别成表（若有效），重置，并把打破的 token + 切出的尾行（`restNodes`）**回收重喂**找下一张表。**这就是一遍扫描出多张表、覆盖高的原因。**

`TableRecognitionArea`（状态机）两相：
- **相 A 建表头带**：首批垂直连续（~1 行距内）的行成 header band；贪心建 header **列**：同行（baseline<0.9em 且 `toLineMergeProbability(table)>0.75`）扩展、下一行（x 重叠且行距<1.5em）追加，**并自适应** `adaptiveNextLineToleranceFactor = lineSpacing×1.05`（**从表头学到行距**，下游沿用——覆盖关键）；两 header 都 x 重叠则 `joinHeaders`。`checkHeaders` 要 ≥2 列且列垂直对齐概率 >0.75。
- **相 B 加 body**：拒绝（关表）当 baseline 落差 >3em / 回到表头带上方 / 落在已附 border 外 / 两侧水平溢出 >1.2em。否则每个 body token 先各成单行 cluster。

`TableRecognizer` 五阶段把区域变网格：
```
preprocess（炸成单行 cluster、按 0.9em 桶分 rowNumber、header 左→右定 colNumber）
calculateInitialColumns（单一 header 包含 → 该列；歧义则待定）
mergeWeakClusters（无 header 的弱 cluster：按 强包含0.0001/包含0.001/中心重叠0.01/重叠0.1/距离1.0 的加权最近 header 归列）
mergeClustersByMinGaps（互为最近邻 + 局部最小 gap → 合并列碎片，无固定阈值）
constructTable（每 cluster 都有 header+col 才建；按 row×col 出网格；updateTableRows 切/并行）
```
最终 `Table.validate`：rows<2/cols<2/2×2<4 填充 → 0 分；否则按"任一 cell 文本与上一行 baseline 重叠"算 intersection，`score=1-maxIntersection`，**<0.75 拒**。消费层再查"每行≥2非空 cell + 列左右单调 + 行上下单调"。

**为什么覆盖远高于"gap 阈值对齐检测"**（这就是 3→13 的差距来源）：
1. **自适应行距**——从表头/正文学行距（`adaptiveNextLineToleranceFactor`、`maxRowGaps`、`pickCompactRows`），固定阈值要么把密表切碎要么把松表并进正文；
2. **表头锚定列 + 内容吸引**（强包含→中心重叠→重叠→距离的级联），无一致空白 gutter 的不规则表（短/空/右对齐数字/多行单元格）仍能归列；
3. **互最近邻 gap 合并**（无尺度，4pt 或 40pt 列间距都行）；
4. **流式 + restNodes 回收**（一页出多张表）；
5. **概率化同行判定**（上下标/脚注/混字体不撕裂单元格）；
6. **先激进接受后几何拒绝**（validation 多准则），不需要前期干净信号。

> 我方现有 `detect_borderless_tables` 本质是第 2/3 点缺失的"gap 阈值对齐"版（内容门控防误判），所以只抓最规则的；`detect_ruled_tables`(booktabs) 抓了横线表。要到 ODL 覆盖，需移植**聚类识别器**（表头锚定 + 吸引级联 + 自适应行距 + restNodes 回收）。

---

## 3. docparse-rs vs ODL：gap 总表

| 阶段 | 我方状态 | 备注 |
|---|---|---|
| 内容流解释（定位 chunk）| ✅ 有 | interpreter/font/cmap |
| 内容过滤（去微小/页外/背景/字符率）| ⚠️ 多数缺 | 影响 precision |
| 有框表格（图连通+顶点聚类+span 推断+删冗余）| ⚠️ 仅闭合矩形+外框 | 漏合并/部分线/错位 |
| **无框聚类表格识别器** | ❌ **缺**（仅 gap 阈值版）| **最高杠杆** |
| 表格 cell 归位 + span + 单元格递归结构化 | ⚠️ 1×1 无 span | TEDS 结构精度 |
| 文本行/词重建 | ✅ 有（含去断字）| 可加扣空格宽 |
| 阅读顺序 XY-cut | ✅ 有 | 保持 |
| 段落合并 | ⚠️ fill_x 粗 | 可加 leading+对齐 |
| 标题提升 | ⚠️ 基础启发式 | 升 neighbor-relative |
| 标题分级 | ⚠️ 仅 bool heading | 加 style 桶 + 编号深度 |
| 列表 | ❌ 缺 | 优先级低 |
| 页眉页脚 | ✅ 有（跨页重复）| ODL 更细（list-label 序列）|
| caption 链接 | ❌ 缺 | 低 |
| 序列化 table/heading/list 节点 | ✅ table/heading 有 | list 缺 |

**两端已具，中间语义层是主战场；其中无框聚类表格识别器是 ROI 最高的单点。**

---

## 4. 怎么做 — 优先级与分期（许可干净：参考算法、独立重写、注明 veraPDF 出处）

### P1（最高 ROI）· 移植聚类表格识别器 → 追平 ODL 表格覆盖
按子 agent 给的端到端计划，落 `core::table` 新增模块（arena 索引而非 Rc/RefCell）：
1. 几何/概率原语：`TextInfoChunk`（fontSize=max、baseLine=min）、`toLineMergeProbability(is_table=true)`、`getUniformProbability`。
2. 数据结构：`TableTokenRow`/`ClusterGap{link:ClusterId,gap}`/`TableCluster{header,col_number,rows,min_left/right_gap}`（arena + tombstone）。
3. `TableRecognitionArea`：相 A（表头带 + 自适应行距 + joinHeaders + checkHeaders）+ 相 B（四条拒绝）。
4. `TableRecognizer` 五阶段（含 `updateMinGap` 平均 gap、`isWeakCluster` 链走、吸引级联）。
5. `Table::validate`（row-intersection 0.75 门）+ `pickCompactRows`/`areSeparateRows` + restNodes 回收。
6. 驱动：按阅读顺序流式喂 token，complete→识别→重置→回喂。
- **常量**：照搬 `TableUtils` 那张表（NEXT_LINE_TOLERANCE 1.05、ONE_LINE 0.9、TABLE_GAP 3.0、NEXT_TOKEN_LENGTH 1.2、各 0.75 阈值、ROW_WIDTH 1.2、INTER_TABLE_GAP 1.8）为命名常量、可调。
- **验收**：`compare_odl.py` 表格召回 3→接近 ODL（5/6→6/6 且每文档表数接近）、含表 TEDS 明显上升；`compare_docling.py` 同步看；三件套零误判（lorem/正文不成表）；确定性。
- **风险/陷阱**（子 agent 标注）：fontSize=max/baseLine=min 的归一要逐调用点对齐；gap 可为负不要 clamp；mutual-nearest 要双向 + header-into-header 排除；postprocess 若覆盖偏低，instrument `mergeWeakClusters`/`mergeClustersByMinGaps`（列吸引），而非调阈值；restNodes 不回收会漏邻接表。
- **工程量**：大但**完全确定、已逐行规格化**。建议先 P1a（区域状态机 + 单表识别 + validate）跑通一张学术表，再 P1b（restNodes 回收多表 + 弱 cluster 吸引）拉满覆盖。

### P2 · 表格结构精度（TEDS）
有框走 `TableBorder` 的顶点聚类 + span 推断 + 删冗余（抓合并单元格）；单元格内容用 `reconstruct_lines` 已就绪；多值单元格按子列再分。验收 TEDS 升。

### P3 · 标题升级（MHS）
neighbor-relative 加权分 + max 字号 + 长度加权众数 + **标题分级**（style 桶 + 编号深度）+ 罕见度加分。低风险、`compare_*` 直接量。

### P4 · 文本/段落精化（NID 边际）
测 gap 扣空格字形宽；段落合并加 leading-rhythm + 左/右/中对齐三选一（修多栏左列）。

### P5（远期/低优先）· 列表、caption、内容过滤、有框外框放松。

---

## 5. 边界与许可

- **只参考算法**：上文所有阈值/流程是 veraPDF 的**事实参数与判定逻辑**，独立用 Rust 重写，在模块 `//!` 注明对应 veraPDF 类（如 `table.rs` 注 `ClusterTableConsumer`/`TableRecognizer`/`TableRecognitionArea`）。**不拷贝 GPL 代码**。
- **不引入全局状态**：veraPDF 的 `StaticContainers`/`isDataLoader`/`isHuman` 是其工程包袱，我方保持纯函数 + 逐页并行，只取常量与决策函数。
- **保持我方优势**：XY-cut 阅读顺序、零依赖单二进制、确定性、可溯源——这些是 ODL（JVM）也给不出的速度/部署维度，已超越。

---

## 6. 决策建议

> 数据已证：文本/阅读顺序/标题**追平 ODL**；唯一确定性可达的大差距是**表格检出覆盖**，根因是缺**聚类表格识别器**。
>
> **下一步 = P1 移植聚类表格识别器**——这是把表格召回从 3 拉到 ODL 级（~13）的唯一确定性路径，规格已逐行拆清，许可干净。工程量大但风险可控（harness 每步量化）。其余（标题升级 P3、段落精化 P4）低风险可穿插。
