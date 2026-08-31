# Chunk overlap profile test cases

## TC-001 · 同页正文按完整 block 重叠
- 用例：RAG chunking / P0 / unit
- 前置：三个可区分正文 block，小 target、可容纳最后一个完整 block 的 overlap。
- 步骤：调用 `chunk_document_with`。
- 预期：第二条以第一条尾 block 开头；bbox 是真实复用 block 与新 block 的 union；char_len 精确。
- 测试位置：`crates/docparse-core/src/chunk.rs`

## TC-002 · 引用边界不泄漏 overlap
- 用例：citation fidelity / P0 / unit
- 前置：heading 或换页位于两个 paragraph 组之间。
- 步骤：开启 overlap 分块。
- 预期：新 section/page 不带前一边界文本，heading_path/section_id/page 正确。
- 测试位置：`crates/docparse-core/src/chunk.rs`

## TC-003 · 默认行为兼容
- 用例：library compatibility / P0 / unit
- 前置：包含标题、正文、表格的固定 Document。
- 步骤：比较 `chunk_document` 与 `chunk_document_with(Default)`。
- 预期：结果逐字段相同。
- 测试位置：`crates/docparse-core/src/chunk.rs`
