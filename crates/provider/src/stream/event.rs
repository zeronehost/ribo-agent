//! 流式响应过程中的事件定义。

use std::sync::Arc;

use ra_core::message::{AssistantMessage, StopReason, ToolCall};

/// 提供方流式产出回复过程中的单个事件。
///
/// 事件按「开始 → 若干增量 → 结束」的模式描述三类内容：
/// 思考过程（Reasoning*）、文本（Text*）与工具调用（ToolCall*）；
/// 流首尾另有 [`Start`](Self::Start) 与终结事件 [`Done`](Self::Done) /
/// [`Error`](Self::Error)。
///
/// 每个进行中事件都携带 `partial`：截至当前事件、仍在累积中的
/// [`AssistantMessage`] 快照，调用方可直接用于实时渲染，无需自行拼装。
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderEvent {
  /// 流开始：提供方已接受请求，即将开始产出内容。
  Start {
    /// 初始的助手消息快照。
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
  /// 流正常结束：模型已停止生成，消息为最终收敛结果。
  Done {
    /// 模型给出的停止原因。
    reason: StopReason,
    /// 收敛完成的最终助手消息。
    message: AssistantMessage,
  },
  /// 流因错误结束：消息中通常带有错误描述。
  Error {
    /// 对应的失败停止原因。
    reason: StopReason,
    /// 携带错误信息的最终助手消息。
    message: AssistantMessage,
  },
}

impl ProviderEvent {
  /// 判断事件是否为终结事件。
  ///
  /// 仅 [`Done`](Self::Done) 与 [`Error`](Self::Error) 返回 `true`；
  /// 事件流以此判定何时停止拉取并兑现最终结果。
  pub fn is_terminal(&self) -> bool {
    matches!(self, Self::Done { .. } | Self::Error { .. })
  }
}
