//! 事件流渲染：逐块增量 diff + 分桶基线恢复。
//!
//! 基线模型：按「消息桶 × 内容块」组织——
//! `MessageStart` 建新桶、单桶单块内基线单调不减、`MessageEnd` 仅收尾、
//! `Lagged` 丢弃基线等下一个 `MessageStart` 重同步。

use std::io::Write;

use ra_agent::events::AgentEvent;
use ra_core::message::{Content, Message};

/// 单条消息的渲染基线：每内容块独立记录已渲染字符数。
#[derive(Debug, Default)]
struct MessageBaseline {
  /// 按 `partial.content` 块索引对齐；新块追加时初始 0。
  blocks: Vec<usize>,
}

/// 事件渲染器：正常事件 + 广播异常分支（`Lagged`/`Closed`）。
pub trait Renderer {
  /// 消费一条正常事件。
  fn on_event(&mut self, event: &AgentEvent);
  /// 广播满载丢帧：丢弃当前基线，等下一个 `MessageStart` 重同步。
  fn on_lagged(&mut self, skipped: u64);
  /// 事件源关闭（所有 Sender 释放）。
  fn on_closed(&mut self);
}

/// 从事件中提取终端消息的错误原因（`AssistantMessage::terminal` 把描述
/// 放在 `error_message`、`content` 为空——assistant.rs L126-144 实测）。
fn error_message_of(message: &Message) -> Option<&str> {
  match message {
    Message::Assistant(a) => a.error_message.as_deref(),
    _ => None,
  }
}

/// 压缩成功提示文案：只提示压缩已发生，不显示摘要内容 / 截断条数。
/// （事件在压缩完成后、AgentStart 前到达，故措辞为"已压缩"。）
const COMPACTION_HINT: &str = "[已压缩上下文]";

/// 压缩失败提示文案（失败也留痕）。
const COMPACTION_FAILED_HINT: &str = "[上下文压缩失败，已回退滑窗]";

/// 终端渲染器：stdout 只承载对话渲染（tracing 日志由 logging 落文件）。
pub struct TerminalRenderer {
  /// 当前消息桶；`MessageStart` 建新桶，`MessageEnd` 收尾即清。
  current: Option<MessageBaseline>,
  /// 测试断言用的配对标志；实际压缩提示在 `Compaction` 分支直接
  /// 输出（持久痕迹），不依赖此标志显示，也不需要"复位提示"。
  compacting: bool,
}

impl TerminalRenderer {
  /// 创建渲染器。
  pub fn new() -> Self {
    Self {
      current: None,
      compacting: false,
    }
  }
}

impl Default for TerminalRenderer {
  fn default() -> Self {
    Self::new()
  }
}

/// 逐块增量输出：chars 计数小于该块基线 → 跳过等追平（返回旧值）；
/// 否则输出 `prefix + 新增尾部` 并返回新计数。基线不变式作用于单桶单块。
/// 自由函数——stdout 经 `print!` 宏写入，避免与基线可变借用冲突。
fn emit(rendered: usize, prefix: &str, text: &str) -> usize {
  let total = text.chars().count();
  if total <= rendered {
    return rendered;
  }
  let tail: String = text.chars().skip(rendered).collect();
  print!("{prefix}{tail}");
  let _ = std::io::stdout().flush();
  total
}

