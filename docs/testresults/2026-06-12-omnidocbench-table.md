# 测试结果 · OmniDocBench 表格识别(人工真值 · UniRec)

> 2026-06-12 · 计划 [omnidocbench-benchmark.md](../plans/omnidocbench-benchmark.md) 阶段 1
> 脚本:[`scripts/eval/omnidocbench/table_eval.py`](../../scripts/eval/omnidocbench/table_eval.py) + example [`odb_recognize`](../../crates/docparse-ocr/examples/odb_recognize.rs)

## 为什么换这个 benchmark

旧记分牌(vs ODL/Docling)测**一致度**——和别的系统输出比,且表格真值用**压扁口径**(多级表头子行并一行),导致 opendoc 模型给出真实 span 结构反而扣分。OmniDocBench(CVPR 2025)给**人工标注的 HTML 真值(含真实 rowspan/colspan)**,模型的结构终于被公正评测;它也正是 OpenDoc-0.1B/UniRec 自己报分(90.57%)的 benchmark。

## 方法

OmniDocBench 表格**单模块**协议:给表区域,识别结构出 HTML,与 GT HTML 算 TEDS。
- 输入:OmniDocBench 665 个表(全带 HTML 真值,span 口径),page image 的 `poly` 裁出表区(3× 放大,对齐 `refine_tables` 的 RENDER_SCALE);
- 我方:`UniRec::recognize(表区图) → HTML`(底层统一识别,**绕过确定性检测**——见下"关键发现");
- 评分:两侧 HTML → span 感知格树 → 复用项目精确 **Zhang-Shasha TEDS_X**(H5,`score.py`),数学定界符(`$..$`/`\(..\)`)归一化后比较。

## 结果

| 切片 | 表数 | mean TEDS_X | median |
|---|---|---|---|
| OmniDocBench 表格(前 80,人工真值 span 口径) | 80 | **0.810** | **0.895** |

分布(80 表):满分 4、0.9–1.0 共 33、0.7–0.9 共 24、0.4–0.7 共 14、<0.4 失败 5。**中位数 0.895 说明一半的表识别近乎完美**,均值被少数失败例(复杂表/AR 退化)拉到 0.810——与论文"0.1B 模型在难表上仍有失败"一致。

**对照——同一个 UniRec 模型,两把尺子**:

| 评测 | 真值口径 | UniRec 表 TEDS |
|---|---|---|
| vs Docling(旧记分牌) | 压扁口径 | 0.526(被反噬) |
| **OmniDocBench(人工真值)** | **span 口径** | **0.810** |

**这就是"换 benchmark"的全部意义**:模型没变,把尺子从"和压扁口径的系统输出比一致度"换成"和 span 口径的人工真值比准确度",同一个 UniRec 从 0.526 跳到 **0.810**——0.526 是口径假象,0.810 才是真实表识别能力。

样例(`page-f9583127`,5×N 含数学的表):我方 HTML 与 GT **逐格一致**(`Dimension (D)`/`768 64 64 32 128`…),TEDS 0.995;差异仅数学定界符与个别 OCR 字(`d_ν` vs `d_v`)。

## 阶段 2 · 端到端(检测 + 识别)— 打通后

阶段 1 是**单模块**(GT 表区直喂模型,不掺检测)。阶段 2 打通了 `--layout`(YOLO)检出的表区 → `--table-model` 重抽([提交](../../crates/docparse-ocr/src/layout.rs) `seed_table_regions`),让图像文档**端到端**用上模型:页图 → image-PDF → `docparse --ocr --layout --table-model` → 管线自己检出+识别的表。脚本 [`e2e_table_eval.py`](../../scripts/eval/omnidocbench/e2e_table_eval.py)。

| 评测 | 表区来源 | 表数 | mean TEDS_X |
|---|---|---|---|
| 阶段1 单模块 | GT poly(已知表区) | 80 | 0.810 |
| **阶段2 端到端** | **--layout YOLO 检出** | 8 单表页 | **0.827** |

