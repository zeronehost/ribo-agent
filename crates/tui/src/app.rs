//! TUI 主循环：`tokio::select!` 三源批处理 + AppState 事件映射 +
//! Ctrl-C 生命周期（16 详细设计 §2.1 / §5 / §8）。
//!
//! `AppState`（含 `RefCell<TextArea>`，非 `Sync`）由本任务独占持有
//!（R-R4）；键盘 task 只投喂 `AppInput`，不触碰状态树。

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ra_agent::events::AgentEvent;
use ra_cli::commands::{Command, parse_line};
use ra_cli::config::assemble::CliSession;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use crate::input::keybindings::{self, Action};
use crate::input::{AppInput, keyboard};
use crate::lifecycle;
use crate::render_helpers;
use crate::state::{AppState, ChatBubbleBody, ToolPhase, ToolRun, TurnState};
use crate::turn as turn_ctl;
use crate::view::RootLayout;
use crate::view::input as input_view;

type StdTerminal =
  ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>;

/// TUI 应用：状态树 + 会话门面 + 输入通道 + 视图根布局。
pub struct App {
  /// 集中式状态树（主循环独占）。
  pub state: AppState,
  /// 会话门面（harness / skills / 事件订阅源）。
  session: CliSession,
  /// 键盘事件接收端（键盘 task 投喂）。
  key_rx: mpsc::Receiver<AppInput>,
  /// 视图根布局（无状态渲染）。
  layout: RootLayout,
  /// 绘制计数（测试观测：一轮至多一次 draw）。
  pub draws: usize,
}

impl App {
  /// 构造应用（`key_rx` 为 [`keyboard::start_keyboard`] 的对端）。
  pub fn new(session: CliSession, key_rx: mpsc::Receiver<AppInput>) -> Self {
    let state = AppState::new(
      session.mode,
      session.warnings.clone(),
      session.default_key.clone(),
    );
    Self {
      state,
      session,
      key_rx,
      layout: RootLayout::new(),
      draws: 0,
    }
  }

  /// 主循环：三源 select → 批处理 → 至多一次 draw（16 §2.1）。
  ///
  /// 退出：`exiting` 置位（Ctrl-C / `/exit` / 事件源关闭 / 键盘 task
  /// 死亡）后跳出循环进入 [`App::exit_flow`]——该函数**总是**
  /// `process::exit`，本函数正常不会返回。
  ///
  /// # Errors
  ///
  /// 终端绘制失败时返回错误（由 panic hook / guard 兜底恢复终端）。
  pub async fn run(mut self, mut terminal: StdTerminal) -> anyhow::Result<()> {
    self.draw(&mut terminal)?;
    loop {
      tokio::select! {
        input = self.key_rx.recv() => match input {
          // 键盘 task 死亡（终端关闭）：转入退出流程
          None => self.state.exiting = true,
          Some(AppInput::Key(key)) => self.on_key(key).await,
          Some(AppInput::Resize(w, h)) => self.on_resize(w, h),
        },
        event = self.session.events.recv() => match event {
          Ok(event) => {
            // 批处理：把当前已排队的全部事件一次性应用（16 §2.1）
            self.apply_event(&event);
            while let Ok(next) = self.session.events.try_recv() {
              self.apply_event(&next);
            }
          }
          Err(broadcast::error::RecvError::Lagged(n)) => self.on_lagged(n),
          Err(broadcast::error::RecvError::Closed) => self.state.exiting = true,
        },
      }
      if self.state.exiting {
        break;
      }
      self.draw(&mut terminal)?;
    }
    self.exit_flow().await;
    unreachable!("exit_flow 总以 process::exit 结束");
  }

  /// 绘制一帧：刷新行数缓存（按当前宽度）→ 布局渲染。
  fn draw<B: ratatui::backend::Backend>(
    &mut self,
    terminal: &mut ratatui::Terminal<B>,
  ) -> anyhow::Result<()> {
    let width = terminal
      .size()
      .map(|s| s.width)
      .unwrap_or(render_helpers::FALLBACK_WIDTH);
    render_helpers::refresh_line_cache(&mut self.state, width);
    let Self { state, layout, .. } = self;
    terminal.draw(|frame| layout.render(frame, state))?;
    self.draws += 1;
    Ok(())
  }

