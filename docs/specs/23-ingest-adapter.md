# spec · fastsearch-ingest-adapter

> 模块 #13（[00-模块拆分](00-模块拆分.md)），上游：[FS-303 实施计划](../plans/2026-09-01-FS-303-独立摄取worker与MCP闭环.md)。
> 状态：✅ 已完成并随 FS-303 通过全链路验收（2026-09-01）。

## 1. 目的与范围

CLI 与独立 worker 共用的唯一 docparse → `fastsearch_core::Chunk` 适配层。常态部分只提供
`ChunkProfile`；解析实现由 feature 门控。它不持有 server URL、API key、collection，也不发网络请求。

不做 job 调度、身份判定、对象存储或索引发布；`tenant/acl` 只是兼容既有 CLI 直传路径的适配参数，
worker 输出会在其专用 wire DTO 中删除它们。

## 2. 公开接口

```rust
pub struct ChunkProfile { /* validated provenance */ }
impl ChunkProfile {
    pub fn new(name: impl Into<String>, version: u32, target: usize,
               overlap: usize, table_markdown: bool) -> anyhow::Result<Self>;
    pub fn text_default() -> Self;
    pub fn docparse_default() -> Self;
}

#[cfg(feature = "parse")]
pub struct ParseOptions { /* file/doc_id/tenant/acl/images/profile/enhancements */ }
#[cfg(feature = "parse")]
pub fn chunks_for_file(options: &ParseOptions) -> anyhow::Result<Vec<Chunk>>;
```

feature：`parse`（轻量多格式）、`parse-ocr`、`parse-tables`、`parse-vlm`。后三档均要求对应
运行时模型/服务 env；`Enhancements` 的显式 false 必须保证跳过。

## 3. 数据结构与行为

- `ChunkProfile` 拒绝空名、version=0、target=0、overlap>=target，并把完整 profile 写入
  `metadata.chunking`。
- 解析器按扩展名/magic 分派 PDF/DOCX/HTML/Markdown/CSV/XLSX/PPTX/SRT/EML/Image。
- `ImageBytes::{Object,Inline,None}` 控制是否采集编码图字节；无可编码字节时如实标
  `missing_bytes`，不伪造 inline 资产。
- 增强顺序保持 OCR → VLM → UniRec tables；VLM 先处理，UniRec 只兜底。

## 4. 依赖

常态：core、anyhow、serde_json。`parse` 才引入 vendor/docparse；OCR/VLM/表格重依赖继续按 feature
隔离。server/engine 不依赖本 crate，默认 CLI 以 `default-features=false` 依赖。

## 5. 测试与验收

- profile 边界与 provenance；多格式 dispatch；docparse/core 映射。
- PDF JPEG、DOCX PNG 字节逐字节闭环；`images=none` 文本/坐标零回归。
- `cargo tree -p fastsearch-cli -e normal` 不含 `docparse-*`。
- CLI 原解析测试迁移后结果不变，且仓库不存在第二份 parser registry/映射实现。

## 6. 迭代记录

- 2026-09-01：从 `fastsearch-cli/src/ingest.rs` 移动实现，CLI 改为薄 facade；新增显式增强开关，
  供 worker 把 `parse_profile` 与编译 feature/运行时 env 做三方一致校验。
