# 2026-06-11 · SRT/WebVTT 字幕后端(G1b 首项)+ synth 段距 bug 修复

## TL;DR

新 crate `docparse-srt`(零依赖手写):`.srt`/`.vtt` 每条 cue 解析成一段 `[hh:mm:ss] 文本`,经合成布局汇入同一 IR——格式数 7→8。顺带发现并修复 **synth 布局段距 bug**:所有合成坐标后端(DOCX/HTML/MD/XLSX/PPTX)的相邻段落都被 1.8em 合并门错并成一块,补 0.6em 段距后文本/Markdown 输出恢复一段一块。103 测试、clippy 0、PDF 路径零影响。

## 设计取舍

- **时间戳留在正文**(`[00:00:01] 你好，世界`):字幕的可引用性就在时间点(跳转到媒体时刻),丢掉时间戳的字幕抽取对 RAG 没意义;只保开始时刻、去毫秒,够定位且不啰嗦。
- **两格式一个 crate**:SRT 与 WebVTT 共享"空行分块 + `-->` 时间行 + 文本行"骨架,差异只在头部(`WEBVTT`)与块类型(`NOTE`/`STYLE`/`REGION` 跳过)。
- **VTT 行内标签**:`<i>`/`<b>`/`<c.class>` 剥除;`<v 说话人>` 转 `说话人: ` 前缀(对话字幕的说话人是真实内容);未闭合 `<` 原样保留不吞。
- 时间戳容错:`hh:mm:ss,mmm`(SRT 逗号)/`mm:ss.mmm`(VTT 可省小时,补零对齐);解析不动的原样透传,不丢 cue 文本。

## synth 段距 bug(意外捕获,影响面大于本体)

e2e 验证时发现两条 cue 被并成一段;追到 `core/layout.rs::group_blocks` 的续段条件 `gap ≤ 1.8em`,而 `synth.rs::paragraph` 只推进 1.4em 行高——**合成后端的每个"段落"chunk 本就是完整逻辑块,却被当成上一段的换行续行**。用 `para one.\n\npara two.` 的 Markdown 一测,果然也并("para one. para two.")——DOCX/HTML/MD 的文本输出长期受此影响。修复:段后加 0.6em 段距(中心距 2.0em > 1.8em 门),`PARA_SPACING` 常量注释写明因果。

验证:MD 两段分开、DOCX(list_after_num_headers)逐项一行、HTML(formatting)逐段一块、字幕一 cue 一行;PDF 路径(真实坐标)不经 synth,三件套首行与记分牌不变。

## 边界

- `.srt` 假定 UTF-8(`read_to_string`);Latin-1 老字幕会读取失败报错而非乱码——TODO 按需加编码探测。
- VTT 的 cue 设置(`align:start` 等)与区域定位忽略;嵌套时间戳(卡拉 OK 模式)按普通标签剥除。
- G1b 余项:EML(候 `mail-parser` 依赖征询)、图片即文档(候 `zune-png` 征询)、AsciiDoc/LaTeX 子集、JATS/METS-ALTO。
