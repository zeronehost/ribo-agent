//! `ra-tools`：工具调用 crate。
//!
//! 提供从工具声明到执行记账的完整链路：
//!
//! 1. **声明**：[`traits::AgentTool`] trait 约定工具的 schema、展示名、
//!    执行模式与执行入口；
//! 2. **注册**：[`registry::ToolRegistry`] 维护并发安全的工具表；
//! 3. **编排**：[`manager::ToolManager`] 在注册表之上叠加入参 JSON Schema
//!    校验缓存与整体熔断（复用 [`ra_core::circuit`]）；
//! 4. **执行上下文**：[`context::ToolContext`] 携带取消信号与流式输出回调；
//! 5. **内置工具**：[`builtin`] 提供 echo / calculator / time 与受限
//!    沙箱内的文件系统工具族。
//!
//! 错误码统一以 `tool.*` 为前缀（见 [`error`]）。
//!
//! # Examples
//!
//! ```
//! use std::path::Path;
//!
//! use ra_tools::{builtin::register_builtin, manager::ToolManager};
//!
//! // 以默认配置创建管理器，并注册全部内置工具（含 Schema 编译校验）
//! let manager = ToolManager::new();
//! register_builtin(&manager, Path::new("."))?;
//! for schema in manager.get_tool_schemas() {
//!   println!("{}: {}", schema.name, schema.description);
//! }
//! # Ok::<(), ra_core::error::RiboError>(())
//! ```

pub mod builtin;
pub mod context;
pub mod error;
pub mod manager;
pub mod registry;
pub mod schema;
pub mod traits;
pub mod types;
