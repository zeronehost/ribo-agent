//! 用户侧消息模型。

use super::content::Content;

/// 用户（或本地调用方）发出的消息。
///
/// 字段全部公开，可直接以结构体字面量构造；内容形态见 [`UserContent`]，
/// 既支持纯文本也支持多模态内容块数组。该类型同样可从持久化记录中反序列化恢复。
///
/// # Examples
///
/// ```
/// use ra_core::message::{UserContent, UserMessage};
///
/// // 直接构造一条纯文本用户消息
/// let msg = UserMessage {
///   content: UserContent::Text("hello".to_string()),
///   timestamp: 0,
/// };
///
/// // 从提供方/存储层的 JSON 中反序列化，结果与直接构造一致
/// let restored: UserMessage =
///   serde_json::from_value(serde_json::json!({
///     "content": "hello",
///     "timestamp": 0
///   }))
///   .unwrap();
/// assert_eq!(restored, msg);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserMessage {
  /// 用户消息内容，支持纯文本或多内容块两种形态。
  pub content: UserContent,
  /// 消息产生时间，Unix 毫秒时间戳。
  pub timestamp: i64,
}

/// 用户消息的两种内容形态。
///
/// 采用 serde 无标签（untagged）表示：JSON 字符串直接按
/// [`UserContent::Text`] 解析，JSON 数组按 [`UserContent::Blocks`] 解析，
/// 以兼容提供方「纯文本」与「多模态内容块数组」两种请求格式。
///
/// # Examples
///
/// ```
/// use ra_core::message::UserContent;
///
/// let text: UserContent =
///   serde_json::from_value(serde_json::json!("hello")).unwrap();
/// assert!(matches!(text, UserContent::Text(_)));
///
/// let blocks: UserContent =
///   serde_json::from_value(serde_json::json!([])).unwrap();
/// assert!(matches!(blocks, UserContent::Blocks(_)));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum UserContent {
  /// 纯文本形态：一个 JSON 字符串。
  Text(String),
  /// 多内容块形态：一个 [`Content`] 数组。
  Blocks(Vec<Content>),
}
