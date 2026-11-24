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
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum Message {
  /// 用户发出的消息。
  User(UserMessage),
  /// 模型返回的消息。
  Assistant(AssistantMessage),
  /// 工具执行结果回灌消息。
  ToolResult(ToolResultMessage),
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

/// 判定字符是否属于 CJK 统一表意文字（加权口径：1.0 token/char）。
///
/// 覆盖基本区、扩展 A/B-F 与兼容表意文字；不含日文假名与韩文音节
/// （二者归入"其他 Unicode"档，按 0.5 计）。
fn is_cjk_ideograph(c: char) -> bool {
  matches!(
    c,
    '\u{3400}'..='\u{4DBF}'
      | '\u{4E00}'..='\u{9FFF}'
      | '\u{F900}'..='\u{FAFF}'
      | '\u{20000}'..='\u{2FA1F}'
  )
}

/// 单字符 token 权重，放大 4 倍的定点整数（避免浮点误差）。
///
/// ASCII 1（=0.25）、CJK 表意文字 4（=1.0）、其他 Unicode 2（=0.5）。
fn char_token_weight(c: char) -> usize {
  if c.is_ascii() {
    1
  } else if is_cjk_ideograph(c) {
    4
  } else {
    2
  }
}

/// 按加权字符口径估算任意文本的 token 数（压缩 overhead 复用此口径）。
///
/// 权重：ASCII 0.25、CJK 表意文字 1.0、其他 Unicode（含 Emoji）0.5
/// token/char。内部以放大 4 倍的定点整数累加后统一整除，避免逐字符舍入。
/// 不承诺与真实 tokenizer 对齐——目标是"不系统性低估"。
///
/// # Examples
///
/// ```
/// use ra_core::message::estimate_tokens_from_chars;
///
/// // 8 个 ASCII 字符：8 × 0.25 = 2 token
/// assert_eq!(estimate_tokens_from_chars("abcdefgh"), 2);
/// // 5 个汉字：5 × 1.0 = 5 token
/// assert_eq!(estimate_tokens_from_chars("上下文压缩"), 5);
/// ```
pub fn estimate_tokens_from_chars(text: &str) -> usize {
  text.chars().map(char_token_weight).sum::<usize>() / 4
}

/// 按加权字符口径估算消息序列的 token 占用（唯一真值）。
///
/// 口径：各文本块加权字符数之和 / 4 + 每条消息 4 token 附加；
/// 内联图片与工具调用占位不计（base64 会严重高估）。压缩触发、
/// agent_loop 降级窗口、TUI ctx 占比三处共用此函数。
/// ASCII 场景下与旧口径 `chars/4` 完全等价（定点累加后统一整除）。
pub fn estimate_context_tokens(messages: &[Message]) -> usize {
  fn content_weight(content: &Content) -> usize {
    match content {
      Content::Text(t) => t.content.chars().map(char_token_weight).sum(),
      Content::Thinking(t) => t.content.chars().map(char_token_weight).sum(),
      // 图片与工具调用占位不计（内联图片 base64 会严重高估）
      Content::Image(_) | Content::ToolCall(_) => 0,
    }
  }
  // 各段返回放大 4 倍的定点权重，全局累加后一次整除——与旧 chars/4 对齐
  let mut weighted = 0usize;
  for msg in messages {
    weighted += match msg {
      Message::User(user) => match &user.content {
        UserContent::Text(s) => s.chars().map(char_token_weight).sum::<usize>(),
        UserContent::Blocks(blocks) => {
          blocks.iter().map(content_weight).sum::<usize>()
        }
      },
      Message::Assistant(assistant) => {
        assistant.content.iter().map(content_weight).sum::<usize>()
      }
      Message::ToolResult(result) => {
        result.content.iter().map(content_weight).sum::<usize>()
      }
    };
  }
  weighted / 4 + 4 * messages.len()
}

#[cfg(test)]
mod tests {
  use super::*;

  // 场景：反序列化时 role 为 toolResult。
  // 预期：解析出的消息与完整期望对象相等（逐字段校验分支闭合）。
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
    let expected = Message::ToolResult(ToolResultMessage {
      tool_call_id: "call_1".to_string(),
      tool_name: "search".to_string(),
      is_error: false,
      timestamp: 0,
      content: Vec::new(),
      details: None,
      usage: None,
      added_tool_names: Vec::new(),
    });
    assert_eq!(message, expected);
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

  // 场景：User role 消息先序列化再反序列化。
  // 预期：JSON 中 role 字段按 camelCase 输出为 "user"，往返后消息完全相等。
  #[test]
  fn user_role_roundtrip_test() {
    let original = Message::User(UserMessage {
      content: UserContent::Text("hello".to_string()),
      timestamp: 1700000000000,
    });

    let json = serde_json::to_value(&original).expect("should serialize");
    // role 字段应为 "user"
    assert_eq!(json["role"], "user");

    let restored =
      serde_json::from_value::<Message>(json).expect("should deserialize");
    assert_eq!(restored, original);
  }

  // 场景：Assistant role 消息先序列化再反序列化。
  // 预期：JSON 中 role 字段按 camelCase 输出为 "assistant"，往返后消息完全相等。
  #[test]
  fn assistant_role_roundtrip_test() {
    let original = Message::Assistant(AssistantMessage::empty(
      "demo-provider",
      "demo-model",
      0,
    ));

    let json = serde_json::to_value(&original).expect("should serialize");
    // role 字段应为 "assistant"
    assert_eq!(json["role"], "assistant");

    let restored =
      serde_json::from_value::<Message>(json).expect("should deserialize");
    assert_eq!(restored, original);
  }

  // 场景：三种具体消息类型通过 From 转换为 Message。
  // 预期：分别落到 User / Assistant / ToolResult 分支，便于 map(Message::from)。
  #[test]
  fn from_impls_map_to_expected_variants_test() {
    let user = UserMessage {
      content: UserContent::Text("hi".to_string()),
      timestamp: 0,
    };
    assert_eq!(Message::from(user.clone()), Message::User(user));

    let assistant = AssistantMessage::empty("p1", "m1", 0);
    assert_eq!(
      Message::from(assistant.clone()),
      Message::Assistant(assistant)
    );

    let tool = ToolResultMessage {
      tool_call_id: "call_1".to_string(),
      tool_name: "search".to_string(),
      is_error: false,
      timestamp: 0,
      content: Vec::new(),
      details: None,
      usage: None,
      added_tool_names: Vec::new(),
    };
    assert_eq!(Message::from(tool.clone()), Message::ToolResult(tool));
  }

  // 场景：加权口径——ASCII 0.25 / CJK 1.0 / 其他 0.5（定点 ×4 累加后整除）。
  // 预期：中文不再按 /4 低估（"上下文压缩" 5 汉字 = 5 token，旧口径为 1）。
  #[test]
  fn estimate_tokens_from_chars_weights_categories() {
    assert_eq!(estimate_tokens_from_chars("abcdefgh"), 2); // 8 ASCII / 4
    assert_eq!(estimate_tokens_from_chars("上下文压缩"), 5); // 5 CJK × 1.0
    // 4 个 Emoji（其他档 0.5）：定点 4×2=8，/4 = 2
    assert_eq!(estimate_tokens_from_chars("🙂🙂🙂🙂"), 2);
    // 混合：1 汉字(4) + 4 ASCII(4) = 定点 8 / 4 = 2
    assert_eq!(estimate_tokens_from_chars("中abcd"), 2);
  }

  // 场景：estimate_context_tokens 在 ASCII 场景与旧口径 chars/4 等价，
  // 且每条消息附加 4 token；ToolCall/Image 占位不计入字符。
  // 预期：整段 8 ASCII 字符 → 8/4 + 4×1条 = 2 + 4 = 6。
  #[test]
  fn estimate_context_tokens_ascii_matches_legacy_and_adds_overhead() {
    let messages = vec![Message::User(UserMessage {
      content: UserContent::Text("abcdefgh".to_string()),
      timestamp: 0,
    })];
    assert_eq!(estimate_context_tokens(&messages), 6);
  }

  // 场景：中文消息不再系统性低估（验收标准 2 的边界样本）。
  // 预期：100 个汉字 → 100 token，远大于旧口径的 25。
  #[test]
  fn estimate_context_tokens_cjk_not_underestimated() {
    let text = "中".repeat(100);
    let messages = vec![Message::User(UserMessage {
      content: UserContent::Text(text),
      timestamp: 0,
    })];
    let tokens = estimate_context_tokens(&messages);
    assert_eq!(tokens, 100 + 4); // 100 CJK + 4×1条 消息开销
  }
}
