# 2026-06-11 · LaTeX 源码后端(G1b)+ synth 列表语义通道

## TL;DR

新 crate `docparse-tex`(零依赖、行导向、无 TeX 引擎):article 常用子集——`\title/\author/\maketitle`、三级 section、abstract、itemize/enumerate、`tabular`→真 Table、figure/table 取 `\caption`、数学环境**原样保留**(源格式下原始 TeX 数学就是最忠实文本)、注释/转义/跨行声明配平。**7 份真实 arXiv 源码全过**(含 OTSL 2305.03393、Attention 1706.03762),标题→作者→摘要→章节→图注结构干净;格式数 8→9。顺带给 synth 布局开了**列表语义通道**(`PageBuilder::list_item` 打 `LI` 标签)。109 测试、clippy 0、PDF 记分牌/三件套零变化。

## 关键决策

- **解析的是源码不是排版结果**:不展开宏、不追 `\input`(一文件一文档,也是安全边界)、未知环境正文当散文读;边界写进模块 doc——这是"常用子集",不是 TeX 实现。
- **数学原样保留**:`$..$` 与 `equation/align` 环境的内容不清洗——对 RAG/agent,源文件的 TeX 数学比任何有损转译都忠实。
- **声明命令跨行配平**:多作者 `\author{...}` 常跨多行,单行跳过会让续行以散文泄漏(OTSL 论文实测撞上)——`brace_balance` 跟踪未闭花括号,声明吃到配平为止。
- `\cite`/`\ref`/`\label`/`\footnote`/`\orcidID`/`\thanks` 丢弃(余 "in Figure , we" 类小疤,记录在案);`\and`→逗号;`\keywords`→"Keywords: ..." 段落。

## synth 列表语义通道(横向修复)

e2e 时发现 enumerate 的 "1. First item in ordered list" 被核心层判成**编号标题**(`is_heading_text` 的数字+大写模式),而 "Nested Section" 反被标题连段降级吞掉。几何规则本质上无法区分 "1. Introduction"(编号标题)与 "1. 列表项"——**但合成后端知道语义**。正路是已有的标签否决通道:`PageBuilder` 新增 `list_item()`(chunk 打 `tag: "LI"`,经 `is_nonheading_tag` 否决标题、`tag_list` 确立列表),tex 后端的 `\item` 走它。修后列表/标题全部各归各位。md/docx 后端如有同类形态可同法迁移(待查,未扩散本次范围)。

## 验证

- 单测 ×3(标题页/列表/行内清洗;tabular+caption;注释/数学/层级);
- 7 份真实 arXiv main.tex + 2 份合成示例全部 ok;OTSL 论文 4 页 71 文本块 2 表;
- 压测语料表加入 tex:24 份全 ok、变异样本仍零 panic;
- PDF 路径不经 synth:三件套与记分牌(NID 0.792/MHS 0.685/TEDS 0.419)逐字不变。

## 已知边界

- `\input`/`\include` 不展开:多文件论文只解析主文件可见部分;
- 自定义宏(`\newcommand`)不展开:展开结果以原命令名留在文本或被未知命令规则跳过;
- 行内未知命令保留原样(数学安全的默认);`\\caption` 仅识别行首形态。
