# 2026-06-11 · `--formula-model`:公式→LaTeX(G8c 主路径,UniRec 管线复用)

## TL;DR

G3-R 落地当天顺手收掉公式项:`docparse doc.pdf --formula-model models/unirec`——已内嵌的 DocLayout-YOLO 检出 `isolate_formula` 区域(class 8,G2 时就预留了这层)→ 渲染裁剪 → UniRec 出 LaTeX → **替换区域内的字形汤**。验收样例(code_and_formula.pdf)一目了然:

| | 输出 |
|---|---|
| 确定性基线 | `2a + 8 = 12` ——a² 的上标 **2 漂到了式首**(字形按绘制序到达,上下标脱钩,RAG 最毒的内容) |
| `--formula-model` | **`\[a^{2}+8=12\]`** ✓ |

约 70 行编排 + 纯函数拆分(全部复用 G3-R 栈:crop_region 通用化加 scale 参数、UniRec.recognize 原样)。124 测试(+2)、clippy 0、默认路径记分牌逐字不变。PP-FormulaNet spike 不再需要——G8c 的公式项由此收口。

## 设计要点

- **替换是增强旗标的契约**(同 `--table-model` 替换网格行):区域内 chunk 中心点判定,整批移除后注入一个 LaTeX chunk(`tag: "Formula"`——核心层标签否决通道顺带防其误判标题;`source: "formula:unirec-0.1b"`;confidence 0.8);
- **LaTeX 防劣化门** `usable_latex`:空/超长/含 `<table`(模型认错内容)/纯散文(无 TeX 命令与数学记号)一律拒绝,原文保留;
- `apply_formula` 是纯函数,单测覆盖(区域内替换、区域外保留、标签/溯源戳);
- 成本:YOLO 每页一跑(~2.4s)+ 每公式一次解码(数十 token,亚秒)——重在版面检测,故独立旗标而非默认。

## 边界

- 只处理 **display 公式**(isolate_formula 区域);行内公式(段落中的 $x^2$)版面模型不单独成区,仍是字形汤——待远期(行内检测要另一套信号);
- 公式区域判定依赖 YOLO 检出质量(SCORE_MIN 0.35);漏检的公式保持原状,不会变差;
- `\[...\]` 定界符由模型输出原样保留——下游 Markdown 渲染器可直接吃。

## 系列小结(2026-06-11 的 UniRec 三连)

调研(上午)→ spike 双门(中午)→ `--table-model`(下午)→ `--formula-model`(顺手)——同一个 0.1B 模型、同一条 tract 管线,收掉了原计划要 Ollama/vLLM 才能兜的两类语义。剩余 UniRec 顺带项:高质量 OCR 档(rec 替代,det 仍需 PP-OCR,形态待设计)、rowspan/colspan 语义入 IR(远期)。
