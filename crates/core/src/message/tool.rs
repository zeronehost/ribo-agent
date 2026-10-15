//! 工具调用与工具结果消息模型。

use super::{content::Content, usage::Usage};

/// 模型发起的一次工具调用请求。
///
/// 该结构同时作为助手消息中的一种内容块（[`Content::ToolCall`]）存在；
/// 工具执行完成后，需用 [`ToolResultMessage`] 并带上相同的 [`id`](Self::id) 回灌。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
  /// 工具调用 ID，用于在工具结果消息中关联本次调用。
  pub id: String,
  /// 被调用的工具名。
  pub name: String,
  /// 模型生成的调用参数，通常是 JSON 对象；具体结构由工具自身约定。
  pub arguments: serde_json::Value,
  /// 部分提供方对工具调用签发的校验签名；无签名机制时为 `None`，
  /// 为 `None` 时不参与序列化。
  #[serde(skip_serializing_if = "Option::is_none")]
  pub signature: Option<String>,
  /// 工具所属命名空间（部分提供方用于区分内置工具与第三方工具）；
  /// 为 `None` 时不参与序列化。
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub namespace: Option<String>,
}

/// 工具执行完成后回灌给模型的结果消息。
///
/// 通过 [`tool_call_id`](Self::tool_call_id) 与助手消息中的某次
/// [`ToolCall`] 关联；执行是否失败由 [`is_error`](Self::is_error) 标记。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultMessage {
  /// 对应的 [`ToolCall::id`]。
  pub tool_call_id: String,
  /// 对应的 [`ToolCall::name`]。
  pub tool_name: String,
  /// 执行结果是否为错误：`true` 表示工具执行失败，`content` 中通常为错误描述。
  pub is_error: bool,
  /// 结果产生时间，Unix 毫秒时间戳。
  pub timestamp: i64,
  /// 结果内容块列表（文本、图片等）；为空时不参与序列化，
  /// 反序列化缺省该字段时按空列表处理。
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub content: Vec<Content>,
  /// 供调试或提供方专用的附加结构化细节；为 `None` 时不参与序列化。
  #[serde(skip_serializing_if = "Option::is_none")]
  pub details: Option<serde_json::Value>,
  /// 本次工具执行额外消耗的 token 用量（工具自身又触发了模型调用时才有）；
  /// 为 `None` 时不参与序列化。
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub usage: Option<Usage>,
  /// 执行过程中新发现/注册的工具名列表（如 MCP 动态工具发现场景）；
  /// 为空时不参与序列化。
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub added_tool_names: Vec<String>,
}
