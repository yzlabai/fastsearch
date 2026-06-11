# 2026-06-11 · 依赖批准落地:EML / 图片即文档 / 编码探测 / fuzz 目标(G1b+G7)

## TL;DR

用户一次批准四个依赖(mail-parser/zune-png/encoding_rs+chardetng/cargo-fuzz),当场全部落地:

1. **docparse-eml**(mail-parser 0.9):Subject→标题、From/To/Date 元数据行、首个文本体→段落、附件列举;MIME/quoted-printable/base64/RFC2047/字符集全由库承担,后端只做布局映射。docling 真实样例 2 份全过(含 HTML-only 邮件自动转文本、附件列举)。
2. **docparse-img**(zune-png + 已有 zune-jpeg):PNG/JPEG 单页文档——全幅 ImageChunk(1px=1pt 坐标精确),JPEG **原字节零转码**直通(同 PDF 扫描页策略),PNG 解到 Gray8/Rgb8(去 alpha)。`scanned_no_text` 质量路由 + `--ocr` **整条 N3 管线免费复用**——这是我方独有优势,Docling 同能力要拉整个模型栈。
3. **`core::textio`**(chardetng + encoding_rs):UTF-8 快路径原样,非 UTF-8 探测解码(损毁序列出 U+FFFD,可见不静默);csv/srt/tex 三个文本后端接入。G7 压测发现的 Shift-JIS CSV 边界即修——"因编码拒收真实内容"等于换名的数据丢失。
4. **fuzz/**(cargo-fuzz 工作区,4 个覆盖引导目标):pdf_parse(全管线,新增 `PdfParser::parse_bytes` 字节入口——REST 上传以后也能免临时文件)、eml_parse、img_parse、text_formats(解码+SRT/TeX/CSV 串)。

格式数 9 → **11**;压测语料 707 份(+eml/png/jpg)全零 panic;116 单测、clippy 0。

## 验收亮点:扫描件往返

`chinese_scan.pdf --image-dir` 抽出嵌入 PNG → `docparse p1-1.png --ocr` → 中文全文逐行正确("中国公司年度报告/第一章:公司概况/…")——图片后端与 N3 OCR 管线的拼接零额外代码,质量与 PDF 路径等同。

## 边界(诚实标注)

- EML:嵌套 message/rfc822 不下钻;附件内容不解析(未来增量可按扩展名转投注册表);
- 图片:TIFF、16-bit PNG、CMYK JPEG 之外的奇异布局干净报错;透明通道直接丢弃(不与白底合成——透明扫描件不是真实场景);
- 编码探测只接了纯文本后端(csv/srt/tex);HTML 的 charset meta、DOCX/XLSX 的 zip 内编码各有自己的处理层,不混;
- fuzz 目标已就位并纳入构建,**持续跑批候 nightly 环境**(本机 rustup 安装进行中;24h 长跑是 G7 加强项验收,另行排期)。
