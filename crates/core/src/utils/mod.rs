//! crate 内部使用的通用辅助工具。
//!
//! 本模块仅对 crate 内部可见（`mod utils`），不作为公开 API 的一部分。

/// serde 属性辅助函数（`skip_serializing_if` 的路径型谓词）。
pub(crate) mod serde_fn;
pub mod time;
