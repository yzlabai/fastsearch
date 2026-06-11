# 调研:OpenOCR 0.1B(UniRec/OpenDoc)能否承接表结构/公式/整页转写(2026-06-11)

> 用户提议:考虑 [Topdu/OpenOCR](https://github.com/Topdu/OpenOCR) 的 0.1B 模型。
> 结论先行:**高度对口,值得 spike**——它正打在我们三个"原计划只能靠 VLM 服务"的缺口上(表结构、公式→LaTeX、整页转写),且架构形态比 SLANet/TATR **tract-友好得多**(宿主驱动自回归,无 ONNX `Loop`)。关键不确定性只有一个:**CPU 推理速度**(官方数字全是 A800 GPU)。建议按老规矩 spike 门控,过了就把这三项从"VLM 服务驱动"改划"P4 内嵌"。

## 1. 它是什么

| 项 | 事实 |
|---|---|
| **UniRec-0.1B** | 统一文本+公式识别(OpenDoc 重制版**加入表格识别**):FocalNet 编码器(原生分辨率 ≤960×1408,768 维 /32 下采样)+ 6 层 Transformer 交叉注意力**自回归**解码器(768 hid/12 头),合计 ~0.1B 参数([论文 2512.21095](https://arxiv.org/html/2512.21095v1)) |
| **OpenDoc-0.1B** | 文档解析系统 = PP-DocLayoutV2(版面)+ UniRec-0.1B(逐区域识别) |
| 训练数据 | UniRec40M:4000 万样本(30M 英 + 10M 中;19M 纯文本/13M 公式/8M 混合;arXiv TeX/数字 PDF/手写) |
| 许可 | **Apache-2.0**(代码与权重,HF 页面确认)——与本项目同许可,无边界问题 |
| 权重 | HF `topdu/unirec-0.1b`:`model.pth` 536MB(fp32)+ tokenizer;**HF 仓库无现成 ONNX**,但仓库提供 ONNX 导出工具(2025-03 起),demo 引用过 "UniRec decoder ONNX" |
| 语言 | 中英(正对我们的主战场) |

## 2. 质量与速度声明(均为官方口径,GPU)

- OmniDocBench v1.5:OpenDoc-0.1B **90.57**——0.1B 参数打平/超过一票大模型文档解析器(对照:接入 PaddleOCR-VL 后 SOTA 0.113 编辑距离);
- 文本:全档位超 **PP-OCRv5**(即超过我们现役 v4/v5);公式:超 Pix2Tex **20.3%**;
- 速度:**0.37s/区域、6.2s/页(A800 GPU,动态图未加速)**,比 PaddleOCR-VL 快 5×;**无 CPU 数字**——这是我们的关键未知。

## 3. 对口性:正打在三个"原本只能 VLM"的缺口上

对照 [vlm-service-driven-capabilities.md](vlm-service-driven-capabilities.md) 的清单:

| 我方缺口 | 原路线 | OpenOCR 0.1B 的替代形态 |
|---|---|---|
| 表结构(合并格/多级表头;`--vlm-tables` 的本体) | 7B VLM 服务 | UniRec 表识别(结构 token 序列;**输出格式 OTSL/HTML 待 spike 确认**) |
| 公式→LaTeX(G8c) | VLM 服务(ONNX spike 此前无候选) | UniRec 核心能力,LaTeX 式 token 输出 |
| 整页转写(CJK 复杂版面) | Qwen2.5-VL 32B 级 | OpenDoc 全管线(版面+逐区域);90.57 OmniDocBench 是有竞争力的质量线 |
| OCR 升级(顺带) | — | 文本全档位超 PP-OCRv5,可作 `--ocr-models` 的高质量档 |
| 页型判官 | 小 VLM | **不对口**(它不是分类器;判官仍走 VLM 或画像特征) |

**战略意义**:若 spike 通过,三项从"服务驱动(要装 Ollama/vLLM)"改划"**P4 内嵌**(进程内 tract,单二进制+外部模型文件)"——更贴身份约束(确定性核心+可选模型文件,无服务依赖),部署故事完整保留。

## 4. tract 可行性:比 SLANet/TATR 乐观,但有三道坎

G3 的死因回顾:SLANet 死于 ONNX **`Loop` 算子**(tract 未实现);TATR 死于导出本身。UniRec 的形态不同:

- ✅ **自回归循环在宿主侧**(demo 即"Python 循环反复调 decoder ONNX")——我们用 Rust 循环驱动,完全绕开 `Loop`;encoder 一次 + decoder 逐 token,这是 tract 能跑的形态;
- ⚠️ **坎 1:自导出 ONNX**(HF 无现成):Python 一次性工序(同 v5-mobile TODO 的性质),repo 自带导出工具;需照 N3 经验做 dim 名消毒;
- ⚠️ **坎 2:FocalNet 编码器算子覆盖**未验(focal modulation 的 gather/roll 类算子是风险点);
- ⚠️ **坎 3(最大):CPU 速度**。0.1B 自回归解码,**没有 KV-cache 的 ONNX 每 token 全量重算**(O(n²)),CPU 上可能慢到不可用;带 KV-cache 导出则 tract 需要处理动态 cache 输入。粗估:int8 + KV-cache 是可用性的前提。表区域 ~100–500 token,可接受门槛建议定 **≤5s/表区域(CPU int8)**。

## 5. Spike 实测结果(2026-06-11):双门全过 ✅

> 当日完成 Spike①②(本机 Apple Silicon,CPU)。前提性利好:官方有**现成 ONNX 仓库**(HF `topdu/unirec_0_1b_onnx`:encoder 157MB + decoder 529MB + tokenizer 映射 9.4MB)——坎 1(自导出)消失;decoder 带 `past_key_*`/`present_key_*` **KV-cache 接口**——坎 3 的 O(n²) 风险消失。

### 质量门(Spike①,onnxruntime CPU)

- **2305-pg9 表区域(合并格样本)**:输出**完美 HTML 表,含 `rowspan="2"`/`colspan="3"`**,全部单元格数值逐字正确——这正是确定性 G9d 给不出(只能平铺 5×8)、原计划要 7B VLM 兜的语义。**表格输出格式 = HTML(确认)**,解析回 `Table` 直接可做(且 rowspan/colspan 语义可入 IR——远期项有了实现路径);
- **中文扫描**(chinese_scan 抽出 PNG):全文正确含段落结构,0.9s;
- ORT CPU 延迟:表 1.2s(316 token,~300 tok/s;**单线程 357 tok/s**——内核质量而非线程数)。

### tract 可行性门(Spike②,真实 ONNX 实测)

| 步骤 | tract 0.21(现役) | tract 0.23.1 |
|---|---|---|
| encoder parse/typecheck/**optimize** | ✅ 全过(2858→2115 节点) | ✅ |
| decoder parse/typecheck/**optimize**(符号维) | ✅ 全过(842→594 节点) | ✅ |
| **正确性**(贪心解码 token 序列) | ✅ 与 ORT 逐一相同 | ✅ 逐一相同 |
| encoder 运行(384×960) | 0.87s | 0.65s |
| 解码速度 | **10.1 tok/s**(优化版;declutter 仅 0.3) | **169.2 tok/s**(multithread-mm) |
| pg9 全表估算(316 token) | ~32s ❌ | **≈2.5s ✅(≤5s 门过)** |

算子全是标准件(FocalNet 无奇异算子;decoder 纯 Transformer)。0.21→0.23 的 17× 来自新 GEMV/矩阵内核——**纯 Rust 推理可行且达标,前提是 tract 升级到 0.23**。

### 结论与立项建议

1. **G3 复活立项 `--table-model`(P4 内嵌)**:UniRec-0.1B × tract 0.23,表区域→HTML(含 span)→ Table;同模型顺带公式→LaTeX 与文本识别(可作高质量 OCR 档);
2. **前置工序**:workspace tract 0.21→0.23 升级(PP-OCR/YOLO 回归三件套+记分牌必跑——版本跳两级,API 有变化:`to_array_view` 移除等,spike 已踩);
3. 模型外置 `models/unirec/`(~700MB,gitignored);加载沿 `find_file` 模式;
4. 整页转写仍建议走 OpenDoc 全管线评估(版面+逐区域,另行 spike 页级延迟);VLM 服务路线保留给图片描述/页型判官(见 [vlm-service-driven-capabilities.md](vlm-service-driven-capabilities.md) §3.5)。

## 来源

- [Topdu/OpenOCR](https://github.com/Topdu/OpenOCR)(README:模型清单/许可/ONNX 导出/基准声明)
- [opendoc.md](https://github.com/Topdu/OpenOCR/blob/main/docs/opendoc.md)(管线、CPU/GPU 推理入口、下载方式)
- [UniRec-0.1B 论文(arXiv 2512.21095)](https://arxiv.org/html/2512.21095v1)(架构/速度/基准/训练数据)
- [HF topdu/unirec-0.1b](https://huggingface.co/topdu/unirec-0.1b/tree/main)(权重文件清单/Apache-2.0)
