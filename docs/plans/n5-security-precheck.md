# N5 · 安全预检(模块 9):隐藏文本过滤 → 资源防护

> 承接 [next-iteration.md §N5](next-iteration.md);定位:agent/RAG 接入面的治理底线。N2 服务化已上线,本里程碑优先级随之上升。
>
> 对照:ODL 的隐藏文本过滤(其 README 主打的 prompt-injection 防护);算法独立实现。

## 1. 威胁模型(为什么)

- **隐藏文本 → prompt injection**:PDF 可携带人眼不可见、但文本抽取器照抽的内容(渲染模式 Tr 3、超小字号、页外坐标、同色文字)。下游 agent 把抽取结果当可信上下文时,这是直接注入面。
- **资源耗尽**:恶意构造的 DOCX(zip bomb)/超深对象/超大页可拖死解析服务——CLI 时代是用户自伤,REST/MCP 时代是 DoS 面。

## 2. 分步

### N5a · 隐藏文本检测与过滤(本次)

- 解释器(`interpreter.rs`)跟踪 `Tr` 渲染模式;命中以下任一 → `TextChunk.hidden = true`:
  1. **Tr 3 / Tr 7**(不描边不填充 / 仅入剪裁路径)——经典隐藏 OCR 层/注入载体;
  2. **页外**:bbox 完全落在页面媒体框之外;
  3. **超小字号**:有效字高 < 1pt(正常下标 5–7pt,1pt 远低于可读阈值)。
- **过滤策略(不静默吞)**:`Page::text_chunks()` 过滤 hidden → markdown/text/chunks/质量评分全链路自动排除;**IR JSON 保留** hidden chunk 并带 `"hidden": true` 标注;`quality` 报告加 `hidden_chunks` 计数 + `HiddenTextPresent` flag——下游能审计被滤内容。
- IR:`TextChunk.hidden`(serde default,向后兼容);`SCHEMA_VERSION` 0.2.0→0.3.0(加法变更)。
- **显式不做(TODO 标注)**:同色文字检测(需跟踪填充色与背景推断,误报险高)、被图片覆盖的文字(需 z-order)。
- 验收:单测覆盖三类隐藏 + 可见文本不受伤;含 `3 Tr` 的合成内容流端到端(markdown 无、JSON 有标注、quality 有计数);三件套+评测记分牌零回归。

### N5b · 资源防护(下一步)

- DOCX zip bomb:解压字节上限(docx-rs 之前预检 zip 条目声明尺寸);PDF 对象深度/页数上限早停;REST 已有 256MB body 限制。
- 验收:构造样例被拒,产生可追踪错误码,不 panic 不挂起。

### N5c · 复杂度画像 ✅(2026-06-10)

`quality::PageProfile`:页级 kind(digital/scanned/mixed/empty,按可见文本 × 页面级图覆盖 ≥0.5 判定)+ 信号(text_chars/image_count/image_coverage/tables/enhanced_chunks)。CLI `--profile`、MCP `get_chunks` 信封新增 `profile` 字段。验收:chinese_scan → scanned(覆盖 1.0)、1901 → 15 页 digital(小图 0.006 不误判)。旋转页信号留 TODO(需版面层贯通)。

## 3. 边界

- 检测**确定性、零模型**;阈值近似必须标注(CLAUDE.md §4)。
- 过滤默认开(安全默认);如需保留隐藏文本(取证场景)走 IR JSON,不加 CLI 开关(YAGNI)。
