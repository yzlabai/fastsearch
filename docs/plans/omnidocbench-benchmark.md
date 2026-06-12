# 计划 · 接入 OmniDocBench:从"一致度"到"人工真值准确度"

> 立项依据:当前记分牌(vs ODL/Docling)测的是**一致度**——和别的系统输出比,且表格真值用压扁口径,导致 opendoc 模型给出更真实的 span 结构反而扣分("一致度≠准确度",见 [closing-docling-gaps.md](closing-docling-gaps.md) 经验②)。换成**人工真值** benchmark 才能公正评测确定性路径与模型路径。
>
> 选 **OmniDocBench**(CVPR 2025,opendatalab):1651 页人工标注,表格 **HTML+LaTeX 双标注(span 口径)**、公式 CDM、文本/阅读顺序 edit-distance;且**正是我们用的 UniRec/OpenDoc-0.1B 自己报分的 benchmark(90.57%)**——接它能直接验证我们的纯 Rust UniRec 接入是否达论文水平。

## 0. 关键认知(决定整个方案)

**OmniDocBench 的输入是页面图像(.png),不是 born-digital PDF。** 推论:
1. 我们的**确定性快路径(内容流解析)在纯图像上无文本可抽** → 必须走 OCR / `--ocr` / `--transcribe-model` / `--table-model` 模型路径;
2. 所以 OmniDocBench **评测的是我们的"图像文档 OCR+模型管线",不是 born-digital 快路径**——这点必须在结果里诚实写明(它不取代 vs-ODL/Docling 的 born-digital 记分牌,是**互补的第二记分牌**:那个测确定性快路径的结构一致度,这个测模型路径的视觉解析准确度);
3. 现成入口:`docparse page.png --ocr [--table-model models/unirec] -f markdown`(PNG 走 OCR 路由,docparse-img 后端);
4. 这恰是回答"用了 opendoc 模型效果怎么样"的**正确场景**——模型在视觉文档上才有用武之地。

## 1. 边界与自给自足策略

- **不搭 OmniDocBench 官方 Python 3.8 评测环境**(依赖重、易碎)。只用它的**数据**(图像 + HTML/LaTeX/text 人工真值),用**我们自己的指标实现**评测:
  - 表格 **TEDS**:复用已有的 [`scripts/eval/score.py`](../../scripts/eval/score.py) 的 Zhang-Shasha 精确树编辑距离(`teds_x`,H5 落地)——TEDS 定义标准,自实现与官方同算法;
  - 文本 **归一化编辑距离**:1 − levenshtein/len(标准,自实现);
  - 公式:先归一化编辑距离(CDM 需要额外渲染比对,二期再说)。
- 与论文 90.57% 对标时注明"自实现指标 + 子集",量级参照,不宣称逐位可比(口径同 vs-Docling 的诚实声明)。
- 纯评测侧,**不进二进制**;新脚本放 `scripts/eval/omnidocbench/`。

## 2. 分阶段(先小后大,各自出数字)

### 阶段 0 · 数据获取 ✅
- [x] `OmniDocBench.json`(40MB)下到 `tmp/omnidocbench/`(不进 repo);摸清结构:1651 页、表 665(全带 HTML span 真值)、中英为主;图像按需 curl(.png 实为 progressive JPEG)。

### 阶段 1 · 表格专项 ✅(2026-06-12,见 [testresults](../testresults/2026-06-12-omnidocbench-table.md))
目标:证明 UniRec 表识别在 **span 口径人工真值**下的真实能力(根治压扁口径反噬)。
- [x] 抽 `category_type=table` 块(poly + GT html);
- [x] **关键调整**:`--table-model` 是"重抽确定性检测的表",图像上检测失效(`refined: 0`)→ 改用 GT poly 裁表区**直接喂 `UniRec::recognize`**(新 eval-only example [`odb_recognize`](../../crates/docparse-ocr/examples/odb_recognize.rs)),测模型纯能力(benchmark 单模块标准做法);
- [x] HTML→span 树→复用 `teds_x`(数学定界符归一化),脚本 [`table_eval.py`](../../scripts/eval/omnidocbench/table_eval.py);
- **验收 ✅**:**80 表 mean TEDS_X 0.810、median 0.895**——**同一 UniRec,vs Docling 压扁口径 0.526 → OmniDocBench span 口径 0.810**,坐实"换尺子见真章"。单表样例 0.995 逐格一致;中位数 0.895 即一半的表近乎完美。
- **副产发现**(记入 testresults):图像文档端到端要发挥模型,需把 `--layout`(YOLO)表区接进 `--table-model`(当前 `--layout` 不产生 Table 元素)。

### 阶段 2 · 端到端(整页 markdown)
- [ ] 适配器:`docparse page.png --ocr [--table-model/--transcribe-model] -f markdown` → 转成 OmniDocBench 端到端格式(表格 `<table>` HTML、公式 `$$..$$`、其余 markdown 段落,阅读顺序即输出顺序);
- [ ] 自写端到端评分:按块类型分别算(文本 edit-dist、表 TEDS、公式 edit-dist),综合分参照官方 `((1−text_edit)*100 + table_TEDS + formula_CDM)/3` 的形;
- [ ] 先跑子集(每类文档若干页)验证管线,再按算力扩大;
- **验收**:出端到端综合分,分文档类型(论文/书/试卷/财报…)给明细,定位强弱项。

### 阶段 3 · 写结果 + 迭代完善
- [ ] `docs/testresults/<date>-omnidocbench.md`:确定性 vs 模型路径分项数据、与论文/同类量级对标、诚实边界(图像输入≠born-digital 快路径);
- [ ] 据结果回流改进点(如表模型行切分噪声 G3-R、OCR 弱项),进 plan/按需池;
- [ ] README 记分牌区补"第二记分牌(OmniDocBench 人工真值)"小节。

## 3. 风险与对策

| 风险 | 对策 |
|---|---|
| 全量 1651 页 OCR+模型很慢 | 先子集出信号;全量用 release + rayon,夜跑;按 category 分批 |
| 我方 markdown 标签与 OmniDocBench 格式不符 | 写薄适配器转换,不改核心输出;表 HTML 已有(table_model 出 HTML) |
| 自实现指标与官方有出入 | 标准算法(TEDS/edit-dist)自实现;注明"量级参照非逐位",必要时事后对官方 docker |
| 图像无文本 → 确定性路径空 | 预期内:本 benchmark 测模型路径;born-digital 快路径仍由 vs-ODL/Docling 记分牌负责 |
| huggingface 下载受限 | hf-cli 已在;失败则 hf-mirror 或 API 逐文件取 |

## 4. 验收(整体)

- 阶段 1 表格 TEDS 出数(with/without 模型),证明模型在 span 口径下的真实增益;
- 阶段 2 端到端综合分出数,和论文 90.57% 量级对标,分类型明细;
- testresults 落档 + README 补第二记分牌 + 改进点回流;
- 全程纯评测侧,默认路径与二进制零变化。
