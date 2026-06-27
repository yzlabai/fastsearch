# Devlog · N5c 复杂度画像——模块 9 收口,十大模块全部闭合

> 日期:2026-06-10 · plan:[n5-security-precheck.md §N5c](../plans/n5-security-precheck.md)

## 做了什么

`quality::PageProfile`:每页一条可解释、可序列化的画像,纯 IR 推导零额外成本:

- **kind**:digital / scanned / mixed / empty——按"有无可见文本 × 是否存在覆盖页面 ≥0.5 的图"判定(阈值与解释器像素附着门一致);
- **信号**:`text_chars`(不含 hidden)、`image_count`/`image_coverage`(最大图占页比)、`tables`、`enhanced_chunks`(source 非空 = 模型产出占比);
- 出口:CLI `--profile`(stderr JSON)、MCP `get_chunks` 信封新增 `profile` 字段(加法变更)。

N3 落地后这层才有完整素材(Image 元素带覆盖、source 标签)——这是它此前"暂缓"的原因,现在水到渠成。

## 验收

- 单测:四类 kind 判定 + 信号计数(含 0.2 覆盖小图不误判 digital→scanned);
- e2e:`chinese_scan` → scanned(覆盖 1.0);`1901` → 15 页全 digital(图覆盖 0.006);
- 82 单测、clippy 零 warning;chunks/REST 输出不变(画像不进渲染面,只进观测面)。

## 边界

旋转页/竖排密度信号留 TODO(需版面层信息贯通到 quality);画像目前供观测与下游消费,路由仍走 assess_page(scanned kind 与 ScannedNoText 等价,无行为变化)。
