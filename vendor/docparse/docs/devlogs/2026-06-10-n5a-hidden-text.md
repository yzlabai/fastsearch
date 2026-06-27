# Devlog · N5a 隐藏文本过滤——agent 接入面的 prompt-injection 防线

> 日期:2026-06-10 · plan:[plans/n5-security-precheck.md](../plans/n5-security-precheck.md) · 对照 ODL 隐藏文本过滤,独立实现

## 做了什么

PDF 可携带人眼不可见但抽取器照抽的文本——下游 agent 把解析结果当可信上下文时这是直接注入面。N2 服务化上线后此风险变成对外暴露面,故 N5a 先行。

**检测**(`interpreter.rs`,确定性零模型):命中任一 → `TextChunk.hidden = true`:
1. 渲染模式 `Tr 3`(不画)/ `Tr 7`(仅剪裁)——新增 `Tr` 操作符跟踪(此前 TODO 注明忽略);
2. **页外**:bbox 完全在媒体框外;
3. **超小字号**:有效字高 < 1pt(`TINY_FONT_PT`,正常下标 5–7pt,近似已标注)。

**过滤策略(不静默吞)**:
- `Page::text_chunks()` 只回可见文本 → markdown/text/chunks/质量评分全链路自动排除;
- **IR JSON 保留**隐藏 chunk 并带 `"hidden": true` —— 可审计;
- `quality` 报告新增 `hidden_chunks` 计数 + `hidden_text_present` flag;enhancer 能力匹配显式不覆盖此 flag(确定性已处理,非模型可修)。
- IR `SCHEMA_VERSION` 0.2.0 → **0.3.0**(加法字段,serde default 向后兼容)。

## 验收

- 单测 67(+4 interpreter:Tr 3 隐藏且 Tr 0 复位、Tr 7、页外、0.5pt 微字;5pt 下标不误伤)。
- **端到端注入演示**:手工构造 676 字节 PDF,可见发票文本 + `Tr 3` 注入指令("IGNORE ALL PREVIOUS INSTRUCTIONS…")→ text/chunks 输出只含发票文本;JSON 保留注入文本且 `hidden: true`;quality 报 `1 hidden + hidden_text_present`。
- **零误伤回归**:三件套 + `2305`/`redp5110` 等真实文档 hidden=0;双记分牌不动(vs ODL 0.761 / vs Docling 0.832);clippy 零 warning。

## 显式不做(TODO 已标注)

同色文字(需填充色跟踪+背景推断,误报险)、被图片覆盖文字(需 z-order)。N5b 资源防护(zip bomb/深度上限)为下一步。
