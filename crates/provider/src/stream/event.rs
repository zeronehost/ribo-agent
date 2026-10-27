//! Provider 事件模型。
//!
//! 每个事件都携带截至该事件的半成品快照 `partial: Arc<AssistantMessage>`，
//! 消费方无须自行聚合增量即可获得最新状态。终结事件（`Done` / `Error`）
//! 携带完整的最终消息（不带 partial，改为顶层 `message` 字段）。

use std::sync::Arc;

use ra_core::message::{AssistantMessage, StopReason, ToolCall};

/// 流式事件。
///
/// 除终结事件外，顺序即语义。事件由网络字节流驱动，生产速率不受本进程控制。
/// 每个事件的 `partial` 字段是 [`Arc<AssistantMessage>`]（克隆廉价），
/// 消费方可以随时取出截至当前的聚合状态。
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderEvent {
  /// 流开始：提供方已接受请求，即将开始产出内容。
  Start {
    /// 初始的助手消息快照（空 content，只有 provider/model 元信息）。
    partial: Arc<AssistantMessage>,
  },
  /// 一段思考过程（推理链）开始。
  ReasoningStart {
    /// 思考段序号，同一序号的 delta/end 属于同一段思考。
    index: usize,
    /// 当前的助手消息快照。
    partial: Arc<AssistantMessage>,
  },
  /// 思考过程的文本增量。
  ReasoningDelta {
    /// 所属思考段序号。
    index: usize,
    /// 本次新增的思考文本片段。
    delta: String,
    /// 当前的助手消息快照。
    partial: Arc<AssistantMessage>,
  },
  /// 一段思考过程结束。
  ReasoningEnd {
    /// 所属思考段序号。
    index: usize,
    /// 该思考段累积得到的完整文本。
    content: String,
    /// 当前的助手消息快照。
    partial: Arc<AssistantMessage>,
  },
  /// 一段正式文本输出开始。
  TextStart {
    /// 文本段序号，同一序号的 delta/end 属于同一段文本。
    index: usize,
    /// 当前的助手消息快照。
    partial: Arc<AssistantMessage>,
  },
  /// 正式文本的增量。
  TextDelta {
    /// 所属文本段序号。
    index: usize,
    /// 本次新增的文本片段。
    delta: String,
    /// 当前的助手消息快照。
    partial: Arc<AssistantMessage>,
  },
  /// 一段正式文本输出结束。
  TextEnd {
    /// 所属文本段序号。
    index: usize,
    /// 该文本段累积得到的完整内容。
    content: String,
    /// 当前的助手消息快照。
    partial: Arc<AssistantMessage>,
  },
  /// 一次工具调用的参数输出开始。
  ToolCallStart {
    /// 工具调用序号，同一序号的 delta/end 属于同一次调用。
    index: usize,
    /// 当前的助手消息快照。
    partial: Arc<AssistantMessage>,
  },
  /// 工具调用参数的增量（参数通常以 JSON 片段形式流式到达）。
  ToolCallDelta {
    /// 所属工具调用序号。
    index: usize,
    /// 本次新增的参数字符串片段。
    delta: String,
    /// 当前的助手消息快照。
    partial: Arc<AssistantMessage>,
  },
  /// 一次工具调用的参数输出结束，参数已可解析。
  ToolCallEnd {
    /// 所属工具调用序号。
    index: usize,
    /// 累积解析完成的完整工具调用。
    tool_call: ToolCall,
    /// 当前的助手消息快照。
    partial: Arc<AssistantMessage>,
  },
  /// 流正常结束（终结事件）：模型已停止生成，消息为最终收敛结果。
  Done {
    /// 模型给出的停止原因。
    reason: StopReason,
    /// 收敛完成的最终助手消息。
    message: AssistantMessage,
  },
  /// 流因错误结束（终结事件）：消息中通常带有错误描述。
  Error {
    /// 对应的失败停止原因（`StopReason::Error` 或 `StopReason::Aborted`）。
    reason: StopReason,
    /// 携带错误信息的最终助手消息（人类可读文本见 `message.error_message`）。
    message: AssistantMessage,
    /// 结构化错误码（取自 `crate::error` 的错误码常量）；
    /// 提供方无法归类时为 `None`。
    ///
    /// 供上层区分处置路径：例如 `TIMEOUT` / `TRANSPORT` 提示重试，
    /// `CONTENT_FILTERED` 提示改写输入，`CANCELLED` 不视为故障。
    code: Option<&'static str>,
  },
}

