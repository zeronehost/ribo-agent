//! TUI 主循环：`tokio::select!` 三源批处理 + AppState 事件映射 +
//! Ctrl-C 生命周期。
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
use crate::state::{AppState, ChatBubbleBody, TurnState};
use crate::turn as turn_ctl;
use crate::view::RootLayout;
use crate::view::input as input_view;

type StdTerminal =
  ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>;

/// 装配警告浮层文本：非空返回带计数的逐条列表，空返回 `None`。
fn warnings_overlay_text(warnings: &[String]) -> Option<String> {
  if warnings.is_empty() {
    return None;
  }
  let mut text =
    format!("装配警告 {} 条（不影响运行，Esc 关闭）：\n", warnings.len());
  for warning in warnings {
    text.push_str(&format!("  - {warning}\n"));
  }
  Some(text)
}

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
      session.provider_id.clone(),
      session.model_name.clone(),
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

  /// 刷新状态行上下文三态（总量 / 已压缩 / 当前）。
  ///
  /// 数据源为会话内 `CompactionStats`——`HarnessContext::build` 每轮
  /// 维护，冷启动由 `SessionStatsEstimator` 回放后回填（total=转录估算 /
  /// compressed=0 / current=0），故首次对话前侧栏即有数值。
  /// 窗口分母取装配期 `CliSession.context_window`。
  async fn refresh_context_stats(&mut self) {
    let key = self.state.status.session_key.clone();
    self.state.status.compaction =
      self.session.harness.session_compaction_stats(&key).await;
    self.state.status.context_window = self.session.context_window;
  }

  /// 切换会话后重建对话区：清空当前气泡，从会话转录回放历史气泡。
  ///
  /// 冷启动或 `/sessions <name>` 切到有历史的会话时，Agent 侧虽会回放历史给模型，
  /// 但 UI 气泡队列不会自动填充——必须显式加载，否则对话区只显示
  /// 本轮输入，用户误以为「会话历史丢失」。
  async fn load_session_history(&mut self) {
    let key = self.state.status.session_key.clone();
    let messages = match self.session.harness.session_messages(&key).await {
      Ok(messages) => messages,
      Err(err) => {
        tracing::warn!(%key, %err, "load session history failed");
        self.set_overlay("会话历史加载失败，显示为空");
        return;
      }
    };
    self.state.chat.clear();
    self.state.scroll.clear_cache();
    for message in &messages {
      let bubble = match message {
        ra_core::message::Message::User(user) => {
          let text = match &user.content {
            ra_core::message::UserContent::Text(t) => t.clone(),
            ra_core::message::UserContent::Blocks(blocks) => {
              render_helpers::join_text(blocks)
            }
          };
          ChatBubbleBody::User(text)
        }
        ra_core::message::Message::Assistant(assistant) => {
          let text = render_helpers::join_text(&assistant.content);
          let thinking = assistant
            .content
            .iter()
            .filter_map(|b| match b {
              ra_core::message::Content::Thinking(t) => Some(t.content.clone()),
              _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
          ChatBubbleBody::Assistant {
            text,
            thinking,
            error: None,
          }
        }
        ra_core::message::Message::ToolResult(result) => {
          ChatBubbleBody::ToolResult {
            call_id: Some(result.tool_call_id.clone()),
            tool: result.tool_name.clone(),
            text: render_helpers::join_text(&result.content),
            is_error: result.is_error,
            running: false, // 历史回放均为终态
          }
        }
      };
      self.state.chat.push(bubble);
    }
    // 历史 usage 回填：冷启动 / 切会话后按转录内助手消息的真实 usage 重新
    // 累计（此前仅靠 `TurnEnd` 事件累加，冷启动恒为 0）。
    let (input, output) = usage_totals(&messages);
    self.state.status.total_input = input;
    self.state.status.total_output = output;
    tracing::debug!(
      session_key = %key,
      total_input = input,
      total_output = output,
      "session history usage backfilled"
    );
    // 回到粘底（历史加载后展示最新一轮）
    self.state.scroll.follow_end = true;
    self.state.scroll.offset = 0;
  }

  /// 主循环：三源 select → 批处理 → 至多一次 draw。
  ///
  /// 退出：`exiting` 置位（Ctrl-C / `/exit` / 事件源关闭 / 键盘 task
  /// 死亡）后跳出循环进入 [`App::exit_flow`]——该函数**总是**
  /// `process::exit`，本函数正常不会返回。
  ///
  /// # Errors
  ///
  /// 终端绘制失败时返回错误（由 panic hook / guard 兜底恢复终端）。
  pub async fn run(mut self, mut terminal: StdTerminal) -> anyhow::Result<()> {
    // 冷启动即有转录（磁盘回放）：先加载历史气泡 + ctx 占比再绘制首帧
    self.load_session_history().await;
    self.refresh_context_stats().await;
    // 装配期警告自动弹出：/status 已收敛为 CLI 专用，
    // TUI 侧栏只显示计数——详情在启动时一次性浮层展示（Esc 关闭）。
    // 不覆盖已有浮层（如会话历史加载失败提示——后者对用户更关键）。
    if let Some(text) = warnings_overlay_text(&self.state.status.warnings)
      && self.state.overlay_text.is_none()
    {
      self.set_overlay(text);
    }
    self.draw(&mut terminal)?;
    // loading 动画时钟：100ms/帧；仅在途时递增帧号并触发重绘
    let mut ticker =
      tokio::time::interval(std::time::Duration::from_millis(100));
    ticker.tick().await; // 消费首次立即触发，避免开局空转
    loop {
      let mut need_draw = false;
      tokio::select! {
        _ = ticker.tick() => {
          if self.state.status.in_flight {
            self.state.spinner_tick = self.state.spinner_tick.wrapping_add(1);
            need_draw = true;
          }
        }
        input = self.key_rx.recv() => {
          need_draw = true;
          match input {
            // 键盘 task 死亡（终端关闭）：转入退出流程
            None => self.state.exiting = true,
            Some(AppInput::Key(key)) => self.on_key(key).await,
            Some(AppInput::Resize(w, h)) => self.on_resize(w, h),
          }
        },
        event = self.session.events.recv() => {
          need_draw = true;
          match event {
          Ok(event) => {
            // 批处理：把当前已排队的全部事件一次性应用
            let mut ended = event.is_terminal();
            self.apply_event(&event);
            while let Ok(next) = self.session.events.try_recv() {
              let next_ended = next.is_terminal();
              self.apply_event(&next);
              ended |= next_ended;
            }
            // 终态事件后转录已落盘：按完整转录刷新 ctx 占比
            if ended {
              self.refresh_context_stats().await;
            }
          }
          Err(broadcast::error::RecvError::Lagged(n)) => self.on_lagged(n),
          Err(broadcast::error::RecvError::Closed) => self.state.exiting = true,
          }
        },
      }
      if self.state.exiting {
        break;
      }
      // tick 空闲帧不重绘（避免 CPU 空转）
      if need_draw {
        self.draw(&mut terminal)?;
      }
    }
    self.exit_flow().await;
    unreachable!("exit_flow 总以 process::exit 结束");
  }

  /// 绘制一帧：刷新行数缓存（按当前宽度）→ 布局渲染。
  fn draw<B: ratatui::backend::Backend>(
    &mut self,
    terminal: &mut ratatui::Terminal<B>,
  ) -> anyhow::Result<()> {
    let terminal_width = terminal
      .size()
      .map(|s| s.width)
      .unwrap_or(render_helpers::FALLBACK_WIDTH);
    // 关键：行数缓存必须用对话区宽度（终端宽 - 侧栏），否则折行数低估
    // → 滚动偏移错位、内容显示不全（布局改造回归修复）
    let width = crate::view::RootLayout::dialog_width(terminal_width);
    self.state.dialog_width = width;
    render_helpers::refresh_line_cache(&mut self.state, width);
    let Self { state, layout, .. } = self;
    terminal.draw(|frame| layout.render(frame, state))?;
    self.draws += 1;
    Ok(())
  }

  /// 键盘分派：先查键位表，未命中交给编辑器。
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

  /// 提交：命令分派或发起回合。
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
      Ok(Command::Sessions(name)) => {
        input_view::clear_editor(&self.state);
        let Some(name) = name else {
          // 无参：列出会话 + 用法提示
          let keys = self.session.harness.list_sessions().await;
          let mut text = String::new();
          let current = &self.state.status.session_key;
          if keys.is_empty() {
            text.push_str("（无会话）\n");
          }
          for key in &keys {
            let marker = if key == current { "→ " } else { "  " };
            text.push_str(&format!("{marker}{key}\n"));
          }
          text.push_str("用法：/sessions <名称> 切换或新建");
          self.set_overlay(text);
          return;
        };
        // 带参：切换会话（不存在则新建）；在途拒绝
        if matches!(self.state.turn, TurnState::Running { .. }) {
          self.set_overlay("回合进行中：/stop 取消后再切换会话。");
          return;
        }
        let existed = self
          .session
          .harness
          .list_sessions()
          .await
          .iter()
          .any(|k| k == &name);
        if name == self.state.status.session_key {
          self.set_overlay(format!("已在会话 {name}。"));
          return;
        }
        self.state.status.session_key = name.clone();
        let message = if existed {
          format!("已切换到会话 {name}")
        } else {
          format!("会话 {name} 不存在，已新建（空白历史）")
        };
        self.set_overlay(message);
        // 切换会话后立即重建对话区（回放该会话的历史气泡）+ ctx 占比
        self.load_session_history().await;
        self.refresh_context_stats().await;
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
      // CliOnly 命令（status、/history）在 TUI 中降级为指引提示：
      // 状态由侧栏常驻、历史即对话区本身
      Ok(Command::Status) => {
        self.set_overlay(
          "/status 仅 CLI 可用——TUI 左侧栏已常驻显示会话/模式/模型/token/上下文占比。",
        );
        input_view::clear_editor(&self.state);
      }
      Ok(Command::History(_)) => {
        self.set_overlay(
          "/history 仅 CLI 可用——TUI 对话区即完整历史，PgUp/PgDn 翻页查看。",
        );
        input_view::clear_editor(&self.state);
      }
      Ok(Command::Exit) => self.state.exiting = true,
      Err(err) => {
        self.set_overlay(err.to_string());
        input_view::clear_editor(&self.state);
      }
    }
  }

  /// Tab 补全：命令 token（行首 `/`）候选 + 模糊匹配。
  fn on_complete(&mut self) {
    let text = input_view::editor_text(&self.state);
    let Some(completion) = crate::input::completion::complete(&text) else {
      return;
    };
    // 命令 token 位于行首（补全仅支持首行命令）：整体重建编辑器内容，
    // End 键把光标移到行末（等价于「插入文本末」）
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
    // 与 draw 使用同一宽度（对话区有效宽），否则滚动上限错位
    let width = self.state.dialog_width;
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

  /// 窗口尺寸变化：重绘 + 行数缓存按新宽度重算 +
  /// 编辑器折行随下一次渲染自动重排；offset 在渲染时 clamp。
  fn on_resize(&mut self, width: u16, height: u16) {
    // 宽度变化的行数重算由 draw 前的 refresh_line_cache 完成
    //（cached_lines 仅宽度一致才命中）；offset clamp 在 DialogView
    // 渲染时执行（min(总行数-可视行) 兜底，不跳底不越界）
    tracing::debug!(width = width, height = height, "terminal resized");
  }

  /// 广播 Lagged：丢弃本地基线（清流式指针 + 行数缓存），等待
  /// MessageStart 重同步。
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

  /// `AgentEvent` → `AppState` 映射。
  pub fn apply_event(&mut self, event: &AgentEvent) {
    match event {
      // 压缩痕迹进对话流（持久气泡，不显示摘要内容 / 截断条数）；
      // 失败回退同样留痕（黄色提示）。Compaction 与 AgentStart
      // 同批到达，瞬时状态在即时渲染下不可见，气泡是唯一可见载体。
      AgentEvent::Compaction { failed } => {
        self
          .state
          .chat
          .push(ChatBubbleBody::Compaction { failed: *failed });
      }
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
          // 回灌消息按源序到达。若 ToolExecutionStart/End 已按调用序
          // 插入并更新了同名气泡，则此处只更新、不重复插入（按 call_id
          // 关联）；无对应气泡才新建（兼容取消/脱敏回灌等边缘路径）。
          let text = render_helpers::join_text(&result.content);
          let bubble_id = self
            .state
            .chat
            .tool_bubble_index(&result.tool_call_id)
            .and_then(|pos| {
              let bubble = self.state.chat.messages.get_mut(pos)?;
              if let ChatBubbleBody::ToolResult {
                text: slot,
                is_error,
                running,
                ..
              } = &mut bubble.body
              {
                *slot = text.clone();
                *is_error = result.is_error;
                *running = false;
              }
              Some(bubble.id)
            });
          if let Some(id) = bubble_id {
            self.state.chat.invalidate_cache(id);
          } else {
            self.state.chat.push(ChatBubbleBody::ToolResult {
              call_id: Some(result.tool_call_id.clone()),
              tool: result.tool_name.clone(),
              text,
              is_error: result.is_error,
              running: false,
            });
          }
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
            && let ChatBubbleBody::Assistant {
              text,
              thinking,
              error,
              ..
            } = &mut bubble.body
          {
            *text = render_helpers::join_text(&assistant.content);
            *thinking = assistant
              .content
              .iter()
              .filter_map(|b| match b {
                ra_core::message::Content::Thinking(t) => {
                  Some(t.content.clone())
                }
                _ => None,
              })
              .collect::<Vec<_>>()
              .join("\n");
            *error = assistant.error_message.clone();
          }
          // 终态消息可能与最后一个流式快照不同：必须失效行数缓存，
          // 否则新增行的高度按旧缓存算 → 末尾内容被截（回合结束丢失）
          self.state.chat.invalidate_cache(id);
        }
        self.state.chat.streaming_id = None;
      }
      AgentEvent::ToolExecutionStart {
        tool_call_id,
        tool_name,
        ..
      } => {
        // 工具调用按发生序内联到对话：插入「执行中」气泡，
        // 后续 ToolExecutionEnd / 回灌消息按 call_id 更新同一气泡。
        let id = self.state.chat.push(ChatBubbleBody::ToolResult {
          call_id: Some(tool_call_id.clone()),
          tool: tool_name.clone(),
          text: "执行中…".into(),
          is_error: false,
          running: true,
        });
        let _ = id;
      }
      AgentEvent::ToolExecutionEnd {
        tool_call_id,
        is_error,
        result,
        ..
      } => {
        // 按 call_id 找到 Start 插入的气泡，填入结果并标记完成。
        let contents: Vec<ra_core::message::Content> = result
          .content
          .iter()
          .cloned()
          .map(ra_core::message::Content::from)
          .collect();
        let bubble_id = self
          .state
          .chat
          .tool_bubble_index(tool_call_id)
          .and_then(|pos| {
            let bubble = self.state.chat.messages.get_mut(pos)?;
            if let ChatBubbleBody::ToolResult {
              text,
              is_error: slot_error,
              running,
              ..
            } = &mut bubble.body
            {
              *text = render_helpers::join_text(&contents);
              *slot_error = *is_error;
              *running = false;
            }
            Some(bubble.id)
          });
        if let Some(id) = bubble_id {
          self.state.chat.invalidate_cache(id);
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

  /// 流式快照直写流式气泡（Text/Thinking 全量聚合）。
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

  /// 累加真实 `Usage` 到状态行。
  fn accumulate_usage(&mut self, usage: &ra_core::message::Usage) {
    self.state.status.total_input += usage.input.max(0) as u64;
    self.state.status.total_output += usage.output.max(0) as u64;
  }

  /// Ctrl-C / `/exit` 退出流程（四步；R-R2：等待期 select! 同武装
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

/// 按转录内助手消息的真实 usage 汇总 token 累计（冷启动 / 切会话回填）。
///
/// 与 `accumulate_usage` 同口径（`usage.input` / `usage.output`，负值按 0 计），
/// 但从历史转录整体重算而非逐事件累加——冷启动回放不产生 `TurnEnd` 事件，
/// 累计只靠事件时会恒为 0。
fn usage_totals(messages: &[ra_core::message::Message]) -> (u64, u64) {
  let mut input = 0u64;
  let mut output = 0u64;
  for message in messages {
    if let ra_core::message::Message::Assistant(assistant) = message {
      input += assistant.usage.input.max(0) as u64;
      output += assistant.usage.output.max(0) as u64;
    }
  }
  (input, output)
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
    // 工具调用内联到对话：Start 插入、End 更新、回灌去重（仍 3 个气泡）
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
      Body::ToolResult {
        ref tool,
        ref call_id,
        running: false,
        is_error: false,
        ref text,
        ..
      } if tool == "echo" && call_id.as_deref() == Some("c1") && text == "done"
    ));
    // usage 累计（工具结果 usage 也计入）
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

  // 场景：冷启动回放后 provider 已按当前转录写入估算三态
  // → 首次对话前侧栏即有数值；刷新时读回并同步窗口分母。
  #[tokio::test]
  async fn refresh_context_stats_reads_session_stats() {
    let (mut app, _dir) = test_app();
    assert_eq!(app.session.context_window, 8192); // mock 模型窗口
    // 初始状态为 None（尚未从会话读取）
    assert_eq!(app.state.status.compaction, None);

    // 冷启动回放 → 会话内已有估算三态（空转录 → total=0、compressed=0、
    // current=0：本次进程尚未发起请求，窗口未被占用）
    app.refresh_context_stats().await;
    let stats = app
      .state
      .status
      .compaction
      .expect("cold start must carry estimated stats");
    assert_eq!(stats.total_tokens, 0);
    assert_eq!(stats.compressed_tokens, 0);
    assert_eq!(stats.current_tokens, 0);
    // 窗口分母同步自装配期真值
    assert_eq!(app.state.status.context_window, 8192);
  }

  // 场景（新布局）：侧栏渲染 provider/model/token，对话底部 loading
  // 在途时显示动画、空闲显示「就绪」；窄屏侧栏隐藏。
  #[test]
  fn layout_renders_sidebar_and_loading() {
    let (mut app, _dir) = test_app();
    app.state.status.provider = "openai".into();
    app.state.status.model = "gpt-test".into();
    app.state.status.total_input = 123;
    app.state.status.total_output = 45;
    let mut terminal =
      ratatui::Terminal::new(TestBackend::new(100, 30)).expect("backend");
    app.draw(&mut terminal).expect("draw");
    let buf = terminal.backend().buffer();
    let text: String = buf.content().iter().map(|c| c.symbol()).collect();
    // CJK 在 buffer 中按全角展开（"状态" → "状 态"）
    assert!(
      text.contains("状 态"),
      "sidebar title missing, got: {text:?}"
    );
    assert!(text.contains("openai"), "provider missing");
    assert!(text.contains("gpt-test"), "model missing");
    assert!(text.contains("123"), "input token missing");
    assert!(text.contains("就 绪"), "idle loading missing");

    // 在途：loading 区显示动画字符
    app.state.status.in_flight = true;
    app.state.spinner_tick = 1;
    app.draw(&mut terminal).expect("draw in flight");
    let text: String = terminal
      .backend()
      .buffer()
      .content()
      .iter()
      .map(|c| c.symbol())
      .collect();
    assert!(text.contains("处 理 中"), "in-flight loading missing");

    // 窄屏：侧栏隐藏，不显示 provider 文本
    let mut narrow =
      ratatui::Terminal::new(TestBackend::new(50, 24)).expect("backend");
    app.draw(&mut narrow).expect("narrow draw");
    let text: String = narrow
      .backend()
      .buffer()
      .content()
      .iter()
      .map(|c| c.symbol())
      .collect();
    assert!(
      !text.contains("openai"),
      "sidebar should hide on narrow screen"
    );
  }

  // 场景（布局回归）：行数缓存与滚动上限必须用对话区宽度（终端宽 - 侧栏），
  // 否则窄对话区折行被低估 → 内容显示不全 + 滚动无法触底。
  #[test]
  fn scroll_uses_dialog_width_not_terminal_width() {
    // 100 列时侧栏压缩到 1/3，对话文本区再减 Block 边框 2 列
    assert_eq!(crate::view::RootLayout::dialog_width(100), 65);
    assert_eq!(
      crate::view::RootLayout::dialog_width(50),
      48 // 窄屏无侧栏，仍减边框 2
    );
    let (mut app, _dir) = test_app();
    // 构造长文本气泡，模拟 draw 写入正确宽度的缓存
    app.state.chat.push(Body::User("行".repeat(200)));
    let width = app.state.dialog_width;
    let total = render_helpers::refresh_line_cache(&mut app.state, width);
    assert!(total > 2, "long text must wrap into multiple lines");
    // 上滚后 follow_end 解除、offset 可设置（不再被错误宽度锁死）
    app.scroll_by(-1);
    assert!(!app.state.scroll.follow_end);
    app.scroll_by(1000); // 下滚超量 → clamp 触底恢复跟随
    assert!(app.state.scroll.follow_end);
  }

  // 场景（会话切换回归）：写入历史后切换会话键，重建对话区应显示
  // 历史气泡（而非只显示本轮输入）；空会话则清空气泡。
  #[tokio::test]
  async fn load_session_history_rebuilds_bubbles() {
    let (mut app, dir) = test_app();
    // 在会话目录预写一条历史（test key），模拟磁盘回放
    let sessions = dir.path().join(".ribo/sessions");
    std::fs::create_dir_all(&sessions).expect("mkdir sessions");
    std::fs::write(
      sessions.join("test.jsonl"),
      "{\"v\":1,\"type\":\"meta\",\"key\":\"test\",\"created_at\":0}\n\
       {\"v\":1,\"type\":\"message\",\"ts\":0,\"message\":{\"role\":\"user\",\"content\":\"历史消息A\",\"timestamp\":0}}\n",
    )
    .expect("write jsonl");
    // 切到 test 会话并重建对话区
    app.state.status.session_key = "test".into();
    app.state.chat.push(Body::User("sentinel".into()));
    app.load_session_history().await;
    assert_eq!(app.state.chat.messages.len(), 1, "历史气泡应重建");
    assert!(
      matches!(&app.state.chat.messages[0].body, Body::User(t) if t == "历史消息A"),
      "历史内容应正确映射"
    );
  }

  // 场景：装配警告浮层文本构建。
  #[test]
  fn warnings_overlay_text_builds_list_or_none() {
    assert_eq!(warnings_overlay_text(&[]), None);
    let text = warnings_overlay_text(&["auth.toml should be 0600".into()])
      .expect("非空警告应返回文本");
    assert!(text.contains("装配警告 1 条"));
    assert!(text.contains("auth.toml should be 0600"));
    assert!(text.contains("Esc 关闭"));
  }

  // 场景：有警告时首帧自动弹出警告浮层（不覆盖已有浮层）。
  #[test]
  fn startup_auto_shows_warnings_overlay() {
    let (mut app, _dir) = test_app();
    // test_app 走真实装配、warnings 可能为非空：先清空，测「无警告不弹」
    app.state.status.warnings.clear();
    assert!(app.state.overlay_text.is_none());
    // run 启动段的等效逻辑：无已有浮层才弹出
    if let Some(text) = warnings_overlay_text(&app.state.status.warnings)
      && app.state.overlay_text.is_none()
    {
      app.set_overlay(text);
    }
    assert!(app.state.overlay_text.is_none(), "无警告不应弹出");
    // 注入警告后再走一遍 → 弹出且含详情
    app.state.status.warnings = vec!["auth.toml should be 0600".into()];
    if let Some(text) = warnings_overlay_text(&app.state.status.warnings)
      && app.state.overlay_text.is_none()
    {
      app.set_overlay(text);
    }
    let text = app.state.overlay_text.expect("警告浮层应弹出");
    assert!(text.contains("装配警告 1 条"));
    assert!(text.contains("auth.toml should be 0600"));
  }

  // 场景：ChatState::clear 清空气泡、流式指针与失效缓存。
  #[test]
  fn chat_clear_resets_streaming_and_invalidated() {
    let mut state = AppState::new(
      ra_cli::config::ConfigMode::Mock,
      "mock".into(),
      "m".into(),
      Vec::new(),
      "d".into(),
    );
    let id = state.chat.push(Body::User("x".into()));
    state.chat.streaming_id = Some(id);
    state.chat.invalidate_cache(id);
    state.chat.clear();
    assert!(state.chat.messages.is_empty());
    assert_eq!(state.chat.streaming_id, None);
  }

  // 决定性验证（滚不到底回归）：多气泡 + 粘底时，最后一条气泡文本
  // 必须出现在缓冲区——行数估算若漏算分隔空行/栅栏行，offset 偏小，
  // 底部内容会被截断（历史会话气泡多时尤其明显）。
  #[test]
  fn follow_end_shows_last_bubble_in_buffer() {
    let (mut app, _dir) = test_app();
    const N: usize = 30;
    for i in 0..N {
      app
        .state
        .chat
        .push(Body::User(format!("MARKER-{i:02}-END")));
    }
    app.state.scroll.follow_end = true;
    let mut terminal =
      ratatui::Terminal::new(TestBackend::new(100, 24)).expect("backend");
    app.draw(&mut terminal).expect("draw");
    let text: String = terminal
      .backend()
      .buffer()
      .content()
      .iter()
      .map(|c| c.symbol())
      .collect();
    assert!(
      text.contains(&format!("MARKER-{:02}-END", N - 1)),
      "last bubble must be visible when following end; got tail: {}",
      &text[text.len().saturating_sub(400)..]
    );
  }

  // 场景（回合结束内容丢失回归）：用户上滚后发起新回合，submit 应恢复
  // 粘底；MessageEnd 的终态文本必须失效行数缓存并重绘，末条可见。
  #[tokio::test]
  async fn new_turn_restores_follow_end_and_final_text() {
    let (mut app, _dir) = test_app();
    for i in 0..15 {
      app.state.chat.push(Body::User(format!("OLD-{i:02}")));
    }
    // 用户上滚查看历史 → follow_end=false
    app.scroll_by(-1);
    assert!(!app.state.scroll.follow_end);

    // 提交新回合 → 恢复粘底（需编辑器有文本）
    crate::view::input::set_editor_text(&app.state, "new question");
    assert!(crate::turn::submit(&mut app.state, &app.session));
    assert!(
      app.state.scroll.follow_end,
      "submit must restore follow_end"
    );

    // 模拟 assistant 流式快照 + 终态补充内容
    let id = app.state.chat.push(Body::Assistant {
      text: "partial".into(),
      thinking: String::new(),
      error: None,
    });
    app.state.chat.streaming_id = Some(id);
    let final_msg = assistant_msg("final-answer-with-extra-content");
    app.apply_event(&AgentEvent::MessageEnd { message: final_msg });
    assert!(app.state.scroll.follow_end);
    assert!(
      matches!(&app.state.chat.bubble_mut(id).unwrap().body,
        Body::Assistant { text, .. } if text == "final-answer-with-extra-content"),
      "final text must overwrite partial"
    );

    let mut terminal =
      ratatui::Terminal::new(TestBackend::new(100, 24)).expect("backend");
    app.draw(&mut terminal).expect("draw");
    let text: String = terminal
      .backend()
      .buffer()
      .content()
      .iter()
      .map(|c| c.symbol())
      .collect();
    assert!(
      text.contains("final-answer-with-extra-content"),
      "final answer must be visible after turn end"
    );
  }

  // 决定性验证（窗口化渲染 + sticky_bottom）：大内容（200 气泡 + 超长
  // 文本气泡）时 follow_end 锁底显示末条、首条不可见（窗口生效）；
  // 上滚可到中部、滚回触底恢复跟随。
  #[test]
  fn huge_content_windowed_render_reaches_tail() {
    let (mut app, _dir) = test_app();
    for i in 0..200 {
      app.state.chat.push(Body::User(format!("HEAD-{i:03}")));
    }
    // 超长单气泡（约 200 视觉行）：开头 TAIL-SENTINEL、结尾 END-MARKER
    app.state.chat.push(Body::Assistant {
      text: "TAIL-SENTINEL\n".to_string()
        + &"超长内容行。".repeat(300)
        + "\nEND-MARKER",
      thinking: String::new(),
      error: None,
    });
    app.state.scroll.follow_end = true;

    let mut terminal =
      ratatui::Terminal::new(TestBackend::new(100, 24)).expect("backend");
    app.draw(&mut terminal).expect("draw");
    let text: String = terminal
      .backend()
      .buffer()
      .content()
      .iter()
      .map(|c| c.symbol())
      .collect();
    assert!(
      text.contains("END-MARKER"),
      "sticky bottom must show tail of huge content"
    );
    assert!(
      !text.contains("HEAD-000"),
      "windowed render must clip head (only visible window folded)"
    );

    // 上滚到中部：能读到早期内容；下滚到底：恢复跟随
    app.scroll_page(-1000); // 大幅上滚
    assert!(!app.state.scroll.follow_end);
    app.draw(&mut terminal).expect("draw mid");
    let text: String = terminal
      .backend()
      .buffer()
      .content()
      .iter()
      .map(|c| c.symbol())
      .collect();
    assert!(
      text.contains("HEAD-0"),
      "scrolling up must reveal early bubbles"
    );
    app.scroll_by(100_000); // 滚到底 → 恢复跟随
    assert!(app.state.scroll.follow_end);
    app.draw(&mut terminal).expect("draw tail again");
    let text: String = terminal
      .backend()
      .buffer()
      .content()
      .iter()
      .map(|c| c.symbol())
      .collect();
    assert!(
      text.contains("END-MARKER"),
      "scroll to bottom must show tail again"
    );
  }

  /// Compaction 事件在对话流插入持久系统气泡（不显示摘要内容），
  /// AgentStart 不影响气泡（痕迹保留），回合继续走常规在途文案。
  #[test]
  fn compaction_bubble_persists_in_dialog_flow() {
    let (mut app, _dir) = test_app();
    let mut terminal =
      ratatui::Terminal::new(TestBackend::new(100, 30)).expect("backend");

    app.apply_event(&AgentEvent::Compaction { failed: false });
    // 气泡已入流且类型正确（文案由 dialog 渲染；事件无摘要内容字段）
    assert_eq!(app.state.chat.messages.len(), 1);
    assert!(matches!(
      &app.state.chat.messages[0].body,
      ChatBubbleBody::Compaction { failed: false }
    ));

    // AgentStart 不清除痕迹气泡；回合在途正常置位
    app.apply_event(&AgentEvent::AgentStart);
    assert_eq!(app.state.chat.messages.len(), 1);
    assert!(app.state.status.in_flight);

    app
      .draw(&mut terminal)
      .expect("draw with compaction bubble");
    let text: String = terminal
      .backend()
      .buffer()
      .content()
      .iter()
      .map(|c| c.symbol())
      .collect();
    // CJK 在 buffer 中按全角展开（"已压缩" → "已 压 缩"）
    assert!(
      text.contains("已 压 缩"),
      "compaction bubble missing: {text:?}"
    );
    assert!(!text.contains("SECRET-SUMMARY"));

    // 失败回退也留痕：断言黄色失败气泡文案可见
    app.apply_event(&AgentEvent::Compaction { failed: true });
    assert_eq!(app.state.chat.messages.len(), 2);
    assert!(matches!(
      &app.state.chat.messages[1].body,
      ChatBubbleBody::Compaction { failed: true }
    ));
    app.draw(&mut terminal).expect("draw with failed bubble");
    let text: String = terminal
      .backend()
      .buffer()
      .content()
      .iter()
      .map(|c| c.symbol())
      .collect();
    assert!(
      text.contains("压 缩 失 败"),
      "failed bubble missing: {text:?}"
    );
  }

  // 场景（冷启动回填）：历史助手消息的真实 usage 计入 Token 累计——
  // 冷启动不产生 `TurnEnd` 事件，此前累计恒为 0。
  #[test]
  fn usage_totals_sums_assistant_usage_from_transcript() {
    let mut first =
      ra_core::message::AssistantMessage::empty("mock", "mock-model", 0);
    first.usage.input = 100;
    first.usage.output = 20;
    let mut second =
      ra_core::message::AssistantMessage::empty("mock", "mock-model", 0);
    second.usage.input = 50;
    second.usage.output = 5;

    let messages = vec![
      user_msg("hi"),
      ra_core::message::Message::Assistant(first),
      ra_core::message::Message::Assistant(second),
    ];

    let (input, output) = usage_totals(&messages);
    assert_eq!(input, 150);
    assert_eq!(output, 25);

    // 无助手消息（空转录）→ 累计为 0
    let (input, output) = usage_totals(&[user_msg("hi")]);
    assert_eq!((input, output), (0, 0));
  }
}
