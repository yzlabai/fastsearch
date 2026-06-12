# docparse-rs · 项目状态 / 记分牌 / 经验

> 当前进度、记分牌、待办与跨阶段经验教训的**单一真源**。CLAUDE.md §8 只留指针指向本文件。
> 战略见 [roadmap.md](roadmap.md)；执行里程碑历史见 [plans/](plans/) 下各计划 + [devlogs/](devlogs/)；
> **接入 Agent/RAG** 见 [agent-integration.md](agent-integration.md)（CLI/MCP/REST/库 + chunk schema + 引用/增强模式）。

## 1. 完成里程碑

**全部完成**（截至 2026-06-11，见 devlogs/ 双部会话总结）：M1–M7、N1–N5、Phase 4 可自主项：

- **G1** 格式 3→12（含 AsciiDoc）；
- **G2/G4/G8a**；
- **G3-R**：`--table-model` 内嵌 UniRec-0.1B×tract 0.23 重抽表结构，合并格语义端到端正确；
- **G6**：Python 客户端 + LangChain 验收；
- **G7**：压测 1847 输入 + fuzz ~1020 万次零 panic；
- **G8b/G8c**：`--vlm-describe/--vlm-tables` mock 验收、`--formula-model` 公式→LaTeX、`--transcribe-model` 整页转写（中英域；韩文域外被退化守卫安全拦截）、MCP/REST 全增强透传；
- **G9** 全部（G9d TEDS 验收门过）；
- IR 0.7.0（Cell span 语义 + 图片 base64 内嵌）。

**Phase 5（H1–H7）** 收官（2026-06-12）：CCITT 扫描解码、cls 旋转、读序异常分/路由收口、双栏左列重排、APTED 尺子、隐藏文本盲区（架构边界=不做）、打磨篮子。见 [plans/hardening-iteration.md](plans/hardening-iteration.md)。

**Phase 6（OmniDocBench 提分，2026-06-12，主负结论）**：逐一证伪所有便宜旋钮——学术表 0.52 是 **UniRec-0.1B 固定 960×1408 输入在大/宽/密表上的真实天花板**（行列诊断:主因大表 UNDER-segment + 超宽表 NO-PRED；**看图核对**把 #4/#16 初判的"口径反噬"推翻=实为模型 over-split）；落地 B1（e2e 表评测 strip_math 对称）+ B2（UniRec EOS 门控失控退化抢救，零回归）；B3 area 下采样 / 抬 max_tokens（0.517→0.466）/ 抬分辨率 1280×1792（净≈0 但救饥饿表=扭曲合身表 + 3× 慢破速度门）实测 no-op/反降/破门；真杠杆=更大/多分辨率表模型 / `--vlm-tables` 服务（均在内嵌+保速域外）。见 [plans/omnidocbench-score-lift.md](plans/omnidocbench-score-lift.md)。

## 2. 记分牌（两套互补，详见 [testresults/2026-06-12-omnidocbench.md](testresults/2026-06-12-omnidocbench.md)）

① **OmniDocBench**（人工真值，模型路径，第一参考）——文本/公式（UniRec）各 ~0.87（论文子集近论文级）、表结构 TEDS_X 0.810（median 0.895，80 表）、套官方公式 Overall ≈75（对标 OpenDoc-0.1B 90.67 / Docling ~80–85 / Marker 78.44）；**短板=学术难表**（端到端 0.52，模型天花板）+ 轻量 `--ocr` mobile（0.42–0.44，用 `--transcribe-model` 提质）。

② **一致度**（born-digital，vs ODL/Docling，压扁口径会反噬合并格）——NID 0.792/0.822、MHS 0.685/0.643、TEDS_X 0.477/0.526。

## 3. 待办

- **候外部输入**：PyPI/crates.io/MCP-registry 发布（账号，已暂缓）、arXiv 千份压测与 fuzz 24h（资源/排期）；
- **候设计/按需**：行内公式、G3b 确定性 span 推断、JATS/METS-ALTO、RTL（韩文 CJK 缺口属 VLM 服务域）、Markdown-span 输出；难表→`--vlm-tables` 真实服务实测；
- （OCR 档已定案=`--transcribe-model`；AsciiDoc 已落地。）
- 详见 [plans/closing-docling-gaps.md](plans/closing-docling-gaps.md)。

## 4. 跨阶段经验教训

1. **记分牌大跳几乎全是评测/输出管线 bug**——分数可疑先怀疑管线；
2. **参照系口径会反噬更忠实的输出**（span vs 压扁），产品价值用语义样例验收；
3. **依赖版本本身可以是性能特性**（tract 0.21→0.23 = 17×）；
4. **管道退出码会掩盖失败**；
5. **新格式 e2e 是共享层的免费测试矩阵**；
6. **提分前先验"瓶颈是模型还是尺子"**——便宜旋钮（max_tokens/重采样/分辨率）对模型天花板无效，抬 token 上限反让难表退化更久；
7. **口径判断必须看图坐实**（#4/#16 初判"口径反噬"裁图后推翻=实为模型 over-split）；
8. **"显然该有用"的杠杆也要量**——分辨率确是真杠杆（饥饿表 0→0.97）但脆模型对它净≈0 且破速度门，**真杠杆≠能用**。
