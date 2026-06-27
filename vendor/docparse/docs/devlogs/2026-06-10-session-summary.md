# Devlog · 会话总结：聚类表格 + 评测纠偏 + 标题精度（确定性已达 ODL/Docling 同台）

> 日期：2026-06-09→10 · 主题：把表格检出与评测记分牌推到"确定性前沿"，并证实 clean LTR 已达 docling/ODL 水平
> 方法：诊断（dump 真实数据）→ 参 veraPDF/ODL/Docling 算法独立重写 → `compare_odl`/`compare_docling` 量化 → 提交；净负实验一律按预设标准回退。

## 一、做了什么（全部已并 main，零回归，clippy/单测绿）

### 1. 聚类表格识别器 `core::table_cluster`（参 veraPDF `ClusterTableConsumer` 独立重写）
- **P1a 脚手架**：header 锚定列状态机（`belongs_to_headers_area`/`expand_header` 自适应行距/`check_headers`/`add_cluster`）+ recognizer（行列号、单 header 包含归列、网格、`validate`）+ 驱动 + 单测。
- **P1b 出成果**：吸引级联（`attract_to_header`，放开严格包含）+ **按列喂入**（`split_columns`，sweep-line 栏间沟——解锁双栏论文）+ 精度门（数值/≥3列/逐列均长/密度）。**找到 ruled/borderless 漏掉的真实宽数值表**（2203 `Tags|Bbox`、`Model|mAP`、`Tabula`），表 3→4、MHS +0.014，**零回归**。
- 设计/分析文档：[refer/opendataloader-verapdf-analysis](../refer/opendataloader-verapdf-analysis.md)、[plans/cluster-table-recognizer-rust](../plans/cluster-table-recognizer-rust.md)；devlog [P1a](2026-06-09-p1a-cluster-table-scaffold.md)/[P1b](2026-06-09-p1b-cluster-attraction.md)。

### 2. P1c 结构校验路线——**实验否决**
3 个实验（列对齐门/列分离门/行节律裁图注）全部净负或无效。**根因**：残留误判是页眉/图注/CJK 编号散文这类"2 列对齐结构"，过任何几何门；区分只能靠内容信号（P1b 现用）。连完整 veraPDF 结构管线也救不了（2 列簇==2 header → 不 bail）。devlog [p1c-investigation](2026-06-09-p1c-investigation.md)。

### 3. 评测记分牌纠偏（本会话最大单项收益）
逐词 diff `multi_page`（NID 0.600 但 TEDS/MHS 满分，蹊跷）→ 两个 GT 提取器都**漏列表文本**：
- `odl_extract.py` 没递归 `list items` 键；`docling_gt_extract.py` 跳过 `group` 节点 → 列表项全丢。
- 各修一行递归后（**我方产出未变**）：LTR NID **ODL 0.651→0.722、Docling 0.698→0.763**；`multi_page` 0.600→0.984、`2305` 0.751→0.921、`redp5110` 0.813→0.972。
- devlog [benchmark-harness-fixes](2026-06-09-benchmark-harness-fixes.md)。

### 4. 标题精度（MHS）
`redp5110` 我方 100 标题 vs Docling 22（SQL 代码块每行触发规则）。两道确定性门：去代码/数据行（`= ; { } < >`）+ 裁连续标题串（runs≥3 只留首）。**MHS ODL 0.584→0.614、Docling 0.612→0.625**，NID 不动。

## 二、当前记分牌（去 RTL，born-digital LTR）

| 同台 | NID | MHS | 含表 TEDS |
|---|---|---|---|
| **vs ODL**（15 份）| **0.722** | **0.614** | 0.044 |
| **vs Docling**（10 份）| **0.763** | **0.625** | 0.110 |

逐文档（vs ODL）：`code_and_formula` 0.999、`picture` 0.998、`multi_page` 0.984、`redp5110` 0.972、`2305-pg9` 0.990、`2305` 0.921。

## 三、诚实判定：离 docling/ODL 还有多远

- **clean born-digital LTR：已达其水平**（上列 0.92–0.99；agreement 型指标 0.95+ 即结构同构）。
- 聚合被两类**确定性天花板**拖低，皆属 roadmap N3 **enhancer/版面模型**领域：
  1. **CJK 复杂版面**：`skipped_*` 0.12–0.22、`normal_4pages` 0.48–0.61（韩文信息图/label-value 序）。
  2. **最难双栏论文首页**：`2203` 0.57、`2206` 0.61（作者块/版权脚注多栏阅读顺序；且 ODL 自身文本粘连如 `public,largeground`，无法在不退化下追平）。
