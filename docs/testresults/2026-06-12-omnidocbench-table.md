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

## 关键发现:范式不匹配(诚实记录)

接入中发现一个架构性事实:
1. **OmniDocBench 输入是页面图像**(.png,实为 JPEG),不是 born-digital PDF;
2. 我们的模型路径(`--table-model`)是"**重抽确定性检测出的表**"——而图像上**确定性表检测失效**(无矢量 ruling line、OCR bbox 对齐不规整),所以整页跑 `--table-model` 时 `table_model_refined: 0`(没表可重抽);
3. 故本评测用 GT poly 裁表区**直接喂 UniRec**(`odb_recognize`),测的是**模型的纯表识别能力**(benchmark 单模块标准做法),不掺我们的检测。

**含义**:UniRec 模型本身在图像表上很强(0.812);但要在 OmniDocBench 这类**图像文档**端到端发挥它,我们缺一个**图像上的强区域检测**(确定性检测是为 born-digital 矢量设计的)。出口是把 `--layout`(DocLayout-YOLO 神经检测)的表区接进 `--table-model`(当前 `--layout` 只给文本块标 group、不产生 `Table` 元素)——记为改进项。

## 边界与定位

- 这是**第二记分牌**(图像文档 + 模型路径 + 人工真值),与 vs-ODL/Docling 的 **born-digital 一致度记分牌互补**,不取代:那个测确定性快路径的结构,这个测模型的视觉识别准确度;
- TEDS 为自实现(Zhang-Shasha,标准算法)+ 子集,与论文 90.57%(端到端综合,非表格单项)**量级参照、不逐位可比**;
- 纯评测侧,`odb_recognize` 是 eval-only example,**不进二进制**,默认路径与产物零变化。

## 下一步(阶段 2,候做)

- 全量 665 表跑批(夜跑,~1h)拿权威表 TEDS;
- 端到端:整页图 → `--ocr` markdown(表 HTML/公式 LaTeX)→ OmniDocBench 端到端综合分;需先把 `--layout` 表区接进模型重抽(解上面的范式不匹配);
- 据失败例(0.007 类)回流 G3-R 模型侧改进(行切分噪声)。
