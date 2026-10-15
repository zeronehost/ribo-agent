//! 消息内容块模型：一条消息可以由一个或多个不同类型的内容块组成。

use super::tool::ToolCall;

/// 消息中的单个内容块。
///
/// 序列化时以 `type` 字段区分具体类型（camelCase 命名，如 `"toolCall"`），
/// 因此一段内容数组可以混合承载文本、图片、思考过程与工具调用。
///
/// # Examples
///
/// ```
/// use ra_core::message::Content;
///
/// let content: Content =
///   serde_json::from_value(serde_json::json!({
///     "type": "text",
///     "content": "hello"
/// }))
/// .unwrap();
/// assert!(matches!(content, Content::Text(_)));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Content {
  /// 模型的思考过程（推理链），部分提供方会附带用于校验的签名。
  Thinking(ThinkingContent),
  /// 普通文本内容。
  Text(TextContent),
  /// 图片内容，以内联数据形式承载。
  Image(ImageContent),
  /// 模型发起的工具调用请求。
  ToolCall(ToolCall),
}

/// 思考过程内容块，对应模型的推理链输出。
///
/// 部分提供方会返回 `signature`，在多轮对话中回传思考块时应原样携带，
/// 供提供方校验思考内容未被篡改。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ThinkingContent {
  /// 思考过程的文本内容。
  pub content: String,
  /// 提供方签发的思考内容签名；无签名能力的提供方为 `None`。
  pub signature: Option<String>,
  /// 该思考块是否因安全策略被编辑/脱敏；为 `false` 时不参与序列化。
  #[serde(default, skip_serializing_if = "crate::utils::serde_fn::is_false")]
  pub redacted: bool,
}

/// 文本内容块。
///
/// `signature` 用于少数对文本输出也做签名校验的提供方，普通场景下为 `None`。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TextContent {
  /// 文本正文。
  pub content: String,
  /// 可选的输出签名。
  pub signature: Option<String>,
}

/// 图片内容块，图片以 base64 数据内联承载。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageContent {
  /// base64 编码的图片数据（不含 `data:` 前缀之外的 MIME 头部分）。
  pub data: String,
  /// 图片 MIME 类型，例如 `"image/png"`。
  pub mime_type: String,
}