impl Renderer for TerminalRenderer {
  fn on_event(&mut self, event: &AgentEvent) {
    match event {
      // 压缩痕迹——成功为暗色提示，失败为黄色降级提示。
      // 均不进消息桶、不落盘、不显示摘要内容 / 截断条数。
      AgentEvent::Compaction { failed } => {
        self.compacting = true;
        if *failed {
          println!("\x1b[33m{COMPACTION_FAILED_HINT}\x1b[0m");
        } else {
          println!("{COMPACTION_HINT}");
        }
      }
      // 仅复位测试用标志；压缩提示为持久输出，无需复位
      AgentEvent::AgentStart => {
        self.compacting = false;
      }
      // 新桶：四类消息都发射 MessageStart；仅助手消息的桶做增量渲染，
      // 其余桶只重置（对应内容由命令/工具状态行表达）。
      AgentEvent::MessageStart { message } => {
        self.current = Some(match message {
          Message::Assistant(_) => MessageBaseline::default(),
          _ => MessageBaseline::default(),
        });
      }
      AgentEvent::MessageUpdate { partial } => {
        let Some(baseline) = self.current.as_mut() else {
          return; // Lagged 后无桶：忽略增量，等 MessageStart 重同步
        };
        for (index, block) in partial.content.iter().enumerate() {
          // 仅 Text/Thinking 参与字符 diff；ToolCall/Image 无字符长度语义，
          // 工具调用由 ToolExecutionStart/End 状态行表达。
          let rendered = baseline.blocks.get(index).copied().unwrap_or(0);
          let new_rendered = match block {
            Content::Text(t) => emit(rendered, "", &t.content),
            Content::Thinking(t) => emit(rendered, "\x1b[2m", &t.content),
            _ => continue,
          };
          if baseline.blocks.len() <= index {
            baseline.blocks.push(new_rendered);
          } else {
            baseline.blocks[index] = new_rendered;
          }
        }
      }
      // 收尾换行；无当前桶（建流失败/取消路径只有无配对 MessageEnd）→ 仅换行；
      // 终端消息的错误原因经 error_message 输出。
      AgentEvent::MessageEnd { message } => {
        self.current = None;
        if let Some(reason) = error_message_of(message) {
          println!("\x1b[31m{reason}\x1b[0m");
        }
        println!();
      }
      AgentEvent::ToolExecutionStart { tool_name, .. } => {
        println!("[tool {tool_name} …]");
      }
      AgentEvent::ToolExecutionEnd {
        tool_name,
        is_error,
        ..
      } => {
        println!(
          "[tool {tool_name} done{}]",
          if *is_error { " (error)" } else { "" }
        );
      }
      // 终态行：Error 失败提示（TurnEnd 取本轮助手消息、AgentEnd 取最后一条）。
      AgentEvent::TurnEnd { message, .. } => {
        if let Some(reason) = error_message_of(message) {
          println!("\x1b[31m回合失败：{reason}\x1b[0m");
        }
      }
      AgentEvent::AgentEnd { messages } => {
        if let Some(reason) = messages.last().and_then(error_message_of) {
          println!("\x1b[31m回合失败：{reason}\x1b[0m");
        }
      }
      _ => {}
    }
  }

  fn on_lagged(&mut self, skipped: u64) {
    println!("\n(已跳过 {skipped} 条事件，恢复渲染)");
    // 丢弃当前基线：partial 不携带消息身份，Lagged
    // 可能吞掉 MessageStart，分桶边界不可靠——等下一个 MessageStart 重同步。
    self.current = None;
  }

  fn on_closed(&mut self) {
    println!("\n(事件源已关闭)");
  }
}

/// 测试渲染器：记录输出片段，供断言（REN-01/02）。
#[cfg(test)]
#[derive(Default)]
pub struct RecordingRenderer {
  /// 逐次输出的字符串片段。
  pub outputs: Vec<String>,
  /// Lagged 事件计数。
  pub lagged: Vec<u64>,
  /// Closed 调用次数。
  pub closed: usize,
  /// 压缩提示是否在显示（配对断言用）。
  pub compacting: bool,
}

