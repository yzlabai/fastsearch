# Geometry-safe chunk overlap for FastSearch FS-204

> 日期：2026-08-31
> 状态：✅ 已完成（2026-08-31）
> 上游：FastSearch `docs/plans/2026-08-31-FS-204-分块profile与上下文预算.md`

## 需求三件套

- 目标用户：需要调节 RAG 分块、同时依赖 page/bbox 精确引用的知识库开发者。
- 使用场景：FastSearch 客户端解析文档时指定 target/overlap；相邻 prose chunk 保留上下文，但不能伪造半段文字的坐标。
- 产品定位：延续 docparse “结构提取快路径 + 可溯源 chunk”能力，给 FastSearch KB-1.2 提供真实的分块 seam。

## 范围

- 给 `ChunkOptions` 增加 `overlap_chars`，只在因 target 形成的同页、同 section paragraph 边界复用完整 layout block。
- overlap 是上限；尾 block 单独超过上限时该边界不重叠。
- 默认 0，保证 `chunk_document` 与旧 `chunk_document_with(Default)` 输出不变。
- 不跨 heading/table/image/code/list/page，不切半个 block，不新增依赖。

## 使用例子

```rust
let chunks = chunk_document_with(&doc, ChunkOptions {
    target_chars: 800,
    overlap_chars: 120,
    table_markdown: false,
});
```

## 测试与验收

- 多 paragraph fixture 在小 target 下产生至少两条 chunk，第二条以前一条尾 block 开头。
- overlap 文本、page、bbox union、heading_path、section_id、char_len 都与源 block 一致。
- heading/page/barrier 不携带 overlap。
- 默认 options 的序列化快照不变；core fmt/clippy/tests 全绿。

## 完成记录

- `ChunkOptions.overlap_chars` 默认 0；target flush 只携带上一个 paragraph 的完整尾部 block。
- `ParaBuf` 保存真实 block parts，以 parts 重建 text、bbox union、char_len，不制造半 block 坐标。
- heading/table/image/code/list/page 均以 overlap=0 flush，section/page 不泄漏。
- `cargo test -p docparse-core`：93 passed；`cargo clippy -p docparse-core --all-targets -- -D warnings`：通过。
- 详情（含测试结果）：[开发日志](../devlogs/2026-08-31-chunk-overlap-profile.md)。
