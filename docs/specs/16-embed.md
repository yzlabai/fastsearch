# spec · fastsearch-embed

> 模块 #6，依赖：fastsearch-core。阶段 P2。上游：[产品设计 §3.3](../plans/2026-06-24-产品设计文档.md)、需求 F12。
> 状态：**开发中**。

## 1. 目的与范围

嵌入后端抽象 + 离线基线实现。

- `Embedder` trait：`dim()` + `embed(texts, kind: Query|Passage)`。
- `HashEmbedder`：**确定性、零依赖**的 hashing bag-of-words 嵌入（固定维、L2 归一化）。用途：① 让全链路离线/CI 可跑（无需下模型）；② 作为 fallback。**非语义模型**——真正语义嵌入需 Candle/ort + 模型（下一迭代）。
- query/passage 前缀（e5 风格）：embed 按 kind 加前缀再编码。

**不做**：真神经模型推理（Candle/ort + bge/e5，下一迭代，重依赖 + 模型下载）；rerank（rerank 模块）。

## 2. 公开接口

```rust
pub enum EmbedKind { Query, Passage }
pub trait Embedder {
    fn dim(&self) -> usize;
    fn embed(&self, texts: &[String], kind: EmbedKind) -> anyhow::Result<Vec<Vec<f32>>>;
}
pub struct HashEmbedder { dim: usize }
impl HashEmbedder { pub fn new(dim: usize) -> Self; }
```

## 3. 行为规约

- **确定性**：同输入（含 kind）→ 同向量（可复现、可测）。
- **维度固定**：所有输出长度 = dim；L2 归一化（零文本→零向量，不 panic）。
- **kind 前缀**：Query→"query: "，Passage→"passage: "（与 e5 约定一致；影响 hash 结果）。
- **健壮**：空 texts→空结果；空字符串→零向量。

## 4. 依赖

`fastsearch-core`（可选，用不到也行）、`anyhow`。零重依赖。

## 5. 测试用例

1. dim 正确、所有输出长度=dim。
2. 确定性：同文本同 kind 两次结果一致。
3. 不同文本→不同向量；同文本 Query vs Passage→不同向量（前缀生效）。
4. L2 归一化：非零向量模≈1。
5. 空文本→零向量、不 panic；空 batch→空。

## 6. 验收标准与状态

- [x] v1 完成：Embedder trait + EmbedKind + HashEmbedder（确定性/L2 归一化/前缀）+ 5 单测绿。clippy 净、fmt 净。

**已知限制 / 下一迭代：**
- HashEmbedder **非语义**——只用于离线/CI 跑通与 fallback。真语义嵌入需 **Candle（纯 Rust，include_bytes 静态链 e5-small）或 ort（ONNX）+ 模型下载**，作为 opt-in feature 在下一迭代加（重依赖，按 docparse 模型 opt-in 哲学，首次用时下载）。
- 接入 engine 的"自动嵌入回填"（CDC 后对新 chunk 生成向量）待与 vector/sync 串联。
