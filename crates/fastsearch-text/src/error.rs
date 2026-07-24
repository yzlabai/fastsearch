//! 错误类型。

use thiserror::Error;

#[derive(Debug, Error)]
pub enum TextError {
    #[error("tantivy error: {0}")]
    Tantivy(#[from] tantivy::TantivyError),
    #[error(
        "text index schema is incompatible with this fastsearch version; rebuild the derived index from the source store: {0}"
    )]
    SchemaMismatch(String),
    #[error("query parse error: {0}")]
    QueryParse(String),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("core error: {0}")]
    Core(#[from] fastsearch_core::CoreError),
}

pub type Result<T> = std::result::Result<T, TextError>;
