# 2026-06-11 · 会话总结:G9 收官后的八连推 —— 格式 11、生态接入、压测+fuzz 全绿

## TL;DR

昨日 G9d 收官(TEDS 0.419)之后,本日连续落地 **8 个推送、6 个里程碑**,Phase 4 的可自主推进项全部清空:

| 提交 | 里程碑 | 一句话 |
|---|---|---|
| e35f930 | 图片导出 `--image-dir` | ODL external 模式平齐;JPEG 直通/位图 PNG,JSON `file` + Markdown `![]()` |
| 93d378c | SRT/WebVTT 字幕(G1b) | 每 cue 一段带时间戳;**顺带修 synth 段距 bug**(全部合成后端段落被错并) |
| eb4b0e4 | G7 压测 | 1847 输入(647 清洁+1200 变异)**零 panic**,验收门过 |
| 0602070 | `--vlm-tables`(G8b) | VLM 重抽已检出表结构,mock 全链路过;失败保底确定性网格;IR 增 `Table.source` |
| 1537e2d | LaTeX 后端(G1b) | 7 份真实 arXiv 源码全过;**顺带开 synth 列表语义通道**(`LI` 标签防 "1. 项" 误判标题) |
| abf3a42 | G6 Python 客户端 + loader | 零依赖双传输;LangChain **五行验收实测过**(真 langchain-core,PDF→Documents 带 page+bbox) |
| b900b6d | 依赖批次(用户一次批四个) | EML(mail-parser)/ 图片即文档(zune-png,**扫描件往返验收**)/ 编码探测(Shift-JIS CSV 修复)/ fuzz 四目标 |
| f68ca20 | fuzz 烟雾 | nightly+ASan,四目标合计 **~1020 万次执行零崩溃**(pdf 全管线 cov 9167) |

体量变化:格式数 **3→11**(PDF/DOCX/HTML/XLSX/PPTX/MD/CSV/SRT·VTT/LaTeX/EML/PNG·JPEG),**16 个 crate**,116 单测,clippy 0;`clients/python` 新增生态面。记分牌全程零回归(vs ODL 0.792/0.685/0.419;vs Docling 0.822/0.643/0.474)。

## 模式与经验(本日新增)

1. **横向 bug 的两次"顺带捕获"都来自新后端 e2e**:字幕暴露 synth 段距(影响 docx/html/md 多日)、LaTeX 暴露列表/标题几何歧义(语义通道正解)。新格式不只是广度——它是对共享层的免费测试矩阵。
2. **管道退出码再次掩盖失败**:fuzz "EXIT=0" 实为 tail 的退出码,构建失败被吞;前台重跑才暴露。`cmd | tail; echo $?` 不可信,要 `PIPESTATUS`/前台验证。
3. **rustup 空壳工具链**:CDN TLS 连续中断后,rustup 组件登记"已安装"但文件缺失(`Missing manifest`/`can't find crate for core`),`component remove + add` 强制落盘才修复——环境损坏时别信元数据,验文件。
4. **"因编码拒收真实内容"等于换名的数据丢失**:textio 探测解码(U+FFFD 可见不静默)是对"不静默吞数据"红线的正向延伸。
5. **复用优先**:图片即文档零新管线(全幅 ImageChunk 搭 N3 OCR 路由);`--vlm-tables` 零新协议(G8b 栈);Python 客户端零依赖(subprocess+urllib)。

## 剩余池(全部候外部输入)

- **发布**:PyPI(docparse-client)/ crates.io / MCP registry——候账号与署名决策;
- **真实服务验收**:`--vlm-describe`/`--vlm-tables` 对 Ollama(qwen2.5-vl 级)实测回填;
- **G7 加强**:fuzz 24h 长跑(排期)、arXiv 千份原版口径(网络/存储);
- **按需立项**:AsciiDoc、JATS/METS-ALTO、RTL(G5)、MCP/REST 新能力透传(--layout/--vlm-*/图片 base64 模式)。

---

## 第二部分(同日下午—晚):VLM 调研 → UniRec 当日全链落地(九个提交)

