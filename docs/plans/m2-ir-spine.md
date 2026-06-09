# Plan · M2 IR 脊梁（版本化 + provenance + 质量评分骨架）

> 里程碑细化，上层见 [beating-docling.md](beating-docling.md) M2 / [roadmap §6](../roadmap.md)。
> 目标：把"确定 + 可溯源"两项胜负手落到 IR 上，且**便宜、早定**——后续 M6 切块、M7 路由都路由在它之上。

## 设计决策

IR 是系统最重要的长期接口（roadmap 模块 1），改一次要稳，避免后续 churn。三件事：

1. **Schema 版本化**：`ir::SCHEMA_VERSION`，写入 `Document.provenance.schema_version`，agent 可据此判断兼容性。
2. **Provenance**：文档级 `Provenance { schema_version, parser, parser_version }`（哪个解析器/版本产出）。**不**给每个 chunk 重复存 parser/version（冗余）；元素级溯源用**已有的 `bbox + page`**（即引用锚点）+ 新增轻量 `confidence: f32`。多来源融合（M7）时再加元素级 source 覆盖。
3. **质量评分骨架**：`core::quality::QualityReport`，从 `&Document` 计算 coverage（有文本页占比）、garbled_ratio（非打印/替换字符占比）、flags（如 `ScannedNoText`）。**先出分，路由晚接**（M7）。格式无关，放 `core`。

## 落点

| 文件 | 改动 |
|---|---|
| `core/ir.rs` | `SCHEMA_VERSION`、`Provenance`、`TextChunk.confidence`、`Document.provenance`；serde `default` 保前后兼容 |
| `core/quality.rs`（新） | `QualityReport` + `QualityFlag` + `analyze(&Document)`，含单测 |
| `core/lib.rs` | `pub mod quality` |
| `pdf/lib.rs` | 填 `Document.provenance`（parser="pdf"，版本取 `CARGO_PKG_VERSION`） |
| `pdf/interpreter.rs` | `TextChunk` 构造加 `confidence: 1.0`（确定性路径） |
| `cli/main.rs` | `--quality` flag：把 `QualityReport`（JSON）打到 stderr，不污染 stdout |

## 验收

- JSON 顶层含 `provenance`（schema/parser/version）；每 text chunk 含 `bbox+page+confidence`（引用可定位率 100%）。
- **确定性**：同文件解析 100 次，输出 JSON 逐字节一致（rayon `par_iter().collect()` 保序 + `sort_by_key`，font HashMap 不影响输出序）。
- 质量评分：`chinese_scan`（扫描件）coverage=0、flag `ScannedNoText`；数字 PDF coverage 高、garbled 低。
- 三件套 + 2408 文本零回归；clippy 零 warning；单测全过。
- **不做**：路由/回退（M7）、元素级 source 覆盖（M7）、reading-order 异常分（占位留空，待 M3 后补）。
