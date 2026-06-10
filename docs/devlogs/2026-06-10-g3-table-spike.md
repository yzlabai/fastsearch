# Devlog · G3 门控 spike:表结构模型 × tract——两个候选全部失败,按计划降级

> 日期:2026-06-10 · plan:[closing-docling-gaps.md §G3](../plans/closing-docling-gaps.md)("spike 不过即降级,不硬上") · spike 代码 `tmp/slanet-spike`(不合入)

## 试了什么(全部带具体死因)

| 候选 | 架构 | 结果 |
|---|---|---|
| **SLANet-plus**(7.7MB,RapidTable/PP-StructureV2) | 自回归注意力解码器 | ❌ ONNX `Loop` 算子,tract `Unimplemented(Loop)` ——计划标注的最高风险点命中 |
| **Table Transformer**(115MB,Microsoft/Xenova 导出) | DETR 检测式(无 Loop,理论 tract 友好) | ❌ 三连:`into_optimized` 在合法形状下 OOM(exit 137);非 32 对齐输入 Reshape 推断失败;跳过优化后运行时位置嵌入广播错配(192 vs 196608)——该导出按 onnxruntime 动态形状假设制作,tract 静态管线不兼容 |

过程产出(留用):SLANet 词表的**内嵌 ONNX metadata 提取**方法(手写 protobuf 顶层遍历,48 个结构 token);RapidTable 预处理参数(488²,ImageNet 归一);TATR 标签集(行/列/列头/合并单元格,DETR cxcywh)。

## 结论与路线(按计划降级,不硬上)

1. **本地 tract 表结构模型:此路不通**(当前可得的 ONNX 导出范围内)。重开条件:tract 支持 Loop / 出现 tract 友好的表结构导出 / 决定引 ort(C++,需用户决策,暂不提请);
2. **主路线改为 VLM(G8b 顺路)**:表区域图 → 视觉模型 → markdown 表——`VlmClient` + raster + crop 全部现成,实现 `--vlm-tables` 任务约百行;Qwen2.5-VL 级模型的表结构能力即 TEDS 差距的解;验收依赖本地起 Ollama/vLLM;
3. **G3b 确定性兜底**保持原计划(高置信合并单元格几何推断,P1c 教训约束下小步做)。

## 教训

"风险最高"的预判 + "spike 定生死"的纪律救了一周工时:两个模型从下载到判死共约 1.5 小时,没有任何代码债进入主干。
