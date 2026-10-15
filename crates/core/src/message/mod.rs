//! 与模型提供方交互的对话消息模型。
//!
//! 本模块定义一轮对话中可能出现的三类消息：
//!
//! - [`UserMessage`]：用户（或本地调用方）发出的消息；
//! - [`AssistantMessage`]：模型返回的消息，可携带文本、思考过程、工具调用等内容块；
//! - [`ToolResultMessage`]：工具执行完成后回灌给模型的结果消息。
//!
//! 三类消息统一收敛在 [`Message`] 枚举下，按 `role` 字段做内部标签序列化，
//! 可在同一个 JSON 数组中混合传输与持久化。子模块中的类型通过 `pub use ...::*`
//! 在本模块扁平再导出，使用方直接 `use ra_core::message::Message` 即可。

mod assistant;
mod content;
mod tool;
mod usage;
mod user;

pub use self::{assistant::*, content::*, tool::*, usage::*, user::*};

/// 对话中的一条消息，按发言角色区分具体类型。
///
/// 序列化时采用 `role` 字段做内部标签（serde 内部表示为 `"user"`、
/// `"assistant"`、`"toolResult"`，反序列化时也依据该字段判定枚举变体，
/// 因此三类消息可以放在同一个 JSON 数组中混合存储。
///
/// # Examples
///
/// ```
/// use ra_core::message::Message;
///
/// let json = serde_json::json!({
///   "role": "assistant",
///   "content": [],
///   "model": "gpt",
///   "provider": "demo",
///   "usage": {
///     "input": 0,
///     "output": 0,
///     "cacheRead": 0,
///     "cacheWrite": 0,
///     "totalTokens": 0,
///     "cost": {
///       "input": 0.0,
///       "output": 0.0,
///       "cacheRead": 0.0,
///       "cacheWrite": 0.0,
///       "total": 0.0
///     }
///   },
///   "stopReason": "stop",
///   "timestamp": 0
/// });
///
/// assert!(matches!(
///     serde_json::from_value::<Message>(json).unwrap(),
///     Message::Assistant(_)
/// ));
/// ```
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum Message {
  /// 用户发出的消息。
  User(UserMessage),
  /// 模型返回的消息。
  Assistant(AssistantMessage),
  /// 工具执行结果回灌消息。
  ToolResult(ToolResultMessage),
}

/// 手动实现反序列化：先取出 `role` 标签判定目标变体，再把整个 JSON 对象
/// 交给对应具体消息类型反序列化；未知角色直接报错，避免静默丢数据。
impl<'de> serde::Deserialize<'de> for Message {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: serde::Deserializer<'de>,
  {
    // ===== 取值阶段 =====
    // 先反序列化为通用 JSON 值，才能 peek 其中的 role 字段而不消耗输入。
    let value = serde_json::Value::deserialize(deserializer)?;
    let role = value
      .get("role")
      .and_then(serde_json::Value::as_str)
      .ok_or_else(|| serde::de::Error::missing_field("role"))?;
    // ===== 分发阶段 =====
    // 按角色选择具体变体；错误信息原样转交，便于定位具体字段问题。
    match role {
      "user" => serde_json::from_value(value)
        .map(Message::User)
        .map_err(serde::de::Error::custom),
      "assistant" => serde_json::from_value(value)
        .map(Message::Assistant)
        .map_err(serde::de::Error::custom),
      "toolResult" => serde_json::from_value(value)
        .map(Message::ToolResult)
        .map_err(serde::de::Error::custom),
      other => Err(serde::de::Error::unknown_field(
        other,
        &["user", "assistant", "toolResult"],
      )),
    }
  }
}

/// 便于将具体消息直接收集为 [`Message`]，例如 `.map(Message::from)`。
impl From<UserMessage> for Message {
  fn from(value: UserMessage) -> Self {
    Message::User(value)
  }
}

/// 便于将具体消息直接收集为 [`Message`]，例如 `.map(Message::from)`。
impl From<AssistantMessage> for Message {
  fn from(value: AssistantMessage) -> Self {
    Message::Assistant(value)
  }
}

/// 便于将具体消息直接收集为 [`Message`]，例如 `.map(Message::from)`。
impl From<ToolResultMessage> for Message {
  fn from(value: ToolResultMessage) -> Self {
    Message::ToolResult(value)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  // 场景：反序列化时 role 为 toolResult。
  // 预期：能够命中 ToolResult 分支并成功解析，证明该 match 臂完整闭合。
  #[test]
  fn deserialize_tool_result_message_test() {
    let json = serde_json::json!({
      "role": "toolResult",
      "toolCallId": "call_1",
      "toolName": "search",
      "isError": false,
      "timestamp": 0
    });

    let message = serde_json::from_value::<Message>(json)
      .expect("toolResult message should deserialize");
    assert!(matches!(message, Message::ToolResult(_)));
  }

  // 场景：content 为空的 ToolResultMessage 先序列化再反序列化。
  // 预期：空列表序列化时被省略，缺省字段反序列化时按空列表补回，往返保持相等。
  #[test]
  fn tool_result_empty_content_roundtrip_test() {
    let original = Message::ToolResult(ToolResultMessage {
      tool_call_id: "call_1".to_string(),
      tool_name: "search".to_string(),
      is_error: false,
      timestamp: 0,
      content: Vec::new(),
      details: None,
      usage: None,
      added_tool_names: Vec::new(),
    });

    let json = serde_json::to_value(&original).expect("should serialize");
    // 空列表字段不应出现在序列化结果中
    assert!(json.get("content").is_none());

    let restored = serde_json::from_value::<Message>(json)
      .expect("should deserialize without content field");
    assert_eq!(restored, original);
  }

  // 场景：role 取值不在三种合法角色之内。
  // 预期：反序列化直接报错，而不是静默丢弃或错误归并。
  #[test]
  fn deserialize_unknown_role_test() {
    let json = serde_json::json!({
      "role": "system",
      "content": "ignored"
    });

    assert!(serde_json::from_value::<Message>(json).is_err());
  }

  // 场景：输入对象缺少 role 字段。
  // 预期：以 missing_field 错误失败。
  #[test]
  fn deserialize_missing_role_test() {
    let json = serde_json::json!({
      "content": "no role here"
    });

    assert!(serde_json::from_value::<Message>(json).is_err());
  }
}
