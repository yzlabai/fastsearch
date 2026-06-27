# Devlog · G8b 首增量:VLM 服务接入(OpenAI 兼容)+ 图片描述任务

> 日期:2026-06-10 · plan:[closing-docling-gaps.md §G8b](../plans/closing-docling-gaps.md) · 新依赖:ureq+base64(用户批准)

## 交付

- **`docparse-vlm`**(第八个 crate):`VlmClient` 说 OpenAI 兼容协议(`POST /v1/chat/completions` + base64 PNG data-URL)——一个协议通吃 vLLM/Ollama/LM Studio/云端;Bearer 鉴权可选;120s 超时。
- **首任务:图片描述**(`annotate_pictures`):每页 ≥1% 面积的图区域 → 按需渲染该页(docparse-raster,**矢量图表同样可描述**,不限嵌入位图)→ 裁剪 + 降采样(最大边 1024,防 ~8MB 请求)→ VLM 出一两句描述 → 以 `[figure] …` 文本注入图的位置(`source: "vlm:<model>"`,confidence 0.8)——text/markdown/chunks 在阅读序中天然可见,RAG 直接受益。
- **自研最小 PNG 编码器**(stored-deflate + 手写 CRC32/Adler32):零图像/压缩依赖。
- CLI:`--vlm-describe --vlm-url <u> --vlm-model <m> [--vlm-api-key]`(PDF,opt-in)。

## 工程要点

- **协议钉死不靠外部服务**:单测起一次性 TcpListener mock 服务器,断言路径/鉴权头/请求体结构(model/messages/image_url 前缀),回放 OpenAI 格式响应——协议正确性在 CI 里就锁住。
- **降级哲学**:服务不可达/单图失败 → stderr + 跳过,确定性结果照常交付(实测 Connection refused 全程不崩,`vlm_described_figures: 0`);缺配置 → 明确报错。
- review 加固:大图降采样(实测前的审查发现)、PNG 测试升级为**解码回放验证**(stored 块还原 + Adler32 校验 + 像素存活)、crop 测试验证 y 翻转的像素内容。

## 待办(G8b 余项)

- **真实服务端到端**:本机无 Ollama/vLLM,验收用 mock 锁协议;有环境后跑 `ollama run qwen2.5vl` + 实测一例并回填 testresults;
- 图表→表格、公式→LaTeX、整页转写(prompt 模板已留位)、页型判官(`--layout` 自动路由的候选解)、MCP/REST 透传。

92 单测(+4)、clippy 零 warning、二进制 25.27MB(ureq +1.8MB,30MB 门内)、记分牌不受影响(默认关)。
