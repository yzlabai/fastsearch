# spec · fastsearch-embed

> 模块 #6，依赖：fastsearch-core。阶段 P2。上游：[产品设计 §3.3](../plans/2026-06-24-产品设计文档.md)、需求 F12。
> 状态：**v1.1 已落地**（trait + HashEmbedder + HttpEmbedder；server 端 query/passage 自动嵌入已接入）。多模态 `EmbedInput`（M1/MM8）`gated`。

## 1. 目的与范围

嵌入后端抽象 + 离线基线实现。

- `Embedder` trait：`dim()` + `embed(texts, kind: Query|Passage)`。
- `HashEmbedder`：**确定性、零依赖**的 hashing bag-of-words 嵌入（固定维、L2 归一化）。用途：① 让全链路离线/CI 可跑（无需下模型）；② 作为 fallback。**非语义模型**——真正语义嵌入经**可配置 HTTP 后端**（`HttpEmbedder`：Ollama / OpenAI 兼容）接入。
- query/passage 前缀（e5 风格）：embed 按 kind 加前缀再编码。

**不做**：**进程内模型推理（Candle/ort 编译内置）**——真语义统一委托外部 HTTP 服务（`HttpEmbedder`），不引重依赖/不打包模型；rerank（rerank 模块）。

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
- [x] v1.1（**可配置 HTTP 嵌入后端，本地 Ollama 实网验证 done**，2026-06-25）：`HttpEmbedder` + `EmbedderConfig`/`build_embedder`/`from_env`。
  - 两协议：**Ollama** 原生 `/api/embed`（`{model,input}`→`{embeddings}`）与 **OpenAI 兼容** `/v1/embeddings`（`{data:[{embedding,index}]}`，按 index 排序）——后者覆盖 TEI/vLLM/LM Studio/llama.cpp-server/OpenAI。同步阻塞（`ureq`，纯 Rust）契合同步 trait；server 在 `spawn_blocking` 调用。
  - 可配置：`url/model/dim/api_key/query_prefix/passage_prefix/timeout`；**维度与 PG 向量列必须一致**（响应维度不符即报错，含诊断）。环境变量 `FASTSEARCH_EMBEDDER=hash|ollama|openai` + `FASTSEARCH_EMBED_*`。
  - 纯逻辑（请求体/响应解析/维度·条数校验/前缀/端点）+8 单测；**实网 env-gated**：本机 Ollama `nomic-embed-text-v2-moe`(768) 验证连通/维度/确定性/**语义性**（cos(相关)=0.31 > cos(无关)=0.18）。

**已知限制 / 下一迭代：**
- HashEmbedder **非语义**，仅离线/CI/fallback；真语义经 HTTP 后端（Ollama/OpenAI 兼容）接入——**这是默认且唯一推荐路径**（绕开重依赖与模型下载）。**进程内模型推理（Candle/ort）已决定不做**（2026-06-25）。
- **管线已接入（2026-06-27 核对）**：**server** 路径自动嵌入——query 走 `EmbedKind::Query`、passage/CDC 写入走 `EmbedKind::Passage`（`engine.set_embedder` + `apply_upsert` 在 CDC 落地主循环自动嵌 chunk → 写派生向量索引）。**CLI 仍纯文本路径**（未装 embedder，BM25-only 或需 `ingest_vector`/`req.vector` 外部传向量）。换嵌入模型维度变化时需同步 PG `vector_dim` 并重建派生索引。
- **多模态嵌入（MM8，gated，未实现）**：`Embedder` trait 当前仍只吃 `&[String]`（文本）。跨模态单向量（`EmbedInput::{Text,Image}` + `HttpEmbedder` 图像 base64 路由 + `caps()` 文图同空间断言，接 SigLIP-2/JinaCLIP-v2/jina-v4/Voyage/Cohere）属 [多模态计划 M1](../plans/2026-06-25-多模态功能设计与开发计划.md)，**待多模态 HTTP 模型服务**，状态 `gated`——M0 阶段图/音/视频经 docparse 的 caption/转录走**文本**嵌入,本 crate 无需改动。