  /// 键盘分派：先查键位表，未命中交给编辑器（16 §7.2）。
  async fn on_key(&mut self, key: KeyEvent) {
    let Some(action) = keybindings::map_key(&key) else {
      self
        .state
        .editor
        .borrow_mut()
        .input(tui_textarea::Input::from(key));
      return;
    };
    match action {
      Action::Submit => self.on_submit().await,
      Action::Complete => self.on_complete(),
      // Ctrl-C：exit_flow 统一处理 stop / 等待落盘 / 超时强退
      Action::Exit => self.state.exiting = true,
      Action::ScrollUp => self.scroll_by(-1),
      Action::ScrollDown => self.scroll_by(1),
      Action::PageUp => self.scroll_page(-1),
      Action::PageDown => self.scroll_page(1),
      Action::Cancel => {
        if self.state.help_open || self.state.overlay_text.is_some() {
          self.state.help_open = false;
          self.state.overlay_text = None;
        } else {
          input_view::clear_editor(&self.state);
        }
      }
    }
  }

  /// 提交：命令分派或发起回合（16 §10 / §9）。
  async fn on_submit(&mut self) {
    let text = input_view::editor_text(&self.state);
    match parse_line(&text) {
      Ok(Command::Chat(chat)) if !chat.is_empty() => {
        let submitted = turn_ctl::submit(&mut self.state, &self.session);
        if !submitted {
          self.set_overlay("回合进行中：/stop 取消后可继续。");
        }
      }
      Ok(Command::Chat(_)) => {} // 空输入忽略
      Ok(Command::Help) => {
        self.state.help_open = true;
        input_view::clear_editor(&self.state);
      }
      Ok(Command::New(name)) => {
        turn_ctl::rotate_key(&mut self.state, name);
        let key = self.state.status.session_key.clone();
        input_view::clear_editor(&self.state);
        self.set_overlay(format!("已切换到会话 {key}"));
      }
      Ok(Command::Stop | Command::Cancel) => {
        let stopped =
          turn_ctl::request_stop(&mut self.state, &self.session).await;
        let message = if stopped {
          "已请求取消在途回合。".to_string()
        } else {
          "没有进行中的回合。".to_string()
        };
        input_view::clear_editor(&self.state);
        self.set_overlay(message);
      }
      Ok(Command::Skills) => {
        let mut text = String::new();
        if self.session.skills.is_empty()
          && self.session.skill_diagnostics.is_empty()
        {
          text.push_str("（无技能）");
        }
        for (skill, origin) in self
          .session
          .skills
          .iter()
          .zip(self.session.skill_origins.iter())
        {
          let origin_label = match origin.as_str() {
            "project" => "来自项目".to_string(),
            "global" => "来自全局".to_string(),
            other if other.starts_with("plugin:") => {
              format!("来自插件 {}", &other["plugin:".len()..])
            }
            other => other.to_string(),
          };
          text.push_str(&format!("skill: {skill}（{origin_label}）\n"));
        }
        for diagnostic in &self.session.skill_diagnostics {
          text.push_str(&format!("diagnostic: {diagnostic}\n"));
        }
        self.set_overlay(text);
        input_view::clear_editor(&self.state);
      }
      Ok(Command::Sessions) => {
        let keys = self.session.harness.list_sessions().await;
        let text = if keys.is_empty() {
          "（无会话）".to_string()
        } else {
          keys.join(", ")
        };
        self.set_overlay(format!("会话：{text}"));
        input_view::clear_editor(&self.state);
      }
      Ok(Command::Status) => {
        let status = &self.state.status;
        let in_flight = matches!(self.state.turn, TurnState::Running { .. });
        self.set_overlay(format!(
          "session: {} | mode: {:?} | in_flight: {in_flight} | tok in:{} out:{} | warnings: {}",
          status.session_key,
          status.mode,
          status.total_input,
          status.total_output,
          status.warnings.len()
        ));
        input_view::clear_editor(&self.state);
      }
      Ok(Command::History(n)) => {
        let key = self.state.status.session_key.clone();
        let messages = self
          .session
          .harness
          .session_messages(&key)
          .await
          .unwrap_or_default();
        let skip = messages.len().saturating_sub(n);
        let text = messages[skip..]
          .iter()
          .map(render_helpers::history_line)
          .collect::<Vec<_>>()
          .join("\n");
        self.set_overlay(if text.is_empty() {
          "（无历史）".to_string()
        } else {
          text
        });
        input_view::clear_editor(&self.state);
      }
      Ok(Command::Exit) => self.state.exiting = true,
      Err(err) => {
        self.set_overlay(err.to_string());
        input_view::clear_editor(&self.state);
      }
    }
  }