- 确定性这条路的**文本/阅读顺序/标题/表格检出**都已采到大头；继续微调边际 <0.01 且回归风险升（本会话 5 个净负实验已验证并回退）。

## 四、进度标注

- ✅ 表格：cluster P1a/P1b 并入（bordered→ruled→cluster→borderless 四检测器）；P1c 否决留档。
- ✅ 评测：两个 GT 提取器修复，记分牌反映真实；clean LTR 达标。

## 五、后续会话延伸（2026-06-10 同日，本节为追加）

上半场收官后继续推进,**近期里程碑基本清空**。各项独立 devlog 见 [devlogs/](.)。

### 1. chunk 栏序修复——双栏论文首页不是天花板,是我方 bug
诊断"`2203`/`2206` 双栏首页"被判为确定性天花板,逐层排查发现 layout 读序全对,坏在 chunk 组装的**页级 y-sort 毁掉栏序**（左右栏 y 重叠被重排）。改为保 layout 序、表格按栏拼接。**`2203` NID 0.568→0.936、`2206` 0.615→0.748;vs ODL 0.722→0.761、vs Docling 0.763→0.832**,仅 `normal_4pages` -0.03（CJK 已知）。[chunk-order-fix](2026-06-10-chunk-order-fix.md)

### 2. N2 服务化（MCP + REST）——agent 可直连
- **MCP stdio**（`docparse mcp`,手写 JSON-RPC,**零新依赖**）:parse_document/get_chunks/locate 三 tool。
- **REST**（`docparse serve`,axum+tokio,用户批准）:`POST /parse` + `/healthz`,与 CLI **逐字节一致**。
- [n2-serving](2026-06-10-n2-serving.md)。模块 10 收口。

### 3. N5 安全预检——模块 9 收口
- **N5a 隐藏文本过滤**（防 prompt injection）:`Tr 3/7`/页外/微字 → `hidden`,渲染输出排除、IR 保留可审计、quality 计数;schema 0.3.0。[n5a](2026-06-10-n5a-hidden-text.md)
- **N5b 资源防护**:`core::limits` 手写 ZIP 中央目录预检（zip-bomb,不解压）+ 页数早停,零依赖。[n5b](2026-06-10-n5b-resource-guards.md)

### 4. 记分牌诚实化第二轮——又三个测量 bug + 一个真产品 bug
TEDS 按内容配对（非索引）+ 只评 2D 表;MHS 修跑页眉漏滤（数字折叠 + 边沿判定）;NFKC 连字归一。**TEDS vs ODL 0.044→0.098、vs Docling 0.110→0.187;MHS 0.614→0.627 / 0.625→0.645;NID 微涨**。[running-header-and-teds-honesty](2026-06-10-running-header-and-teds-honesty.md)

### 当前记分牌（去 RTL born-digital LTR）
| 同台 | NID | MHS | TEDS |
|---|---|---|---|
| vs ODL（15）| **0.764** | **0.627** | 0.098 |
| vs Docling（10）| **0.833** | **0.645** | 0.187 |

### 已完成 / 仅剩
- ✅ M1–M7、N1（评测）、N2（服务化）、N4 大部（表格四检测器）、N5（a 隐藏文本 + b 资源防护）。
- ⬜ **近期仅剩 N3 真实 enhancer**——需 OCR/模型**部署选型决策**（tesseract 外接进程 vs HTTP VLM），用户已暂缓。
- 🧊 其余（N4 表格 recall、CJK 版面）属**确定性天花板**（P1c 已证放宽门必回归）或需人工真值/神经模型。`amt` MHS=0 已查实为 agreement 噪声（ODL 该页 0 标题、我方 2 个 figure-label）——不向参照的 0 调参。**确定性可测纠偏已采尽**。

> **本会话最大教训**:记分牌的大跳**几乎全是评测/输出管线 bug,不是解析能力变化**（列表漏算、chunk 栏序、TEDS 配对、页眉漏滤）。分数可疑时**先怀疑管线**;agreement 指标的 onlyours/onlyodl 差集是最快的 bug 探针。
