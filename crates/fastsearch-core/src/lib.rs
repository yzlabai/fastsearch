//! # fastsearch-core
//!
//! fastsearch 的纯逻辑地基：文档模型、查询/过滤 AST、融合算法、引用模型。
//! **不依赖任何后端**（无 Tantivy / Postgres / 向量库），所有后端通过 trait
//! 边界接入。详见 [spec](../../docs/specs/10-core.md)。

mod error;
mod filter;
mod fusion;
mod model;
mod query;

pub use error::{CoreError, Result};
pub use filter::{AclFilter, FieldSource, FieldValue, Filter};
pub use fusion::{fuse, Fusion, Scored};
pub use model::{BBox, Chunk, ChunkKind, Citation, GlobalId, ImageMeta};
pub use query::{RerankSpec, SearchMode, SearchRequest};
