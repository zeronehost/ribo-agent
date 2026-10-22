//! `ra-core`：整个工作空间共享的核心 crate。
//!
//! 目前承载两类基础设施：
//!
//! - 统一错误处理：[`error::RiboError`]、[`error::RiboResult`] 与
//!   [`error::Severity`]；
//! - 消息模型：[`message`] 模块定义与模型提供方交互的对话消息结构（用户消息、助手消息、
//!   工具结果、内容块与用量统计）。
//!
//! 各模块的原始路径为稳定公开路径，上层 crate（例如未来的 `ra-provider`）
//! 直接从对应模块引入即可，例如 `use ra_core::error::RiboError`。
//!
//! # Examples
//!
//! ```
//! use ra_core::error::{RiboError, RiboResult, Severity};
//!
//! fn run() -> RiboResult<()> {
//!   Err(RiboError::new("demo", "not_ready", "feature not ready")
//!     .with_severity(Severity::Warning))
//! }
//!
//! assert!(run().is_err());
//! ```

pub mod error;
pub mod message;
pub mod utils;
