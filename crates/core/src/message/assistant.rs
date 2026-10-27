//! 模型（助手）侧的消息与停止原因模型。

use super::{content::Content, usage::Usage};

/// 模型返回的一条助手消息。
///
/// 除正文 [`content`](Self::content) 外，还携带模型标识、token 用量、停止原因等元信息，
/// 以及流式/延迟响应场景下才出现的可选字段（[`deferred`](Self::deferred)、
/// [`response_id`](Self::response_id) 等）。可选字段在为 `None` 时不参与序列化。
///
/// # Examples
///
/// ```
/// use ra_core::message::AssistantMessage;
///
/// let msg = AssistantMessage::empty("demo-provider", "demo-model", 0);
/// assert!(msg.content.is_empty());
/// assert_eq!(msg.provider, "demo-provider");
/// ```
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
  /// 模型输出的内容块列表，可能同时包含文本、思考过程与工具调用。
  pub content: Vec<Content>,
  /// 实际生成回复的模型标识。
  pub model: String,
  /// 提供方标识，例如服务名或渠道名。
  pub provider: String,
  /// 本次回复的 token 用量与费用统计。
  pub usage: Usage,
  /// 停止原因，用于决定后续是结束轮次还是继续工具调用等流程。
  pub stop_reason: StopReason,
  /// 消息生成时间，Unix 毫秒时间戳。
  pub timestamp: i64,
  /// 提供方实际路由到的模型名（与请求模型名不同时才有值）。
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub response_model: Option<String>,
  /// 提供方为本次回复分配的响应 ID，用于多轮关联或追溯。
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub response_id: Option<String>,
  /// 延迟响应句柄：结果尚未就绪时，凭此句柄后续轮询。
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub deferred: Option<DeferredHandle>,
  /// 提供方返回的错误描述；正常回复时为 `None`。
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub error_message: Option<String>,
  /// 提供方给出的原始停止原因字符串，用于保留无法归入 [`StopReason`] 的取值。
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub raw_stop_reason: Option<String>,
  /// 提供方原生的「是否结束本轮对话」标记；本字段为透传值，语义以
  /// [`stop_reason`](Self::stop_reason) 为准。
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub end_turn: Option<bool>,
}

impl AssistantMessage {
  /// 构造一条「占位」助手消息：内容为空、用量为零、停止原因为
  /// [`StopReason::Pending`]，其余可选字段均为 `None`。
  ///
  /// 适用于流式响应刚开始、尚在等待首个事件时先建立消息骨架的场景。
  ///
  /// # 参数
  ///
  /// - `provider`：提供方标识。
  /// - `model`：模型标识。
  /// - `timestamp`：消息时间戳（Unix 毫秒）。
  ///
  /// # Examples
  ///
  /// ```
  /// use ra_core::message::{AssistantMessage, StopReason};
  ///
  /// let msg = AssistantMessage::empty("demo", "gpt", 0);
  /// assert!(msg.content.is_empty());
  /// assert_eq!(msg.stop_reason, StopReason::Pending);
  /// ```
  pub fn empty(
    provider: impl Into<String>,
    model: impl Into<String>,
    timestamp: i64,
  ) -> Self {
    Self {
      content: Vec::new(),
      model: model.into(),
      provider: provider.into(),
      usage: Usage::zero(),
      stop_reason: StopReason::Pending,
      timestamp,
      response_model: None,
      response_id: None,
      deferred: None,
      error_message: None,
      raw_stop_reason: None,
      end_turn: None,
    }
  }

