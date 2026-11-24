//! 一次请求的上下文聚合。
//!
//! 把"系统提示、历史消息、工具列表、工具选择策略"打包成一个值传递给
//! [`Provider::stream`](crate::traits::Provider)，避免方法签名随选项增长而膨胀。
//!
//! `Context` 与上游 `ra_core::message` 的类型直接耦合：
//! - `system_prompt` 用 `Vec<Content>` 而非单个 `String`，因为模型可能支持多模态系统提示；
//! - `tools` 与 `tool_choice` 决定适配器是否附带工具声明
//!   （某些 provider 不支持工具调用，适配器应据此跳过声明）。

use ra_core::message::{Content, Message, ToolChoice, ToolSchema};

/// 一次请求的上下文。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Context {
  /// 系统提示（多模态支持）。
  pub system_prompt: Vec<Content>,
  /// 历史消息。
  pub message: Vec<Message>,
  /// 可用工具列表。
  pub tools: Vec<ToolSchema>,
  /// 工具选择策略。
  pub tool_choice: ToolChoice,
}