  /// Tab 补全：命令 token（行首 `/`）候选 + 模糊匹配（16 §7.4）。
  fn on_complete(&mut self) {
    let text = input_view::editor_text(&self.state);
    let Some(completion) = crate::input::completion::complete(&text) else {
      return;
    };
    // 命令 token 位于行首（补全仅支持首行命令）：整体重建编辑器内容，
    // End 键把光标移到行末（等价于「插入文本末」，16 §7.4）
    input_view::set_editor_text(&self.state, &completion.text);
    self
      .state
      .editor
      .borrow_mut()
      .input(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
    if completion.ambiguous {
      self.set_overlay(format!(
        "候选：{}",
        completion
          .alternatives
          .iter()
          .map(|m| m.name)
          .collect::<Vec<_>>()
          .join("  ")
      ));
    } else {
      self.state.overlay_text = None;
    }
  }

  /// 滚动 N 行（上滚脱离跟随，触底恢复跟随）。
  fn scroll_by(&mut self, delta: i32) {
    let width = render_helpers::FALLBACK_WIDTH;
    let total =
      render_helpers::refresh_line_cache(&mut self.state, width) as i32;
    let scroll = &mut self.state.scroll;
    if delta < 0 {
      scroll.follow_end = false;
      scroll.offset = scroll.offset.saturating_sub(delta.unsigned_abs() as u16);
    } else {
      let max = total.saturating_sub(1) as u16;
      let next = scroll.offset.saturating_add(delta.unsigned_abs() as u16);
      scroll.offset = next.min(max);
      if next >= max {
        scroll.follow_end = true;
      }
    }
  }

  /// 翻页（页高 10 行，简单够用）。
  fn scroll_page(&mut self, direction: i32) {
    self.scroll_by(direction * 10);
  }

  /// 窗口尺寸变化（16 §7.5）：重绘 + 行数缓存按新宽度重算 +
  /// 编辑器折行随下一次渲染自动重排；offset 在渲染时 clamp。
  fn on_resize(&mut self, width: u16, height: u16) {
    // 宽度变化的行数重算由 draw 前的 refresh_line_cache 完成
    //（cached_lines 仅宽度一致才命中）；offset clamp 在 DialogView
    // 渲染时执行（min(总行数-可视行) 兜底，不跳底不越界）
    tracing::debug!(width = width, height = height, "terminal resized");
  }

  /// 广播 Lagged：丢弃本地基线（清流式指针 + 行数缓存），等待
  /// MessageStart 重同步（16 §5，对齐 CLI on_lagged）。
  fn on_lagged(&mut self, skipped: u64) {
    self.state.chat.streaming_id = None;
    self.state.scroll.clear_cache();
    self.state.chat.push(ChatBubbleBody::Assistant {
      text: format!("(已跳过 {skipped} 条事件，等待重同步)"),
      thinking: String::new(),
      error: None,
    });
  }

  /// 设置通用浮层文本。
  fn set_overlay(&mut self, text: impl Into<String>) {
    self.state.overlay_text = Some(text.into());
  }

  /// `AgentEvent` → `AppState` 映射（16 §5 表）。
  pub fn apply_event(&mut self, event: &AgentEvent) {
    match event {
      AgentEvent::AgentStart | AgentEvent::TurnStart => {
        self.state.status.in_flight = true;
      }
      AgentEvent::MessageStart { message } => match message {
        ra_core::message::Message::User(user) => {
          let text = match &user.content {
            ra_core::message::UserContent::Text(t) => t.clone(),
            ra_core::message::UserContent::Blocks(blocks) => blocks
              .iter()
              .filter_map(|b| match b {
                ra_core::message::Content::Text(t) => Some(t.content.clone()),
                _ => None,
              })
              .collect::<Vec<_>>()
              .join("\n"),
          };
          self.state.chat.push(ChatBubbleBody::User(text));
        }
        ra_core::message::Message::Assistant(assistant) => {
          let id = self.state.chat.push(ChatBubbleBody::Assistant {
            text: String::new(),
            thinking: String::new(),
            error: None,
          });
          self.state.chat.streaming_id = Some(id);
          self.apply_partial(id, assistant);
        }
        ra_core::message::Message::ToolResult(result) => {
          self.state.chat.push(ChatBubbleBody::ToolResult {
            tool: result.tool_name.clone(),
            text: render_helpers::join_text(&result.content),
            is_error: result.is_error,
          });
        }
      },
      AgentEvent::MessageUpdate { partial } => {
        // 自持快照直写流式气泡（不碰 CLI render.rs 的 MessageBaseline）
        if let Some(id) = self.state.chat.streaming_id {
          self.apply_partial(id, partial);
        }
      }
      AgentEvent::MessageEnd { message } => {
        if let ra_core::message::Message::Assistant(assistant) = message {
          let id = self.state.chat.streaming_id.unwrap_or_else(|| {
            self.state.chat.push(ChatBubbleBody::Assistant {
              text: String::new(),
              thinking: String::new(),
              error: None,
            })
          });
          // let-chains 合并双重判断（AGENTS.md 工程惯例）
          if let Some(bubble) = self.state.chat.bubble_mut(id)
            && let ChatBubbleBody::Assistant { text, error, .. } =
              &mut bubble.body
          {
            *text = render_helpers::join_text(&assistant.content);
            *error = assistant.error_message.clone();
          }
        }
        self.state.chat.streaming_id = None;
      }
      AgentEvent::ToolExecutionStart { tool_name, .. } => {
        self.state.tools.push(ToolRun {
          name: tool_name.clone(),
          phase: ToolPhase::Running,
        });
      }
      AgentEvent::ToolExecutionEnd {
        tool_name,
        is_error,
        ..
      } => {
        // 并行批按完成序发射：按名匹配最后一个 Running 条目
        if let Some(run) = self.state.tools.iter_mut().rev().find(|r| {
          r.name == *tool_name && matches!(r.phase, ToolPhase::Running)
        }) {
          run.phase = ToolPhase::Done(*is_error);
        }
      }
      // 部分结果通道预留（本期不发射）：无状态更新
      AgentEvent::ToolExecutionUpdate { .. } => {}
      AgentEvent::TurnEnd {
        message,
        tool_results,
      } => {
        if let ra_core::message::Message::Assistant(assistant) = &**message {
          self.accumulate_usage(&assistant.usage);
        }
        for result in tool_results {
          if let Some(usage) = &result.usage {
            self.accumulate_usage(usage);
          }
        }
      }
      AgentEvent::AgentEnd { .. } => {
        self.state.turn = TurnState::Idle;
        self.state.status.in_flight = false;
      }
    }
  }

  /// 流式快照直写流式气泡（Text/Thinking 全量聚合，16 §5）。
  fn apply_partial(
    &mut self,
    id: u64,
    partial: &ra_core::message::AssistantMessage,
  ) {
    let text = render_helpers::join_text(&partial.content);
    let thinking = partial
      .content
      .iter()
      .filter_map(|b| match b {
        ra_core::message::Content::Thinking(t) => Some(t.content.clone()),
        _ => None,
      })
      .collect::<Vec<_>>()
      .join("\n");
    // let-chains 合并双重判断（AGENTS.md 工程惯例）
    if let Some(bubble) = self.state.chat.bubble_mut(id)
      && let ChatBubbleBody::Assistant {
        text: bubble_text,
        thinking: bubble_thinking,
        ..
      } = &mut bubble.body
    {
      *bubble_text = text;
      *bubble_thinking = thinking;
    }
    self.state.chat.invalidate_cache(id);
  }

  /// 累加真实 `Usage` 到状态行（16 §5 token 累计口径）。
  fn accumulate_usage(&mut self, usage: &ra_core::message::Usage) {
    self.state.status.total_input += usage.input.max(0) as u64;
    self.state.status.total_output += usage.output.max(0) as u64;
  }

  /// Ctrl-C / `/exit` 退出流程（16 §8 四步；R-R2：等待期 select! 同武装
  /// 「任务结束 / 超时 / 第二次 Ctrl-C」，绝不阻塞主循环）。
  ///
  /// 本函数以 [`lifecycle::finish_exit`]（`process::exit`）结束进程，
  /// 正常不返回。
  async fn exit_flow(mut self) {
    // 键盘 task 在退出等待期持续存活（主循环持有 Receiver）
    if let TurnState::Running { key, handle } = &mut self.state.turn {
      // 第 1 步：stop 在途回合（触发取消）
      let _ = self.session.harness.stop(key).await;
      // 第 2 步：select! 等待「已有在途任务落盘 / 超时 / 第二次 Ctrl-C」
      //（绝不再 spawn_turn 新开一轮；await handle.task 即等待落盘完成）
      let timeout = tokio::time::sleep(self.state.ctrl_c_timeout);
      tokio::pin!(timeout); // Sleep 非 Unpin：select! 需显式 pin
      loop {
        tokio::select! {
          // (a) 在途任务自然结束 → 优雅退出（落盘已完成）
          _ = &mut handle.task => lifecycle::finish_exit(0),
          // (b) 超时（仅首次武装）→ 进入「再按一次即强退」子状态
          _ = &mut timeout, if !self.state.force_ready => {
            self.state.force_ready = true;
          }
          // (c) 第二次 Ctrl-C → 显式恢复终端后强退（退出码 1）
          input = self.key_rx.recv() => {
            // let-chains 合并双重判断（AGENTS.md 工程惯例）
            if let Some(AppInput::Key(k)) = input
              && keybindings::is_ctrl_c(&k)
            {
              lifecycle::finish_exit(1);
            }
            // 其他键忽略，回到本 select! 继续等待
          }
        }
      }
    }
    // Idle：无在途回合，直接优雅退出
    lifecycle::finish_exit(0);
  }
}

/// 键盘 task 启动快捷入口：返回接收端与 task 句柄
///（Receiver 直接交给 [`App::new`]，Sender 已被键盘 task 持有）。
pub fn spawn_keyboard() -> (mpsc::Receiver<AppInput>, JoinHandle<()>) {
  let (tx, rx) = mpsc::channel(64);
  let handle = keyboard::start_keyboard(tx);
  (rx, handle)
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use ra_cli::config::LoadOutcome;
  use ra_cli::config::assemble::assemble_with_steps;
  use ra_core::message::{
    AssistantMessage, Content, Message, StopReason, TextContent,
    ToolResultMessage, Usage, UserContent, UserMessage,
  };
  use ratatui::backend::TestBackend;

  use super::*;
  use crate::state::ChatBubbleBody as Body;

  /// 构造测试用 App（mock 模式 + 空脚本 + 隔离工作空间）。
  fn test_app() -> (App, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temp dir");
    let outcome = LoadOutcome::load(None, Some(dir.path()));
    let session =
      assemble_with_steps(&outcome, Vec::new()).expect("mock 装配成功");
    let (_tx, rx) = mpsc::channel(64);
    drop(_tx);
    (App::new(session, rx), dir)
  }

  /// 用户消息。
  fn user_msg(text: &str) -> Message {
    Message::User(UserMessage {
      content: UserContent::Text(text.to_string()),
      timestamp: 0,
    })
  }

  /// 带文本的助手消息。
  fn assistant_msg(text: &str) -> Message {
    let mut message = AssistantMessage::empty("mock", "mock-model", 0);
    message.content = vec![Content::Text(TextContent {
      content: text.to_string(),
      signature: None,
    })];
    message.stop_reason = StopReason::Stop;
    Message::Assistant(message)
  }

  /// 带 usage 的助手消息。
  fn assistant_with_usage(text: &str, input: i64, output: i64) -> Message {
    let mut message = AssistantMessage::empty("mock", "mock-model", 0);
    message.content = vec![Content::Text(TextContent {
      content: text.to_string(),
      signature: None,
    })];
    message.usage = Usage {
      input,
      output,
      ..Usage::default()
    };
    message.stop_reason = StopReason::Stop;
    Message::Assistant(message)
  }

  fn tool_result_msg(name: &str, is_error: bool) -> Message {
    Message::ToolResult(ToolResultMessage {
      tool_call_id: "c1".to_string(),
      tool_name: name.to_string(),
      is_error,
      timestamp: 0,
      content: vec![Content::Text(TextContent {
        content: "done".to_string(),
        signature: None,
      })],
      details: None,
      usage: None,
      added_tool_names: Vec::new(),
    })
  }

  // 场景（Step A 验收）：完整事件序列 → AppState 快照。
  // 预期：气泡 [User, Assistant(流式聚合), ToolResult]、工具条目 Done、
  // usage 累计、AgentEnd 复位 Idle。
  #[test]
  fn events_build_app_state() {
    let (mut app, _dir) = test_app();

    let mut partial = AssistantMessage::empty("mock", "mock-model", 0);
    partial.content = vec![Content::Text(TextContent {
      content: "hel".to_string(),
      signature: None,
    })];

    app.apply_event(&AgentEvent::AgentStart);
    app.apply_event(&AgentEvent::TurnStart);
    app.apply_event(&AgentEvent::MessageStart {
      message: user_msg("hi"),
    });
    app.apply_event(&AgentEvent::MessageStart {
      message: assistant_msg(""),
    });
    app.apply_event(&AgentEvent::MessageUpdate {
      partial: Arc::new(partial),
    });
    app.apply_event(&AgentEvent::MessageEnd {
      message: assistant_msg("hello"),
    });
    app.apply_event(&AgentEvent::ToolExecutionStart {
      tool_call_id: "c1".into(),
      tool_name: "echo".into(),
      args: serde_json::json!({}),
    });
    app.apply_event(&AgentEvent::ToolExecutionEnd {
      tool_call_id: "c1".into(),
      tool_name: "echo".into(),
      result: Arc::new(ra_tools::types::ToolExecutionResult::default()),
      is_error: false,
    });
    app.apply_event(&AgentEvent::MessageStart {
      message: tool_result_msg("echo", false),
    });
    app.apply_event(&AgentEvent::MessageEnd {
      message: tool_result_msg("echo", false),
    });
    app.apply_event(&AgentEvent::TurnEnd {
      message: Box::new(assistant_with_usage("hello", 10, 5)),
      tool_results: Vec::new(),
    });
    app.apply_event(&AgentEvent::AgentEnd {
      messages: Vec::new(),
    });

    // 气泡快照：User → Assistant(聚合文本) → ToolResult
    assert_eq!(app.state.chat.messages.len(), 3);
    assert!(matches!(
      app.state.chat.messages[0].body,
      Body::User(ref t) if t == "hi"
    ));
    assert!(matches!(
      app.state.chat.messages[1].body,
      Body::Assistant { ref text, error: None, .. } if text == "hello"
    ));
    assert!(matches!(
      app.state.chat.messages[2].body,
      Body::ToolResult { ref tool, is_error: false, .. } if tool == "echo"
    ));
    // 工具条目与 usage 累计
    assert_eq!(app.state.tools.len(), 1);
    assert!(matches!(app.state.tools[0].phase, ToolPhase::Done(false)));
    assert_eq!(app.state.status.total_input, 10);
    assert_eq!(app.state.status.total_output, 5);
    // AgentEnd 复位
    assert!(matches!(app.state.turn, TurnState::Idle));
    assert!(!app.state.status.in_flight);
  }

  // 场景（Step A 验收）：多事件同轮批处理 → 至多一次 draw。
  // 预期：手动应用一批事件后单次 draw 计数 +1（无逐事件重绘）。
  #[test]
  fn batch_applies_then_draws_once() {
    let (mut app, _dir) = test_app();
    let mut terminal =
      ratatui::Terminal::new(TestBackend::new(80, 24)).expect("test backend");

    app.apply_event(&AgentEvent::AgentStart);
    app.apply_event(&AgentEvent::TurnStart);
    app.apply_event(&AgentEvent::MessageStart {
      message: user_msg("hi"),
    });
    let draws_after_events = app.draws;
    app.draw(&mut terminal).expect("test backend draw");
    assert_eq!(app.draws, draws_after_events + 1, "一轮至多一次 draw");
  }

  // 场景：Lagged 丢弃本地基线（清流式指针 + 缓存）。
  #[test]
  fn lagged_drops_streaming_baseline() {
    let (mut app, _dir) = test_app();
    app.apply_event(&AgentEvent::MessageStart {
      message: assistant_msg(""),
    });
    let streaming_id = app.state.chat.streaming_id;
    assert!(streaming_id.is_some());

    app.on_lagged(3);
    assert!(app.state.chat.streaming_id.is_none(), "基线应丢弃");
    assert!(app.state.scroll.line_cache.is_empty());
    // 重同步提示气泡已追加
    let last = app.state.chat.messages.last().expect("提示气泡");
    assert!(
      matches!(last.body, Body::Assistant { ref text, .. } if text.contains("已跳过"))
    );
  }

  // 场景（Step B）：中文 / Emoji 经编辑器输入保持 char boundary 安全。
  // 预期：多字节字符逐个插入不 panic、文本完整、光标可继续输入。
  #[test]
  fn editor_handles_cjk_and_emoji() {
    let (app, _dir) = test_app();
    for ch in ["你", "好", "🌍"] {
      app
        .state
        .editor
        .borrow_mut()
        .input(tui_textarea::Input::from(crossterm::event::KeyEvent::new(
          crossterm::event::KeyCode::Char(ch.chars().next().expect("单字符")),
          crossterm::event::KeyModifiers::NONE,
        )));
    }
    let text = input_view::editor_text(&app.state);
    assert_eq!(text, "你好🌍");
    // 光标仍在合法位置：继续输入不 panic 且文本追加
    app
      .state
      .editor
      .borrow_mut()
      .input(tui_textarea::Input::from(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('!'),
        crossterm::event::KeyModifiers::NONE,
      )));
    assert_eq!(input_view::editor_text(&app.state), "你好🌍!");
  }

  // 场景：在途回合拒绝第二次提交（对齐 CLI 串行回合）。
  #[tokio::test]
  async fn submit_rejected_while_turn_running() {
    let (mut app, _dir) = test_app();
    // 构造在途状态（假句柄：立即完成的空任务）
    let handle = ra_cli::config::assemble::TurnHandle {
      task: tokio::spawn(async {}),
      done: mpsc::channel(1).1,
    };
    app.state.turn = TurnState::Running {
      key: "default".into(),
      handle,
    };

    input_view::set_editor_text(&app.state, "second");
    let submitted = turn_ctl::submit(&mut app.state, &app.session);
    assert!(!submitted, "在途回合应拒绝提交");
    // 输入未被清空（拒绝保留原文）
    assert_eq!(input_view::editor_text(&app.state), "second");
  }
}