  /// 构造一条表示「终结性失败」的助手消息：内容为空、用量为零，
  /// 停止原因与错误信息由调用方指定。
  ///
  /// 适用于请求在拿到任何有效内容前就失败（网络错误、鉴权失败等）的场景。
  ///
  /// # 参数
  ///
  /// - `provider`：提供方标识。
  /// - `model`：模型标识。
  /// - `stop_reason`：失败对应的停止原因，通常为 [`StopReason::Error`]。
  /// - `error_message`：错误描述文本。
  /// - `timestamp`：消息时间戳（Unix 毫秒）。
  ///
  /// # Examples
  ///
  /// ```
  /// use ra_core::message::{AssistantMessage, StopReason};
  ///
  /// let msg = AssistantMessage::terminal(
  ///   "demo",
  ///   "gpt",
  ///   StopReason::Error,
  ///   "upstream timeout",
  ///   0,
  /// );
  /// assert_eq!(msg.stop_reason, StopReason::Error);
  /// assert_eq!(msg.error_message.as_deref(), Some("upstream timeout"));
  /// ```
  pub fn terminal(
    provider: impl Into<String>,
    model: impl Into<String>,
    stop_reason: StopReason,
    error_message: impl Into<String>,
    timestamp: i64,
  ) -> Self {
    Self {
      content: Vec::new(),
      model: model.into(),
      provider: provider.into(),
      usage: Usage::zero(),
      stop_reason,
      timestamp,
      response_model: None,
      response_id: None,
      deferred: None,
      error_message: Some(error_message.into()),
      raw_stop_reason: None,
      end_turn: None,
    }
  }
}

/// 模型停止生成的原因。
///
/// 该枚举的 serde 外部表示为蛇形命名（`rename_all = "snake_case"`），
/// 例如 [`StopReason::ToolUse`] 序列化为 `"tool_use"`。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
  /// 尚未结束（流式占位消息的初始状态）。
  Pending,
  /// 模型自然结束输出。
  Stop,
  /// 触达输出长度上限被截断。
  Length,
  /// 模型请求调用工具，对话轮次尚未结束。
  ToolUse,
  /// 生成过程出错。
  Error,
  /// 被用户或本地策略中止。
  Aborted,
  /// 结果尚未生成，需要延迟轮询（见 [`DeferredHandle`]）。
  Deferred,
}
impl StopReason {
  /// 返回与停止原因对应的短名称，用于日志展示或与提供方私有协议做手工映射。
  ///
  /// 名称与 serde 的蛇形外部表示保持一致（例如 [`StopReason::ToolUse`]
  /// 对应 `"tool_use"`）；需要字符串形式但不经过序列化器时可直接调用本方法。
  ///
  /// # Examples
  ///
  /// ```
  /// use ra_core::message::StopReason;
  ///
  /// assert_eq!(StopReason::ToolUse.as_str(), "tool_use");
  /// assert_eq!(StopReason::Aborted.as_str(), "aborted");
  /// ```
  pub fn as_str(self) -> &'static str {
    match self {
      StopReason::Pending => "pending",
      StopReason::Stop => "stop",
      StopReason::Length => "length",
      StopReason::ToolUse => "tool_use",
      StopReason::Error => "error",
      StopReason::Aborted => "aborted",
      StopReason::Deferred => "deferred",
    }
  }
}

/// 延迟响应句柄：提供方暂未生成结果时，凭此信息在后续请求中轮询。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeferredHandle {
  /// 负责兑现该延迟结果的提供方标识。
  pub provider: String,
  /// 负责兑现的模型标识。
  pub model_id: String,
  /// 延迟任务 ID，轮询时需原样回传。
  pub id: String,
  /// 结果过期时间（Unix 毫秒时间戳）；未提供时为 `None`。
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub expires_at: Option<i64>,
  /// 提供方建议的最早轮询间隔（毫秒）；未提供时为 `None`。
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub poll_after_ms: Option<i64>,
  /// 提供方专属的附加轮询数据，结构由对应提供方定义。
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub data: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
  use super::*;

  /// 场景：`StopReason::as_str` 与 serde 的蛇形表示保持一致。
  ///
  /// 预期：每个变体的字符串与序列化结果相同。
  #[test]
  fn stop_reason_as_str_matches_serde() {
    let cases = [
      (StopReason::Pending, "pending"),
      (StopReason::Stop, "stop"),
      (StopReason::Length, "length"),
      (StopReason::ToolUse, "tool_use"),
      (StopReason::Error, "error"),
      (StopReason::Aborted, "aborted"),
      (StopReason::Deferred, "deferred"),
    ];

    for (reason, expected) in cases {
      let serialized = serde_json::to_value(&reason).expect("序列化应成功");
      assert_eq!(serialized, serde_json::json!(expected));
      assert_eq!(reason.as_str(), expected);
    }
  }
}
