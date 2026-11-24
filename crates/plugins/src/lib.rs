//! `ra-plugins`：Ribo Agent 插件能力 crate（阶段 6）。
//!
//! 让外部插件（native Rust 与 Step B 的 WASM guest）产出的工具经统一
//! 声明边界注入 [`ToolManager`](ra_tools::manager::ToolManager)，agent
//! 主循环零改动。模块分层：
//!
//! 1. **声明层**：[`traits::Plugin`]（语言无关声明边界）+ [`traits::ToolManifest`]；
//! 2. **注册层**：[`registry::PluginRegistry`]（进程内注册表，原子批量注入）；
//! 3. **适配层**：[`adapter::PluginToolAdapter`]（`Plugin` → `Arc<dyn AgentTool>`，
//!    工具名自动加 `{plugin_id}__` 前缀）；
//! 4. **能力层**：[`acl::UrlPolicy`] / [`acl::PluginAcl`]（出站 URL 裁决
//!    与插件能力声明，`http-proxy` 等宿主能力的 ACL 基础）；
//! 5. **权威层**：[`authority::AuthorityManifest`]（宿主侧载的信任源，
//!    guest 自述降级为交叉校验——Step C B3.2 信任链闭合）。
//!
//! 错误码统一以 `plugin.plugin_*` 为前缀（见 [`error`]）。
//!
//! # Examples
//!
//! ```
//! use std::sync::Arc;
//!
//! use async_trait::async_trait;
//! use ra_core::error::RiboResult;
//! use ra_plugins::{registry::PluginRegistry, traits::{Plugin, ToolManifest}};
//! use ra_tools::{context::ToolContext, manager::ToolManager};
//!
//! // 最小 native 插件：提供一个 echo 工具
//! struct EchoPlugin;
//!
//! #[async_trait]
//! impl Plugin for EchoPlugin {
//!   fn id(&self) -> &str { "demo" }
//!
//!   fn tools(&self) -> Vec<ToolManifest> {
//!     vec![ToolManifest {
//!       name: "echo".to_string(),
//!       description: "echo args back".to_string(),
//!       parameters: serde_json::json!({ "type": "object", "properties": {} }),
//!       timeout_hint: None,
//!       idempotent: true,
//!       memory_limit_bytes: None,
//!     }]
//!   }
//!
//!   async fn execute(&self, _tool: &str, args: serde_json::Value,
//!                    _ctx: &ToolContext) -> RiboResult<serde_json::Value> {
//!     Ok(args)
//!   }
//! }
//!
//! # fn main() -> RiboResult<()> {
//! let manager = Arc::new(ToolManager::new());
//! let registry = PluginRegistry::new(manager);
//! registry.register(Arc::new(EchoPlugin))?;
//! // 插件工具以 `demo__echo` 之名对 agent 主循环可见
//! # Ok(())
//! # }
//! ```

pub mod acl;
pub mod adapter;
pub mod authority;
pub mod config;
pub mod error;
pub mod provenance;
pub mod registry;
pub mod traits;
#[cfg(feature = "wasm")]
pub mod wasm;
