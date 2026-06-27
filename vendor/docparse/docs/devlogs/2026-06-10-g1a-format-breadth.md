# Devlog · G1a 格式广度:XLSX / PPTX / Markdown / CSV——格式数 3 → 7

> 日期:2026-06-10 · plan:[closing-docling-gaps.md §G1](../plans/closing-docling-gaps.md) · 新依赖(用户批准):calamine / quick-xml / zip / pulldown-cmark

## 交付(四个薄后端,全走 `DocumentParser` + `synth` 合成坐标)

| crate | 实现 | 验收 |
|---|---|---|
| **docparse-xlsx** | calamine 读 workbook;每 sheet = 一页(sheet 名作标题 + 表格);数字按显示值(整数去 .0);zip-bomb 预检复用 `limits` | 合成 xlsx(inlineStr+数值)→ 管道表格正确 |
| **docparse-pptx** | zip + quick-xml 流式解析 slideN.xml;`a:p`→段落、`p:ph type=title`→标题字号、`a:tbl`→IR 表;**每 slide 一页**(synth 新增 `page_break`);坏 slide 不 panic | 单测:双 slide(标题+正文 / 表格)结构全对 |
| **docparse-md** | pulldown-cmark(ENABLE_TABLES):标题分级字号/段落/列表/代码块/管道表 | 合成 md → `## Report`+段落+表格 |
| **docparse-csv** | **零依赖手写** RFC-4180 子集(引号/内嵌逗号/内嵌换行/转义引号)| 单测覆盖三类转义;`"Liu, Wei"` 端到端正确 |

CLI 注册表四行;`page_break` 为 synth 唯一核心改动。

## 终态

97 单测(+4)、clippy 零 warning、三件套零回归、记分牌不受影响(新格式不进 PDF 评测)。二进制 25.94MB(30MB 门内)。**格式数 3→7**(PDF/DOCX/HTML/XLSX/PPTX/MD/CSV)。

## 边界

XLSX:共享字符串表(t="s")未解(calamine 自动处理真实文件;合成样例用 inlineStr 验证);PPTX:演讲者备注/母版/图片未做;MD:内联格式(粗斜体)拍平为纯文本。G1b 长尾(邮件/字幕/图片即文档/AsciiDoc/LaTeX/XML 族)未动。
