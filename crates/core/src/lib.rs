//! `ra-core`：整个工作空间共享的核心 crate。
//!
//! 目前承载统一的错误处理设施，并按使用频率在 crate 根扁平再导出，
//! 这样上层 crate（例如未来的 `ra-provider`）可以直接 `use ra_core::RiboError`，
//! 无需关心内部模块划分；`error` 模块的原始路径同时保留可用。
//!
//! # Examples
//!
//! ```
//! use ra_core::{RiboError, RiboResult, Severity};
//!
//! fn run() -> RiboResult<()> {
//!   Err(RiboError::new("demo", "not_ready", "feature not ready")
//!     .with_severity(Severity::Warning))
//! }
//!
//! assert!(run().is_err());
//! ```

pub mod error;

pub use error::{RiboError, RiboResult, Severity};
