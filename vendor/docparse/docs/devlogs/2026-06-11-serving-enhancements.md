# 2026-06-11 · MCP/REST 透传全部增强能力(G8b 余项收口)

## TL;DR

这两天建的增强面(`--layout`/`--table-model`/`--formula-model`/`--vlm-describe`/`--vlm-tables`)此前只有 CLI 能用——而本项目的核心受众是 agent。本次把它们全部透传到 MCP 与 REST:

```bash
docparse mcp   --unirec-models models/unirec --vlm-url http://127.0.0.1:11434 --vlm-model qwen2.5vl
docparse serve --port 8642 --unirec-models models/unirec
# MCP tool 参数:  { "path": ..., "table_model": true, "formula_model": true, "layout": true, ... }
# REST 查询参数:  /parse?format=json&table_model=true
```

活进程 e2e 双过(MCP stdio 与 REST multipart 各跑 pg9 → `source: "table:unirec-0.1b"`、10×8 网格);125 测试、clippy 0、默认请求字节不变。

## 设计

- **`EnhanceState` + `EnhanceOpts` 两面共用**(main.rs):State 持服务启动配置(OCR 目录、layout 模型路径、UniRec 目录、VLM url/model/key)与**懒加载模型缓存**——UniRec ~700MB,服务生命周期只加载一次(`OnceLock<Result<Arc<UniRec>,String>>`,失败字符串也缓存=坏配置快速失败,沿 OcrState 惯例);Opts 是请求级布尔开关,全默认关=确定性结果。
- **能力未配置 → 指名启动旗标的干净错误**("start with --unirec-models <dir>"),agent 拿到可行动的信息;**非 PDF + PDF-only 增强 → 静默跳过**(tool description 写明),解析照常成功。
- **应用顺序与 CLI 一致**:ocr → layout → table_model → formula_model → vlm_describe → vlm_tables——同请求跨接口同结果的不变量延续。
- MCP tool schema 三个工具全量加六个布尔参数;REST 查询参数同名;Python 薄客户端两个传输同步加参(子进程版 table_model/formula_model 取目录路径,HTTP 版取布尔)。

## 验证

- 单测:能力未配置的行为(非 PDF 跳过成功)、既有字节一致/降级测试随签名迁移全绿;
- **活进程 e2e**:`docparse mcp --unirec-models …` stdio 来回 → 表被 UniRec 重抽(source 戳对、10×8);`docparse serve` + curl 查询参数 → 同结果;
- 默认请求(无开关)与升级前字节一致。

## 边界

- VLM 的 url/model 是**服务启动配置**而非请求参数——避免把任意外呼地址交给每个请求(同机信任模型下仍是合理收紧);按请求换模型的需求出现时再议;
- `--image-dir`(写盘语义)不透传——服务面的对应物是 base64 embedded 模式(ODL `image_output="embedded"`),另行立项;
- locate 工具也接受增强参数(表替换后定位语义一致)。
