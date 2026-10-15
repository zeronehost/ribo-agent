//! 请求上下文：一次模型调用携带的系统提示词、消息历史与工具集合。

use ra_core::message::{Message, ToolSchema};

/// 一次模型请求的完整对话上下文。
///
/// 与运行参数（超时、重试等，见 [`crate::options::StreamOptions`]）分离：
/// 本结构描述「让模型基于什么内容作答」，随请求体提交给提供方。
#[derive(
  Debug, Default, Clone, PartialEq, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "camelCase")]
pub struct Context {
  /// 系统提示词，用于设定模型的角色与全局行为约束；缺省时不发送该字段。
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub system_prompt: Option<String>,
  /// 对话消息历史，按时间顺序排列（用户、助手、工具结果可混合）。
  pub messages: Vec<Message>,
  /// 本轮允许模型调用的工具 schema 列表；为空时不参与序列化，
  /// 表示这是一次纯对话请求。
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub tools: Vec<ToolSchema>,
}
