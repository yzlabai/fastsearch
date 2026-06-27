# Devlog · G8a 代码块检测——确定性补齐 Docling code 标注的 born-digital 半场

> 日期:2026-06-10 · plan:[closing-docling-gaps.md §G8a](../plans/closing-docling-gaps.md) · 零模型零新依赖

## 实现(一条字体信号贯穿全链)

- **信号**:等宽字体 PS 名(`is_mono_font`:mono/courier/menlo/consolas/monaco/typewriter/cmtt)——前置是 G2 顺带把 `TextChunk.font` 从资源名("F7")改成了 BaseFont 真名。FontDescriptor FixedPitch 标志位留 TODO(名字法已覆盖实践所见)。
- **成组**(layout.rs):`Line.mono`(行内全等宽)→ group_blocks 中**等宽行互连规则**(豁免 fill_x/numeric 门——代码行短且常含数字,字体本身就是延续信号)→ ≥2 行成 `Block.code`。
- **缩进重建**:PDF 里行首空白是定位不是空格字符——按几何重建:`indent = (x0 − min_x0) / 0.5em`,上限 40。
- **出口**:Markdown fenced(```);chunks 新 kind `code`(独立成块,绝不并入散文段);等宽行永不判标题。

## 验收

- `code_and_formula.pdf`:JavaScript 代码块正确围栏,**缩进保留**(`     return a + b;`),与散文/标题正确分离;
- 88 单测(+1 成组与缩进)、clippy 零 warning;
- 双记分牌零回归(vs ODL 0.764 / vs Docling 0.833;`code_and_formula` 仍 0.999/1.0/1.0)、三件套不变。

## 边界

单行内联代码不标(防误报);依赖字体名(无名/非常规等宽字体漏检,FixedPitch 旗标可补);语言识别不做(fence 不带语言标签——VLM/G8b 可后补)。
