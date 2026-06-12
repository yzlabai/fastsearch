# 测评 · OmniDocBench(人工真值,第二记分牌)

> 2026-06-12 · 计划 [omnidocbench-benchmark.md](../plans/omnidocbench-benchmark.md)
> 脚本(纯评测侧,不进二进制):[`scripts/eval/omnidocbench/`](../../scripts/eval/omnidocbench/) ·
> example [`odb_recognize`](../../crates/docparse-ocr/examples/odb_recognize.rs)

## 1. 为什么换这个 benchmark

既有记分牌(vs ODL/Docling)测的是**一致度**——和别的系统输出比,且表格真值用**压扁口径**(多级表头子行并一行),导致内嵌 opendoc/UniRec 模型给出真实 span 结构反而被扣分("一致度 ≠ 准确度")。

**OmniDocBench**(CVPR 2025,opendatalab)给**人工标注**真值:表格 **HTML+LaTeX(真实 span 口径)**、公式 LaTeX、文本/阅读顺序;1651 页、中英为主、10 类文档。它也正是 **OpenDoc-0.1B/UniRec 自己报分(90.57%)** 的 benchmark——接它既公正评测模型,又能对标论文。

## 2. 方法(自给自足,不搭官方 Python 环境)

只用 OmniDocBench 的**数据**(页图 + HTML/LaTeX/text 人工真值),用**项目自有指标**评分:
- **表 TEDS**:复用 H5 的精确 Zhang-Shasha 树编辑距离(`score.py::teds_x`),HTML→span 感知格树,数学定界符归一化;
- **公式**:归一化 LaTeX 字符级相似度(CDM 的轻量代理,CDM 需渲染);
- **文本**:字符级相似度(⚠️ 中文无空格,**必须字符级**——词级 `split()` 会把整页中文压成一个 token 假性归零);
- 与论文 90.57%(端到端综合)**量级参照、不逐位可比**(自实现指标 + 子集)。

页图实为 `.png` 扩展名的 progressive JPEG;模型路径只吃 PDF,故按需把页图包成单页 image-PDF 再跑管线。

## 3. 结果 · 三维度

| 维度 | 路径 | 子集 | 分数 |
|---|---|---|---|
| **表结构(单模块)** | UniRec 模型(GT 表区直喂) | 80 表 | **TEDS_X 0.810**(median **0.895**) |
| **表结构(端到端)** | `--layout` 检表区 → `--table-model` 重抽 | 8 单表页 | **TEDS_X 0.827** |
| **公式** | UniRec 模型(GT 公式区直喂)→ LaTeX | 30 式 | **LaTeX-sim 0.708** |
| **正文文本** | PP-OCRv4 **mobile** OCR + 版面重排 | 10 页 | **字符级 sim 0.423** |

表分布(80 表):满分 4、0.9–1.0 共 33、0.7–0.9 共 24、0.4–0.7 共 14、<0.4 失败 5——**中位数 0.895 即一半的表近乎完美**,均值被少数难表/AR 退化拉低。

## 3b. 按文档类型:学术论文子集

OmniDocBench 按 10 类文档分别报分;**学术论文**(`academic_literature`,215 页:表 123 / 公式 591 / 文本块 1375)是公式最密、版面最规整的一类。用 `OMNIDOC_DOCTYPE=academic_literature` 过滤跑(脚本支持环境变量):

| 维度 | 论文子集 | 全集对照 | 解读 |
|---|---|---|---|
| 公式(LaTeX-sim) | **0.874** | 0.708 | 论文公式规整标准,UniRec 强项 |
| 表(TEDS_X) | **0.517** | 0.810 | 论文学术表最难(多级表头/密集数字/含 LaTeX),全集被书报简单表抬高 |
| 文本 `--transcribe-model`(UniRec) | **0.630** | — | UniRec 整页转写 |
| 文本 `--ocr`(PP-OCRv4 mobile) | 0.440 | 0.423 | 轻量档,作对照 |

**文本两档的差距是重点**:`--ocr`(16MB mobile)0.44 vs `--transcribe-model`(UniRec)0.63——和 OpenDoc-0.1B 同源的 UniRec 文本路径显著更强,这才是对标 leaderboard 的公平基准。

## 3c. 官方端到端 Overall(参考基准)

OmniDocBench 官方 leaderboard 的端到端 **Overall**(0–100,越高越好)= `((1−text_edit)·100 + table_TEDS + formula_CDM)/3`:

| 系统 | 类型 | Overall |
|---|---|---|
| MinerU2.5-Pro | 专用 VLM | 95.75 |
| GLM-OCR / PaddleOCR-VL | 专用 VLM | 95.2 / 94.9 |
| **OpenDoc-0.1B**(=我们用的 UniRec) | 专用 VLM | **90.67** |
| GPT-4o | 通用 VLM | 86.59 |
| GOT-OCR | 专家视觉 | 86.52 |
| Docling | 管线工具 | ~80–85 |
| Marker | 管线工具 | 78.44 |

