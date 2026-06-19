# devlog · 截断 PDF 的清晰诊断(2026-06-19)

一句话:**排查"测试语料里 3 个 arxiv PDF 总解析失败"——查明根因是下载被截断的损坏文件(非解析器 bug),docparse 正确拒绝(失败隔离按设计工作),顺手把 lopdf 那句晦涩的 `invalid start value` 在截断场景下替换成清晰的"文件被截断/不完整"诊断;语料剔除坏文件后 1000 篇全 ok。**

## 缘起

[2026-06-18 会话](2026-06-18-cli-progress-and-batch.md)里搭的 1000 文件批量测试语料,批量报告稳定 `826 ok · 174 failed`。174 个失败全是 3 个 arxiv 源(`1706.03762`/`1810.04805`/`2005.14165`,即 Attention/BERT/GPT-3)被复制 ~58× 的实例,报错 `failed parsing cross reference table: invalid start value`。用户要求:查清,要么修复要么剔除。

## 排查

逐字节看这 3 个文件:

| 文件 | 大小 | startxref | %%EOF | 结论 |
|---|---|---|---|---|
| 1706.03762 | 766405 | ✗ | ✗ | **截断/损坏** |
| 1810.04805 | 436793 | ✗ | ✗ | **截断/损坏** |
| 2005.14165 | 278463 | ✗ | ✗ | **截断/损坏** |
| 1901.03003(对照) | 1536792 | ✓ | ✓ | 完整 |
| 2408.02509v1(对照) | 586552 | ✓ | ✓ | 完整 |

坏文件**末尾是二进制流中途切断**(`tail -c 12` 是乱码,不是 `...%%EOF`),整文件**搜不到 `startxref`/`xref`/`%%EOF`**。完整 PDF 必有 `startxref`+`%%EOF` trailer——三者全无 = 文件被截断。这 3 个是会话早期 curl 时下载不全(arxiv 当时已开始变慢),`%PDF` 头校验放它们过了(头对、体缺)。

**判读**:不是 docparse 的 bug——对截断 PDF **报错拒绝是正确行为**(不该硬产出垃圾);批量的失败隔离也正按设计工作(坏文件单独 ERROR、不中断整批)。问题只在:① lopdf 的 `invalid start value` 对"其实是截断"这一最常见成因毫无指向;② 语料里混进了坏文件。

## 做了什么

**① 清晰诊断(真修复)** [pdf/lib.rs](../../crates/docparse-pdf/src/lib.rs):`load_tolerant` 失败且 `repair_xref_keyword` 也不适用时,加判 `is_truncated(bytes)`——尾部 2KB 无 `%%EOF` 且全文无 `startxref` 即判截断,返回:

> `PDF appears truncated or incomplete — no trailer/%%EOF found (partial download or corrupt file); underlying error: …`

保留底层 lopdf 错误供深究。`is_truncated`/`contains` 为纯函数,单测 `truncation_detected_by_missing_trailer` 覆盖完整/仅 `%%EOF`/截断/空。**对完整 PDF 零影响**(只在错误路径触发);三件套回归不变。实测对 `1706.03762.pdf` 直接吐新诊断。

**② 剔除坏文件(语料)**:从源集删掉这 3 个截断文件,1000 文件语料用剩下 14 个完整源重建——实测 **`1000 ok · 0 failed · 15309 pages · 688 pages/s`**。

**③ 顺手硬化下载器**(`tmp/fetch-papers.sh`,本地/gitignored):校验从"只看 `%PDF` 头"加严到"还要尾部有 `%%EOF`",截断的下载会被丢弃重试,不再留坏文件。

## 验收

全工作区 **34 套件通过**(docparse-pdf 26,+1 截断单测),clippy 零 warning,fmt 净;清洁语料 1000 篇全 ok;截断文件给出清晰诊断。

## 教训

- **下载完整性别只看魔数头**:`%PDF` 头对 ≠ 文件完整;PDF 要校验尾部 `%%EOF`。已落到下载器 + docparse 诊断两处。
- **失败隔离这次帮了大忙**:坏文件没拖垮整批,且报告把它们单独标出来才让人一眼看到"全是这 3 个源"——批量设计的价值实证。
