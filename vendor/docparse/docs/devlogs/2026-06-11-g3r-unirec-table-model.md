# 2026-06-11 · G3-R 收官:`--table-model` 内嵌 UniRec-0.1B 表结构重抽(当天调研→spike→落地)

## TL;DR

用户上午提议评估 OpenOCR 0.1B,当天完成调研→双 spike→立项→落地:**`docparse doc.pdf --table-model models/unirec`** 用进程内 UniRec-0.1B(tract 0.23,纯 Rust)重抽已检出表的结构——**合并单元格/多级表头(rowspan/colspan)端到端正确**,pg9 验收样例 3.4s 出完美 HTML、展开成 10×8 网格。G3(初版死于 SLANet/TATR)正式复活收官。零服务依赖,单二进制+外部模型文件(~700MB)的身份不变。122 测试、clippy 0、默认路径记分牌逐字不变。

## 三步实施

**R1 · tract 0.21→0.23**(f23a4bc):这是 spike 的关键发现——0.23 的新矩阵内核把解码从 10 tok/s 拉到 **169 tok/s(17×)**,可用性由此成立。API 迁移两处(`TypedRunnableModel` 别名 Arc 化、`as_slice_mut/to_array_view`→`to_plain_array_view(_mut)`);回归门全过:chinese_scan OCR 逐字不变、YOLO 版面点火、三件套、双记分牌逐字不变。

**R2 · 推理管线**(`ocr/unirec.rs`):`find_file` 惯例加载 encoder/decoder/tokenizer 映射;预处理(≤960×1408 等比 /64 对齐、(x/255−0.5)/0.5,双线性缩放实测足够);encoder 一次输出固定 cross-K/V;**自回归循环在 Rust 宿主侧**(每步 1 token + 六层 KV-cache 张量传递)——这正是绕开杀死 SLANet 的 ONNX `Loop` 的形态;detokenize(字面词表 + `Ġ/Ċ` + OpenOCR 清理规则,无 regex 依赖)。

**R3 · 表任务接线**(`ocr/table_model.rs`):对已检出表渲染区域(hayro 3×)→ UniRec 出 HTML → **手写子集解析**(`<table>/<tr>/<td|th rowspan colspan>`,rowspan 用悬挂网格 pending 机制正确展开,值复制到所有跨位——与 eval/ODL 口径一致)→ 替换 `Table.rows`,`source: "table:unirec-0.1b"`;防劣化门(≥2×2、非全空)+ 失败保底确定性网格,纪律同 `--vlm-tables`。

## 验收与诚实记录

- **语义正确性 ✅**:pg9 输出 `# enc-layers`(rowspan=2)、`TEDs`(colspan=3)等全部跨格准确,数值逐字对;
- **默认路径零变化 ✅**:NID 0.792 / MHS 0.685 / TEDS 0.419 逐字不变(flag 不开模型字节不读);
- ⚠️ **flag-on 的一致度 TEDS 反降**(pg9 0.804→0.39、redp5110 0.859→0.812、2206 0.421→0.115):**ODL 与 Docling 真值都用压扁口径**——OTSL/HTML 子行被并为一行(`'OTSL HTML' / '0.965 0.969'`),而 UniRec 给出 LaTeX 源 `\multirow` 证实的**真实 10 行结构**。"一致度≠准确度"再添铁证。定位由此明确:**产品质量增强,手动 opt-in,不进一致度记分牌**——与 `--layout` 的"设计版面赢/学术降"同构。

## 经验

1. **用户的一个链接当天变成产品能力**:调研(WebFetch/论文/HF 文件清单)→ Python 质量 spike → tract 双版本速度 spike → 立项三步走——spike 门控让"要不要做"的争论变成数字;
2. **依赖版本本身可以是性能特性**:tract 0.21→0.23 一跳 17×,比任何应用层优化都大——慢的时候先怀疑内核代差,再动手术;
3. **参照系的口径会反噬更好的输出**:模型给出更忠实的结构反而掉分——记分牌只测一致度,产品价值要用语义样本验收;真正的出口是 rowspan/colspan 语义进 IR(远期项,现在有了数据来源)。

## 余项

- 公式→LaTeX(同模型同管线,G8c 主路径,~小项);
- UniRec 作高质量 OCR 档(`--ocr-models` 兼容形态待设计——det 阶段仍需 PP-OCR,UniRec 是 rec 替代);
- rowspan/colspan 语义入 IR + HTML 表输出保留 span(远期);
- 模型获取文档化(README models 节):`huggingface-cli download topdu/unirec_0_1b_onnx`。
