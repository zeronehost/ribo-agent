//! Agent 循环生命周期事件（D6，对齐 pi-rust 全变体）。
//!
//! 事件为 `Debug + Clone` 载荷，不参与序列化（会话持久化存
//! `Message`，不存事件）；广播订阅方各自持副本，大载荷以 [`Arc`]
//! 包裹（克隆廉价）。

use std::sync::Arc;

use ra_core::message::{AssistantMessage, Message, ToolResultMessage};
use ra_tools::types::ToolExecutionResult;

/// 事件载荷：工具执行结果（转义自 ra-tools `ToolExecutionResult`）。
pub type SharedToolResult = Arc<ToolExecutionResult>;

/// Agent 循环生命周期事件。
///
/// 发射顺序（正常完成回合）：
/// `Compaction? → AgentStart → TurnStart → MessageStart/End(prompt) →
/// MessageUpdate ×n → MessageEnd(assistant) → ToolExecutionStart ×n →
/// ToolExecutionEnd ×n → MessageStart/End(tool_result) ×n → TurnEnd → AgentEnd`。
///
/// 变体顺序即生命周期顺序（Compaction 最先且可选、AgentEnd 最后）。
/// `Compaction` 在 `AgentStart` 前发射；提示是对话流中的持久痕迹
///（TUI 气泡 / CLI 一行），`AgentStart` 不复位、痕迹保留。
#[derive(Debug, Clone)]
pub enum AgentEvent {
  /// 本轮执行了上下文压缩（`failed` 区分成功摘要 / 失败回退；不携带摘要内容）。
  ///
  /// 压缩在 `ContextProvider::build` 内完成（早于本事件），事件于
  /// `AgentStart` 前发射；显示端把它转化为对话流中的持久痕迹
  ///（CLI 单行提示 / TUI 系统气泡）。`failed == true` 表示摘要调用失败、
  /// 本轮已回退滑窗（v1.6 用户拍板：失败也留痕，让用户知道发生过什么）；
  /// 冷却抑制 / 复用摘要仍不发射。
  Compaction {
    /// 是否失败回退（`false` = 摘要成功）。
    failed: bool,
  },
  /// 一次 `run_agent_loop` 开始（TurnStart 之前发射一次）。
  AgentStart,
  /// 每轮开始（一轮 = 一次助手响应 + 其工具批）。
  TurnStart,
  /// 任一消息（prompt / 注入 / 助手 / 工具结果）进入转录时。
  MessageStart {
    /// 进入转录的消息。
    message: Message,
  },
  /// 仅助手消息：流式增量（携带 ra-provider 聚合快照）。
  MessageUpdate {
    /// 截至当前事件的助手消息聚合快照。
    partial: Arc<AssistantMessage>,
  },
  /// 消息完成（`MessageStart` 的补事件）。
  MessageEnd {
    /// 最终消息。
    message: Message,
  },
  /// 工具调用开始执行。
  ToolExecutionStart {
    /// 对应的 `ToolCall::id`。
    tool_call_id: String,
    /// 被调用的工具名。
    tool_name: String,
    /// 模型生成的调用参数。
    args: serde_json::Value,
  },
  /// 工具部分结果（预留：工具 `on_update` 通道实现后触发，本期不发射）。
  ToolExecutionUpdate {
    /// 对应的 `ToolCall::id`。
    tool_call_id: String,
    /// 被调用的工具名。
    tool_name: String,
    /// 模型生成的调用参数。
    args: serde_json::Value,
    /// 截至当前的部分执行结果。
    partial_result: SharedToolResult,
  },
  /// 工具调用结束（成功或失败，`is_error` 区分）。
  ///
  /// 并行批中按**完成序**发射；回灌消息按**源顺序**发射（二者解耦）。
  ToolExecutionEnd {
    /// 对应的 `ToolCall::id`。
    tool_call_id: String,
    /// 被调用的工具名。
    tool_name: String,
    /// 执行结果（错误时折叠为含错误文本的结果）。
    result: SharedToolResult,
    /// 是否失败（错误回灌路径为 `true`）。
    is_error: bool,
  },
  /// 每轮结束：本轮助手消息 + 按源顺序排列的工具结果。
  TurnEnd {
    /// 本轮助手消息。
    message: Box<Message>,
    /// 本轮工具结果（源顺序；无工具调用时为空）。
    tool_results: Vec<ToolResultMessage>,
  },
  /// 一次运行结束，携带本次新增消息（转录序）。
  AgentEnd {
    /// 本次运行新增的全部消息。
    messages: Vec<Message>,
  },
}

