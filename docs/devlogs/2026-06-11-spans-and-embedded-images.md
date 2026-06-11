# 2026-06-11 · span 语义入 IR + 图片 base64 内嵌(IR 0.7.0)

## TL;DR

两个收尾需求一次落地,IR 升 **0.7.0**(纯增量字段,旧消费者不受影响):

1. **rowspan/colspan 语义入 IR**:`Cell` 增 `row_span`/`col_span`(锚格 ≥1,序列化时 1 省略)与 `merged`(被覆盖位标记)。**网格平铺口径不破**——每个位置仍materialized(行主索引、eval/ODL 口径全兼容),要真实拓扑的消费者读锚格 span。`--table-model` 路径填真实值(pg9 实测:`# enc-layers` row_span=2、`TEDs` col_span=3、覆盖位 merged=true);确定性检测器恒 1×1(诚实:几何路径推不出 span)。
2. **图片 base64 内嵌**:`ImageChunk.data_base64`+`data_media_type`,CLI `--image-embed`、MCP `images:"embedded"`、REST `?images=embedded`——`--image-dir`(写盘)的服务面对应物,补齐 ODL `image_output` 三态(off/external/embedded)。JPEG 原字节直通 base64、位图转 PNG(复用 vlm 编码器);两面活进程 e2e 过(base64 解码校验)。

125 测试、clippy 0;记分牌/三件套逐字不变(新字段非默认才序列化,默认输出零变化)。

## 设计要点

- **平铺+标注**而非稀疏网格:span 改成稀疏表示会破掉 eval、chunks 文本化、Markdown 管道的所有"rows[r][c] 有效"假设——锚格带 span、覆盖位带 merged 的混合表示让两类消费者各取所需,零迁移成本;
- 序列化卫生:`row_span/col_span` 为 1、`merged` 为 false 时全部省略——确定性输出 JSON 逐字节与 0.6.0 相同;
- embedded 模式在 EnhanceOpts 走 `images_embedded`(解析期需要 decode 开关,故在 parse 前读取);非 PDF 输入(图片即文档后端)天然自带像素,同样可内嵌。

## 余项

- Markdown/HTML 输出利用 span(目前 Markdown 管道仍吃平铺网格——pipe 表本就表达不了 span;HTML 输出格式立项时再用);
- 确定性检测器的 span 推断(G3b 兜底,横线覆盖+对齐,优先级低)。
