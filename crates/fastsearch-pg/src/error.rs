//! 错误类型。

use thiserror::Error;

#[derive(Debug, Error)]
pub enum PgError {
    #[error("postgres error: {0}")]
    Pg(#[from] tokio_postgres::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("mapping error: {0}")]
    Mapping(String),
    #[error("config error: {0}")]
    Config(String),
    #[error("core error: {0}")]
    Core(#[from] fastsearch_core::CoreError),
}

pub type Result<T> = std::result::Result<T, PgError>;