**我们的粗略量级**(论文子集,套官方公式;⚠️ **口径示意非官方端到端分**——分维度相似度代理 + 小子集 + 表/公式为单模块、文本为 transcribe 整页,**不可与上表逐位比**):
`(text 63.0 + table 51.7 + formula 87.4) / 3 ≈ **67**`。

诚实定位:**公式接近论文级(0.87),文本用 UniRec 也不弱(0.63),整体被论文难表(0.52)与管线损耗拉到 ~67 量级,低于 leaderboard 顶部(90+)**。差距来源清楚:① 我们 ≠ OpenDoc 完整系统(它 PP-DocLayoutV2 检测 + UniRec 全栈端到端;我们 DocLayout-YOLO + 分任务重抽 + 自写拼接);② 我们是 **born-digital 优先**,图像文档是补充域;③ 论文学术表是公认最难项。**要逼近 leaderboard 需要端到端 VLM 式管线**(可插拔域),非确定性核心的目标。

## 4. 决定性结论

**① 换尺子见真章(本次立项的核心)** — 同一个 UniRec 表识别,只换真值口径:

| 评测 | 真值口径 | 表 TEDS |
|---|---|---|
| vs Docling(旧记分牌) | 压扁口径 | 0.526(被反噬) |
| **OmniDocBench** | **span 口径(人工)** | **0.810** |

**0.526 是口径假象,0.810 才是真实表识别能力。** 模型没变,尺子对了,差距就显出来。

**② 一个真实产品改进** — 接入中发现范式不匹配:图像文档无矢量 ruling line,确定性表检测失效,`--table-model` 此前 `refined: 0`(模型被挡门外)。已打通 `--layout`(YOLO)检表区 → 生成占位 → `--table-model` 重抽(`seed_table_regions`,失败占位清理)。**图像/扫描文档现在端到端用得上表模型**;同表单模块 0.995 → 端到端 0.970,**检测层损耗仅 0.025**;born-digital 路径零回归。

**③ 完整能力画像(不回避弱项)** — 表强(0.81,模型,近论文级)、公式中上(0.71)、**文本中等(0.42)**。文本分被两件事拉住:① 我们的 OCR 是**轻量 PP-OCRv4 mobile**(16MB,定位"数字页零成本、扫描补充"),非重型图像 OCR;② OmniDocBench 全是图像文档(报纸/手写/试卷/多语种),正是我们**非主场**(born-digital 确定性快路径在此无文本可用)。这与产品定位一致:**born-digital 优先**;图像文本要更强是可插拔 OCR/VLM 的事,非确定性核心的债。

## 5. 边界与定位

- **第二记分牌**(图像文档 + 模型/OCR 路径 + 人工真值),与 vs-ODL/Docling 的 **born-digital 一致度记分牌互补、不取代**:那个测确定性快路径的结构,这个测模型/OCR 的视觉解析准确度;
- 自实现指标(TEDS 标准算法;公式/文本为相似度代理)+ 子集,与论文量级参照;
- 纯评测侧,`odb_recognize` 是 eval-only example、**不进二进制**,默认路径与产物零变化;评测数据不进 repo(`tmp/omnidocbench/`)。

## 6. 复现

```bash
mkdir -p tmp/omnidocbench
curl -sL https://huggingface.co/datasets/opendatalab/OmniDocBench/resolve/main/OmniDocBench.json \
  -o tmp/omnidocbench/OmniDocBench.json          # 真值(40MB);页图脚本按需 curl
cargo build --release -p docparse-ocr --example odb_recognize   # 评测工具
python3 scripts/eval/omnidocbench/table_eval.py 80      # 表 单模块 TEDS_X
python3 scripts/eval/omnidocbench/e2e_table_eval.py 8   # 表 端到端(检测+识别)
python3 scripts/eval/omnidocbench/formula_eval.py 30    # 公式 LaTeX 相似度
python3 scripts/eval/omnidocbench/e2e_text_eval.py 10   # 正文文本 字符级相似度
```

## 7. 下一步(候做)

- 全量 665 表 / 更大公式、文本子集(夜跑)拿权威均值;
- 官方综合分公式 `((1−text_edit)·100 + table_TEDS + formula_CDM)/3` + 真 CDM(渲染比对)+ 多表页 pred↔GT 匹配,和论文 90.57% 逐项对标;
- 失败例(难表/手写/多语种)回流:G3-R 行切分噪声、重型 OCR / VLM 服务(可插拔域)。