impl AgentEvent {
  /// 事件类型标签（snake_case，供日志、测试断言与未来序列化使用）。
  pub fn type_tag(&self) -> &'static str {
    match self {
      Self::Compaction { .. } => "compaction",
      Self::AgentStart => "agent_start",
      Self::TurnStart => "turn_start",
      Self::MessageStart { .. } => "message_start",
      Self::MessageUpdate { .. } => "message_update",
      Self::MessageEnd { .. } => "message_end",
      Self::ToolExecutionStart { .. } => "tool_execution_start",
      Self::ToolExecutionUpdate { .. } => "tool_execution_update",
      Self::ToolExecutionEnd { .. } => "tool_execution_end",
      Self::TurnEnd { .. } => "turn_end",
      Self::AgentEnd { .. } => "agent_end",
    }
  }

  /// 是否为终态事件（仅 `AgentEnd`；一次运行恰好发射一次）。
  pub fn is_terminal(&self) -> bool {
    matches!(self, Self::AgentEnd { .. })
  }
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use ra_core::message::{
    AssistantMessage, Content, Message, StopReason, TextContent, ToolCall,
    ToolResultMessage, UserContent, UserMessage,
  };
  use ra_tools::types::ToolExecutionResult;

  use super::*;

  /// 构造一条固定时间戳的用户消息（样本构造用）。
  fn sample_user_message() -> Message {
    Message::from(UserMessage {
      content: UserContent::Text("hi".to_string()),
      timestamp: 42,
    })
  }

  /// 构造一条单工具调用的助手消息样本。
  fn sample_assistant_message() -> AssistantMessage {
    let mut message = AssistantMessage::empty("mock", "mock-model", 0);
    message.content = vec![Content::ToolCall(ToolCall {
      id: "c1".to_string(),
      name: "echo".to_string(),
      arguments: serde_json::json!({}),
      signature: None,
      namespace: None,
    })];
    message.stop_reason = StopReason::ToolUse;
    message
  }

  /// 构造一条工具结果消息样本。
  fn sample_tool_result() -> ToolResultMessage {
    ToolResultMessage {
      tool_call_id: "c1".to_string(),
      tool_name: "echo".to_string(),
      is_error: false,
      timestamp: 42,
      content: vec![Content::Text(TextContent {
        content: "ok".to_string(),
        signature: None,
      })],
      details: None,
      usage: None,
      added_tool_names: Vec::new(),
    }
  }

  /// 按生命周期顺序构造全部 11 个变体样本。
  fn all_variants() -> Vec<AgentEvent> {
    let user_message = sample_user_message();
    vec![
      AgentEvent::Compaction { failed: false },
      AgentEvent::AgentStart,
      AgentEvent::TurnStart,
      AgentEvent::MessageStart {
        message: user_message.clone(),
      },
      AgentEvent::MessageUpdate {
        partial: Arc::new(sample_assistant_message()),
      },
      AgentEvent::MessageEnd {
        message: user_message,
      },
      AgentEvent::ToolExecutionStart {
        tool_call_id: "c1".to_string(),
        tool_name: "echo".to_string(),
        args: serde_json::json!({}),
      },
      AgentEvent::ToolExecutionUpdate {
        tool_call_id: "c1".to_string(),
        tool_name: "echo".to_string(),
        args: serde_json::json!({}),
        partial_result: Arc::new(ToolExecutionResult::default()),
      },
      AgentEvent::ToolExecutionEnd {
        tool_call_id: "c1".to_string(),
        tool_name: "echo".to_string(),
        result: Arc::new(ToolExecutionResult::default()),
        is_error: false,
      },
      AgentEvent::TurnEnd {
        message: Box::new(Message::ToolResult(sample_tool_result())),
        tool_results: vec![sample_tool_result()],
      },
      AgentEvent::AgentEnd {
        messages: Vec::new(),
      },
    ]
  }

  // 场景：type_tag 覆盖全部 11 个变体。
  // 预期：每个变体的标签与生命周期 snake_case 命名一一对应。
  #[test]
  fn type_tag_covers_all_variants() {
    let expected = [
      "compaction",
      "agent_start",
      "turn_start",
      "message_start",
      "message_update",
      "message_end",
      "tool_execution_start",
      "tool_execution_update",
      "tool_execution_end",
      "turn_end",
      "agent_end",
    ];
    let events = all_variants();

    assert_eq!(events.len(), expected.len(), "样本应覆盖全部变体");
    for (event, tag) in events.iter().zip(expected) {
      assert_eq!(event.type_tag(), tag);
    }
  }

  // 场景：is_terminal 仅对 AgentEnd 为真。
  // 预期：其余 9 个事件均非终态（一次运行恰好一次 AgentEnd）。
  #[test]
  fn is_terminal_only_for_agent_end() {
    for event in all_variants() {
      let terminal = event.is_terminal();
      assert_eq!(
        terminal,
        event.type_tag() == "agent_end",
        "变体 {} 的终态判定错误",
        event.type_tag()
      );
    }
  }
}