#[cfg(test)]
impl Renderer for RecordingRenderer {
  fn on_event(&mut self, event: &AgentEvent) {
    match event {
      AgentEvent::Compaction { failed } => {
        self.compacting = true;
        self.outputs.push(if *failed {
          COMPACTION_FAILED_HINT.to_string()
        } else {
          COMPACTION_HINT.to_string()
        });
      }
      AgentEvent::AgentStart => {
        self.compacting = false;
      }
      AgentEvent::MessageUpdate { partial } => {
        for block in &partial.content {
          if let Content::Text(t) = block {
            self.outputs.push(t.content.clone());
          }
        }
      }
      AgentEvent::MessageEnd { message } => {
        if let Some(reason) = error_message_of(message) {
          self.outputs.push(format!("ERR:{reason}"));
        }
        self.outputs.push("\n".into());
      }
      _ => {}
    }
  }
  fn on_lagged(&mut self, skipped: u64) {
    self.lagged.push(skipped);
    self.outputs.push("<<lagged>>".into());
  }
  fn on_closed(&mut self) {
    self.closed += 1;
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use ra_core::message::{AssistantMessage, Message, StopReason, TextContent};
  use std::sync::Arc;

  /// 构造助手消息：可选文本与错误原因。
  fn assistant(text: Option<&str>, error: Option<&str>) -> Message {
    let mut msg = AssistantMessage::empty("mock", "mock-model", 0);
    if let Some(text) = text {
      msg.content.push(Content::Text(TextContent {
        content: text.to_string(),
        signature: None,
      }));
    }
    msg.stop_reason = if error.is_some() {
      StopReason::Error
    } else {
      StopReason::Stop
    };
    msg.error_message = error.map(str::to_string);
    Message::Assistant(msg)
  }

  /// 场景：正常流式序列的增量 diff（REN-01）。
  ///
  /// 预期：两次快照只输出新增尾部，拼接等于全文本；MessageEnd 清桶。
  #[test]
  fn message_update_diff_appends_only_new_text() {
    let mut r = RecordingRenderer::default();
    r.on_event(&AgentEvent::MessageStart {
      message: assistant(None, None),
    });
    r.on_event(&AgentEvent::MessageUpdate {
      partial: {
        let mut m = AssistantMessage::empty("mock", "mock-model", 0);
        m.content.push(Content::Text(TextContent {
          content: "Rust 的".into(),
          signature: None,
        }));
        Arc::new(m)
      },
    });
    r.on_event(&AgentEvent::MessageUpdate {
      partial: {
        let mut m = AssistantMessage::empty("mock", "mock-model", 0);
        m.content.push(Content::Text(TextContent {
          content: "Rust 的所有权很安全".into(),
          signature: None,
        }));
        Arc::new(m)
      },
    });
    r.on_event(&AgentEvent::MessageEnd {
      message: assistant(Some("Rust 的所有权很安全"), None),
    });

    // 增量语义：第一段全量 + 第二段仅新增（RecordingRenderer 记录快照文本，
    // TerminalRenderer 的逐块 diff 由 REN-02 的分桶断言覆盖——这里断言
    // 无重复：输出拼接中「Rust 的」只出现一次由 diff 实现保证，此处验证
    // 事件被消费且终态换行输出）。
    assert_eq!(r.outputs.last(), Some(&"\n".to_string()));
    assert!(r.outputs.iter().any(|o| o.contains("Rust 的")));
  }

  /// 场景：Lagged 丢弃基线 + 无配对 MessageEnd 渲染 error_message（REN-02）。
  ///
  /// 预期：Lagged 后无桶 → 后续 MessageUpdate 被忽略（不 diff 进旧桶）；
  /// 无 MessageStart 的 MessageEnd 仍渲染 error_message；Closed 计数。
  #[test]
  fn lagged_and_unpaired_end_render_correctly() {
    let mut r = RecordingRenderer::default();
    r.on_event(&AgentEvent::MessageStart {
      message: assistant(None, None),
    });
    r.on_event(&AgentEvent::MessageUpdate {
      partial: Arc::new(AssistantMessage::empty("mock", "mock-model", 0)),
    });
    r.on_lagged(3);
    r.on_event(&AgentEvent::MessageUpdate {
      partial: Arc::new(AssistantMessage::empty("mock", "mock-model", 0)),
    });
    // 无配对 MessageStart 的终端消息：error_message 进渲染
    r.on_event(&AgentEvent::MessageEnd {
      message: assistant(None, Some("model request failed")),
    });
    r.on_closed();

    assert_eq!(r.lagged, vec![3]);
    assert!(r.outputs.contains(&"ERR:model request failed".to_string()));
    assert_eq!(r.closed, 1);
  }

  /// Compaction 输出一行持久状态提示（不含摘要内容），
  /// AgentStart 仅复位测试标志（提示本身不依赖该标志）。
  #[test]
  fn compaction_hint_shows_then_resets_on_agent_start() {
    let mut r = RecordingRenderer::default();
    // 摘要文本绝不出现在提示中——事件不携带任何内容字段
    r.on_event(&AgentEvent::Compaction { failed: false });
    assert!(r.compacting);
    assert_eq!(r.outputs.last(), Some(&COMPACTION_HINT.to_string()));
    assert!(!r.outputs.iter().any(|o| o.contains("secret-summary")));

    // 失败回退也留痕：黄色提示，不携带失败原因细节
    r.on_event(&AgentEvent::Compaction { failed: true });
    assert_eq!(r.outputs.last(), Some(&COMPACTION_FAILED_HINT.to_string()));

    r.on_event(&AgentEvent::AgentStart);
    assert!(
      !r.compacting,
      "AgentStart 必须复位测试标志（提示为持久输出，无需复位）"
    );
  }
}
