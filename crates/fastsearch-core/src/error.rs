//! 错误类型：core 层只在"请求/契约校验"边界产生错误，纯算法不产生 I/O 错误。

use thiserror::Error;

/// `fastsearch-core` 的错误。
#[derive(Debug, Error, PartialEq, Eq)]
pub enum CoreError {
    /// 检索请求非法（字段越界、组合无效等）。
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// 引用 id 解析失败。
    #[error("invalid citation id: {0}")]
    InvalidCitation(String),
    /// 过滤表达式非法。
    #[error("invalid filter: {0}")]
    InvalidFilter(String),
}

/// core 内部统一 Result。
pub type Result<T> = std::result::Result<T, CoreError>;
