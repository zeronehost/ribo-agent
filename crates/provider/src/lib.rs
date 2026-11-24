//! ra-provider：统一的模型提供方抽象层。
//!
//! 本 crate 为上层（网关、工作流引擎等）屏蔽各家 LLM 提供方的线格式差异，
//! 对外暴露统一的事件流与错误模型。核心模块：
//!
//! - [`ids`]：`ProviderId` / `ModelId` 强类型标识。
//! - [`model`]：模型元信息结构体（能力、计费、思考档位等）。
//! - [`context`]：一次请求的上下文聚合（系统提示、历史消息、工具）。
//! - [`traits`]：`Provider` trait 与 `ProviderErrorExt` 错误谓词扩展。
//! - [`error`]：本地化错误码常量、阶段门构造器与 `RiboError` 扩展。
//! - [`stream`]：`ProviderEvent` 事件模型与 `ProviderEventStream` /
//!   `ProviderEventStreamProducer` 传输机制。
//! - [`options`]：流式请求入参封装（超时、重试、采样与取消令牌）。
//! - [`retry`]：请求级重试设施（退避策略、首包超时守卫）。
//! - [`gateway`]：多提供方网关（路由、熔断、重试与降级编排）。

pub mod adapters;
pub mod context;
pub mod error;
pub mod gateway;
pub mod ids;
pub mod model;
pub mod options;
pub mod retry;
pub mod stream;
pub mod traits;

/// 测试辅助工具（仅测试构建启用）。
#[cfg(test)]
pub(crate) mod test_support;
