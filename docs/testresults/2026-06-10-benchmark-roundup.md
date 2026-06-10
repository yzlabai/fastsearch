# 测评总览 · docparse-rs vs OpenDataLoader / Docling(2026-06-10,全功能落地后)

> 本文是十大模块全部闭合后的**综合测评**:质量记分牌(双参照)+ 差异化记分牌(实测)+ OCR 能力轴(N3 新增)+ 按 roadmap §2 四大战场的逐项判定。
> 单项明细:[ODL 对比](2026-06-09-odl-comparison.md) · [Docling 对比](2026-06-09-docling-comparison.md) · [差异化指标](2026-06-10-differentiation-metrics.md) · [N2 服务化](2026-06-10-n2-serving.md) · **平衡视角**(把 Docling 优势摆全):[客观对比](../refer/docling-objective-comparison.md)
> ⚠️ 口径先说清:质量分是**与参照系统的一致度**(agreement),不是对人工真值的准确率;TEDS 为近似实现;吞吐对比中 ODL/Docling 取其**公开宣称值**,非同机同台。

## 1. 质量记分牌(born-digital LTR,一致度)

| 同台 | 文档数 | NID 阅读顺序 | MHS 标题 | TEDS 表格 |
|---|---|---|---|---|
| vs **OpenDataLoader**(确定性同类) | 15(LTR 12) | **0.764** | **0.627** | 0.098(7 份含表) |
| vs **Docling**(神经管线) | 13(LTR 10) | **0.833** | **0.645** | 0.187(6 份含表) |

**分布比聚合更说明问题**:

- clean 文档已**结构同构**:`2305-pg9` 1.000、`code_and_formula` 0.999、`picture` 0.998、`multi_page` 0.984、`redp5110` 0.973、`2203` 0.945、`2305` 0.939——这一档与两家同水平;
- 聚合被两类拖低,且均已定性:**CJK 复杂版面**(`skipped_*` 0.12–0.22,韩文信息图 label-value 序,双方确定性都解不好)与 `amt`/`2206` 残余(0.66/0.75,参照自身亦有噪声);
- TEDS 低是**结构精度**(多级表头/合并单元格,神经域)而非检出——检出召回已 5/6+,`redp5110` 检出 5 表反超 ODL 的 4。

## 2. 差异化记分牌(实测,可重复:`scripts/metrics.sh`)

| 指标 | docparse-rs 实测 | 对照 | 判定 |
|---|---|---|---|
| 二进制体积 | **23.46 MB** 单文件(OCR+版面推理栈 + 按需渲染器) | Docling:Python 环境 + 数百 MB 模型下载;ODL:JVM | ✅ <30MB 门(2026-06-10 上调,见 roadmap §6) |
| 运行时依赖 | **0**(OCR 模型为可选外部文件 ~16MB) | Docling 需 Python/torch;ODL 需 JVM | ✅ |
| 解析延迟(预热) | **<10ms** | Docling 冷启动需加载模型(秒级) | ✅ |
| 吞吐(born-digital) | **700 页/s**(14 页文档,3 次中位) | ODL 宣称 0.02s/页≈50 页/s;Docling CPU 典型 ~1 页/s 量级(其报告) | **~14×/~700×**(宣称值口径) |
| 确定性 | **20/20** 逐字节一致;且 **CLI/MCP/REST 跨接口逐字节一致**(含 OCR 路径) | Docling 神经管线不保证 | ✅ 独有 |
| 引用可定位率 | **216/216 (100%)** chunk 带 page+bbox,`locate` 反查闭环 | Docling item 亦带 bbox provenance;我方增量在反查闭环与 100% 保证 | ✅ |

## 3. OCR 能力轴(N3 后新增的战场)

| | docparse-rs | ODL | Docling |
|---|---|---|---|
| 扫描件 OCR | ✅ **进程内纯 Rust**(PP-OCRv4 × tract),零 Python/C++/子进程 | ❌ 核心无;hybrid 模式 HTTP 外送 docling-serve | ✅ 七引擎(默认 EasyOCR,Python) |
| 中文质量 | PP-OCRv4(中文事实标准模型);`chinese_scan` **14/14 行全对** | 取决于后端 | EasyOCR/同级 |
| 成本模型 | **按页路由**:数字页零模型零加载;整页扫描 1.28s(含模型加载) | 后端往返 | 全管线过模型 |
| provenance | 每行 `source: "ocr:ppocr-v4"` + confidence≤0.99 + bbox | — | cell 级 `from_ocr` |

> 战略含义:roadmap §2 原判定"扫描 OCR 短期不打"已被 N3 改写——**中文扫描件现在可打**,且部署形态(单二进制+16MB 模型文件)是三者中最轻的。神经表格/公式/手写仍外接不打。

## 4. 四大战场判定(roadmap §2 复盘)

| 战场 | 原定位 | 现状判定 |
|---|---|---|
| 数字原生文档 | 要赢 | **赢**:部署/速度/确定性/引用四轴全部实测领先;clean 文档质量同构 |
| 结构理解 | 要持平 | **检出持平,精度不持平**:表格检出召回达 ODL 量级;多级表头/合并单元格属神经域(诚实边界) |
| 多格式广度 | 要持平 | **部分**:PDF/DOCX/HTML ✅;PPTX/XLSX 未做(Docling 全覆盖) |
| 扫描 OCR/神经长尾 | 不打 | **中文扫描已可打**(N3);公式/手写/复杂神经表格仍外接 |

## 5. 还能补测什么(下一步测评建议)

1. **同机吞吐同台**:装 Docling(Python)与重建 ODL jar 各跑 15 份样例计时——把 §2 的"宣称值口径"升级为实测(预计结论方向不变,量级或收窄);
2. **人工真值集**:外采/标注 born-digital 标注(阅读顺序/表结构/标题),把一致度升级为准确率——当前最大的方法学缺口;
3. **OCR 质量面**:多样本扫描集(不同 DPI/倾斜/传真类),量化 PP-OCRv4-via-tract 与 RapidOCR 官方(onnxruntime)的字符准确率差异(理论应一致,验证数值等价性);
4. **TEDS 精确化**:换精确 APTED 实现,当前为结构代理(数字偏保守)。