**检测层损耗极小**:同一个表(`page-f9583127`),单模块 0.995 vs 端到端 0.970——layout 检出的表区足够准,端到端几乎不输"给定 GT 区域"。这坐实打通有效:**图像/扫描文档现在端到端用得上表模型**(此前 `refined: 0`,模型被矢量检测挡在门外)。

## 关键发现:范式不匹配(已由阶段2打通解决)

接入中发现一个架构性事实:
1. **OmniDocBench 输入是页面图像**(.png,实为 JPEG),不是 born-digital PDF;
2. 我们的模型路径(`--table-model`)是"**重抽确定性检测出的表**"——而图像上**确定性表检测失效**(无矢量 ruling line、OCR bbox 对齐不规整),所以整页跑 `--table-model` 时 `table_model_refined: 0`(没表可重抽);
3. 故本评测用 GT poly 裁表区**直接喂 UniRec**(`odb_recognize`),测的是**模型的纯表识别能力**(benchmark 单模块标准做法),不掺我们的检测。

**含义**:UniRec 模型本身在图像表上很强(0.810);但要在 OmniDocBench 这类**图像文档**端到端发挥它,需要**图像上的强区域检测**(确定性检测是为 born-digital 矢量设计的)。**阶段2已解决**:`--layout`(DocLayout-YOLO)的表区接进 `--table-model`(`seed_table_regions` 生成占位表→模型重抽→失败占位清理),端到端 TEDS 0.827、检测损耗仅 0.025。

## 阶段 3 · 整页画像(表 vs 文本)

把表之外的文本也评一次,画出在 OmniDocBench 图像文档上的完整能力画像。文本:页图→`--ocr --layout -f text`→与 GT readable 块(按 `order` 拼接)的字符级相似度(脚本 [`e2e_text_eval.py`](../../scripts/eval/omnidocbench/e2e_text_eval.py);中文无空格,**必须字符级**——词级 `split()` 会把整页中文压成一个 token、相似度假性归零,这是初版的坑)。

| 维度 | 路径 | 子集 | 分数 |
|---|---|---|---|
| 表结构 | UniRec 模型(单模块/端到端) | 80 / 8 | **0.810 / 0.827** |
| 正文文本 | PP-OCRv4 **mobile** OCR + 版面重排 | 10 页 | **0.423**(字符级相似度) |

**诚实定位**:表强(模型,接近论文级)、**文本中等**(0.42)。文本分被两件事拉住——① 我们的 OCR 是**轻量 PP-OCRv4 mobile**(16MB,定位"数字页零成本、扫描件补充"),不是为复杂图像版面优化的重型 OCR;② OmniDocBench 全是图像文档(报纸/手写/试卷/多语种),正是我们**非主场**(born-digital 确定性快路径在这里无文本可用)。这与产品定位一致:**born-digital 优先**(vs-ODL/Docling 记分牌),图像文档是 OCR+模型的补充域。要在图像文本上更强,出口是换重型 OCR / VLM 服务(可插拔域),非确定性核心的债。

## 边界与定位

- 这是**第二记分牌**(图像文档 + 模型路径 + 人工真值),与 vs-ODL/Docling 的 **born-digital 一致度记分牌互补**,不取代:那个测确定性快路径的结构,这个测模型的视觉识别准确度;
- TEDS 为自实现(Zhang-Shasha,标准算法)+ 子集,与论文 90.57%(端到端综合,非表格单项)**量级参照、不逐位可比**;
- 纯评测侧,`odb_recognize` 是 eval-only example,**不进二进制**,默认路径与产物零变化。

## 下一步

- 全量 665 表跑批(夜跑,~1h)拿权威表 TEDS;端到端单表页扩到更大子集;
- **整页综合分**:整页图 → markdown(表 HTML/公式 LaTeX/文本)→ OmniDocBench 端到端综合分(文本 edit-dist + 表 TEDS + 公式 CDM);多表页需 pred↔GT 表匹配。阶段2 已打通检测→模型,整页综合是最后一块;
- 据失败例(0.44/0.49 类)回流 G3-R 模型侧改进(行切分噪声 + 难表)。
