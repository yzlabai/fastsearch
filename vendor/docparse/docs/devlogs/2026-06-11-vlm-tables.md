# 2026-06-11 · `--vlm-tables`:VLM 重抽已检出表的结构(G8b 第二任务)

## TL;DR

`docparse doc.pdf --vlm-tables --vlm-url ... --vlm-model ...`:对每张已检出的表按需渲染其区域、让 VLM 输出 TSV,解析合格(≥2×2 真网格)才**替换**确定性网格——合并单元格/多级表头这些几何检测器做不到的拓扑,模型看图能解。失败一律保底确定性网格(确定性结果永远成立)。替换的表带 `Table.source: "vlm:<model>"`(IR 新溯源字段,同 TextChunk 约定)。本地 mock 服务全链路 e2e 通过;**真实服务验收(Ollama/vLLM)候环境回填**。106 测试、clippy 0、记分牌/三件套零变化。

## 实现(~120 行,全部复用 G8b 栈)

- `docparse-vlm::refine_tables`:沿 `annotate_pictures` 模式——整页渲一次(hayro 2.0×)→ `crop_region` 裁表 bbox(y 翻转)→ `ask_about_image`(TSV prompt:每行一行、TAB 分格、跨格重复值、禁 markdown/评论)。
- `parse_tsv_grid` 防劣化门:容忍 ``` 围栏与参差行(补齐到最宽);**拒绝**散文/拒答(<2×2、过半行只有 1 列)——模型的"我看不到表"绝不能覆盖确定性结果。单测锁定。
- `grid_cells`:VLM 不给几何,cell bbox 取表区(真实)的均匀剖分——诚实近似,注释标明。
- IR:`Table.source: Option<String>`(serde skip-none,默认 None=确定性检测)。
- CLI:`--vlm-tables` 与 `--vlm-describe` 共用 url/model/key 校验,可同开。

## 验证

- 单测 ×3(TSV 解析/拒劣/格平铺)+ 既有协议 mock 测试;
- **全链路 e2e**(本地 mock HTTP 返回 3×3 TSV):pg9 的表被替换为 3×3、`source: vlm:mock-model`、cell bbox 平铺真实表区 ✓;
- **降级路径实测**:服务超时 → stderr 报错、`vlm_refined_tables: 0`、确定性 5×8 网格原样保留、退出码 0 ✓;
- 默认路径(不带旗标)记分牌逐字不变(NID 0.792/MHS 0.685/TEDS 0.419),三件套不变。

## 边界

- 作用域=**已检出的表**:漏检的表不会被 VLM 发现(那是页型判官/整页转写的活,G8b 余项);
- TSV 是有损协定:跨格以重复值表达,rowspan/colspan 语义仍未建模(IR 远期项);
- 真实模型的 TSV 纪律未实测——prompt 可能要按模型调,验收候 Ollama(qwen2.5-vl 级)回填 testresults。