上半场收完 Phase 4 可自主项后,用户两个问题驱动了下半场的完整弧线:

**"哪些地方需要 Ollama 服务驱动?"** → 调研文档([vlm-service-driven-capabilities](../refer/vlm-service-driven-capabilities.md)):七处,2 已实现候验收、5 规划——其中表结构/公式/整页转写三项当时只能走 7B VLM 服务。

**"考虑用 OpenOCR 0.1B?"** → 当天走完 调研→双 spike→立项→三连落地→服务透传→IR 收尾:

| 提交 | 内容 |
|---|---|
| 2adcda4/40ac1c6/5843b59 | VLM 服务调研 + OpenOCR 评估 + **spike 双门全过**(质量:pg9 合并格完美 HTML;速度:tract 0.23 实测 169 tok/s ≈2.5s/表,0.21 仅 10 tok/s——版本代差即性能特性) |
| f23a4bc | **R1 · tract 0.21→0.23**(API 迁移,OCR/YOLO/三件套/双记分牌回归全过) |
| 177c584 | **R2+R3 · `--table-model`**:UniRec 推理管线(Rust 宿主驱动自回归+KV-cache,绕开杀死 SLANet 的 ONNX Loop)+ HTML 子集解析(rowspan 悬挂网格展开)。pg9 端到端 10×8 语义全对,3.4s 纯 Rust。**诚实记录**:flag-on 一致度 TEDS 反降(两个参照系都压扁子行,LaTeX 源 \multirow 证实模型才对)——定位同 --layout,产品增强不进记分牌 |
| 9c4967e | **`--formula-model`**(G8c 收口):YOLO formula 区 + UniRec 出 LaTeX。验收样例:基线 "2a + 8 = 12"(上标漂移)→ `\[a^{2}+8=12\]` ✓ |
| e87bfe4 | **MCP/REST 全增强透传**(G8b 余项):EnhanceState/Opts 两面共用,UniRec 服务级懒加载一次;两面活进程 e2e 双过 |
| f8705f7 | **IR 0.7.0**:`Cell.row_span/col_span/merged`(平铺+标注,默认输出逐字节兼容)+ 图片 base64 内嵌三面(`--image-embed`/`images=embedded`),补齐 ODL image_output 三态 |

**战略结果**:原计划要 Ollama/vLLM 才能兜的表结构与公式两类语义,现在是**进程内纯 Rust**(0.1B 模型,~700MB 外置文件,零服务依赖)——身份约束(单二进制+可选模型文件)完整保留,VLM 服务域收窄为图片描述/页型判官/整页转写。

**新增经验**:
- **依赖版本本身可以是性能特性**(tract 0.21→0.23 = 17× 解码提速)——慢先查内核代差;
- **参照系口径会反噬更忠实的输出**(span 结构 vs 压扁网格)——一致度记分牌测的是一致度,产品价值用语义样例验收;
- 用户的一个链接到产品能力的当日闭环,靠的是 spike 门控把"要不要做"变成数字。

---

## 第三部分(同日深夜):B 列表收尾 + README 改版

- **`--transcribe-model` 整页转写**(f27a095,[devlog](2026-06-11-transcribe-model.md)):域内(中英)是质量升级(标点优于 PP-OCR,B5"OCR 档"由此定案);域外(韩文——评测 CJK 缺口恰全是韩文)暴露 AR 幻觉复读,新增**退化守卫**(周期重复检测)后旗标域外安全无害化,守卫同享表/公式路径。⚠️ 韩文缺口明确归 VLM 服务域。
- **AsciiDoc 后端**(docparse-adoc,零依赖):格式数 **12**、crate 数 17;
- **README 双语改版**:新增"与同类产品对比"(docparse-rs vs Docling vs OpenDataLoader-PDF vs MarkItDown,12 维度,对方占优处照写),亮点/用法/架构表全更新;
- 显式缓办(理由进 plan):行内公式(无区域信号)、G3b span 几何推断(模型路径已通)、JATS/METS-ALTO(候真实需求)、RTL/韩文(VLM 域)。
