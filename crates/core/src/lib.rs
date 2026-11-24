//! `ra-core`：整个工作空间共享的核心 crate。
//!
//! 目前承载四类基础设施：
//!
//! - 统一错误处理：[`error::RiboError`]、[`error::RiboResult`] 与
//!   [`error::Severity`]；
//! - 域内路径解析：[`path::resolve_in_domain`] 提供信任域沙箱校验的唯一实现
//!   （词法拒绝 → canonicalize → starts_with）；
//! - 消息模型：[`message`] 模块定义与模型提供方交互的对话消息结构（用户消息、助手消息、
//!   工具结果、内容块与用量统计）；
//! - 熔断器：[`circuit`] 模块提供无锁三态熔断器（闭合 / 打开 / 半开），
//!   供网关（每提供方）与工具管理器（每工具）等调用方复用。
//!
//! 各模块的原始路径为稳定公开路径，上层 crate（例如 `ra-provider`、
//! `ra-tools`）直接从对应模块引入即可，例如 `use ra_core::error::RiboError`。
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

pub mod circuit;
pub mod error;
pub mod message;
pub mod path;
pub mod utils;