impl ProviderEvent {
  /// 判断是否为终结事件（`Done` 或 `Error`）。
  ///
  /// 终结事件出现后，后续再调用 `next` 一律返回 `None`。
  pub fn is_terminal(&self) -> bool {
    matches!(self, Self::Done { .. } | Self::Error { .. })
  }

  /// 判断是否为内容事件（推理/文本/工具调用开始、增量、结束）。
  ///
  /// `Start` 事件不算内容——这对 TTFT 守卫至关重要：
  /// "从建流成功到首内容事件的间隔"不能把 `Start` 算进去。
  pub fn is_content(&self) -> bool {
    matches!(
      self,
      Self::ReasoningStart { .. }
        | Self::ReasoningDelta { .. }
        | Self::ReasoningEnd { .. }
        | Self::TextStart { .. }
        | Self::TextDelta { .. }
        | Self::TextEnd { .. }
        | Self::ToolCallStart { .. }
        | Self::ToolCallDelta { .. }
        | Self::ToolCallEnd { .. }
    )
  }

  /// 取事件携带的半成品快照。
  ///
  /// 终结事件（`Done` / `Error`）不带 partial，返回 `None`；
  /// 其余事件一律返回 `Some`。
  pub fn partial(&self) -> Option<&Arc<AssistantMessage>> {
    match self {
      Self::Start { partial }
      | Self::ReasoningStart { partial, .. }
      | Self::ReasoningDelta { partial, .. }
      | Self::ReasoningEnd { partial, .. }
      | Self::TextStart { partial, .. }
      | Self::TextDelta { partial, .. }
      | Self::TextEnd { partial, .. }
      | Self::ToolCallStart { partial, .. }
      | Self::ToolCallDelta { partial, .. }
      | Self::ToolCallEnd { partial, .. } => Some(partial),
      _ => None,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn partial() -> Arc<AssistantMessage> {
    Arc::new(AssistantMessage::empty("p1", "m1", 0))
  }

  fn tool_call() -> ToolCall {
    ToolCall {
      id: "call-1".to_string(),
      name: "lookup".to_string(),
      arguments: serde_json::json!({ "q": "rust" }),
      signature: None,
      namespace: None,
    }
  }

  /// 场景：除 `Start` 外的非终结事件都属于内容事件，且携带半成品快照。
  #[test]
  fn content_events_carry_partial_snapshot() {
    let events = vec![
      ProviderEvent::ReasoningStart {
        index: 0,
        partial: partial(),
      },
      ProviderEvent::ReasoningDelta {
        index: 0,
        delta: "思考".to_string(),
        partial: partial(),
      },
      ProviderEvent::ReasoningEnd {
        index: 0,
        content: "思考".to_string(),
        partial: partial(),
      },
      ProviderEvent::TextStart {
        index: 0,
        partial: partial(),
      },
      ProviderEvent::TextDelta {
        index: 0,
        delta: "hi".to_string(),
        partial: partial(),
      },
      ProviderEvent::TextEnd {
        index: 0,
        content: "hi".to_string(),
        partial: partial(),
      },
      ProviderEvent::ToolCallStart {
        index: 0,
        partial: partial(),
      },
      ProviderEvent::ToolCallDelta {
        index: 0,
        delta: "{}".to_string(),
        partial: partial(),
      },
      ProviderEvent::ToolCallEnd {
        index: 0,
        tool_call: tool_call(),
        partial: partial(),
      },
    ];

    for event in events {
      assert!(!event.is_terminal());
      assert!(event.is_content());
      assert!(event.partial().is_some());
    }
  }

  /// 场景：`Start` 携带快照但不算内容事件（TTFT 统计必须跳过它）。
  #[test]
  fn start_is_not_content_but_carries_partial() {
    let event = ProviderEvent::Start { partial: partial() };

    assert!(!event.is_terminal());
    assert!(!event.is_content());
    assert!(event.partial().is_some());
  }

  /// 场景：终结事件不可再当内容消费，且不带半成品快照。
  #[test]
  fn terminal_events_have_no_partial() {
    let done = ProviderEvent::Done {
      reason: StopReason::Stop,
      message: AssistantMessage::empty("p1", "m1", 0),
    };
    let error = ProviderEvent::Error {
      reason: StopReason::Error,
      message: AssistantMessage::empty("p1", "m1", 0),
      code: Some(crate::error::STREAM_INTERRUPTED),
    };

    for event in [done, error] {
      assert!(event.is_terminal());
      assert!(!event.is_content());
      assert!(event.partial().is_none());
    }
  }
}
