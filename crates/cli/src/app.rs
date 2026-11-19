//! REPL 主循环：命令分派、在途回合管理与事件渲染（14 详细设计 §9）。
//!
//! 核心不变式（概设 v2.4）：
//! - **复位权威**在回合任务完成分支（`turn_done_rx` 能拿到 `prompt`
//!   的 `Result`）；`AgentEnd` 仅作渲染权威——它早于落盘完成
//!   （agent_loop 事件 emit 先于 `store.append`），若由它复位会在
//!   append 进行中放行下一轮并吞掉落盘失败；
//! - 在途回合与当前会话键解耦：`/new` 只影响后续回合；
//! - stdout 只承载对话渲染与提示，tracing 日志由 logging 落文件。

use std::future::Future;

use ra_core::error::RiboResult;
use tokio::io::AsyncBufReadExt as _;
use tokio::sync::mpsc;

use crate::commands::{Command, parse_line};
use crate::config::ConfigMode;
use crate::config::assemble::CliSession;
use crate::render::Renderer;

/// 可注入输入源（生产用标准输入，E2E 用通道回放实现）。
pub trait LineSource {
  /// 取下一行输入；`None` 表示 EOF。
  ///
  /// 返回值用 RPITIT 并显式携带 `Send` 约束：[`Repl::run`] 的 future
  /// 需要跨任务 spawn（E2E 主循环与 REPL 并行驱动）。
  fn next_line(&mut self) -> impl Future<Output = Option<String>> + Send;
}

/// 在途回合状态（概设 B3）：在途 key 与当前会话键解耦。
enum TurnState {
  /// 空闲：可发起新回合。
  Idle,
  /// 回合进行中：`key` 为在途会话键（与当前会话键可不同）。
  Running {
    /// 在途回合的会话键（`/stop` 定位用）。
    key: String,
    /// prompt 任务句柄（结果经 `turn_done_rx` 回传；EOF 收尾时等待）。
    task: tokio::task::JoinHandle<()>,
  },
}

/// REPL 主循环。
pub struct Repl<S: LineSource> {
  source: S,
  session: CliSession,
  renderer: Box<dyn Renderer + Send>,
  turn: TurnState,
  /// 当前会话键（`/new` 轮换）。
  current_key: String,
  /// `/new` 缺省时的自动轮换计数（`{default_key}-{n}`）。
  new_counter: usize,
  /// prompt 任务结果通道（复位唯一权威）；每回合重建。
  /// `None`（全部 Sender 释放且无值）= 任务 panic 或被杀，异常终止。
  turn_done_rx: mpsc::Receiver<Result<(), ra_core::error::RiboError>>,
}

impl<S: LineSource + Send + 'static> Repl<S> {
  /// 创建 REPL（`renderer` 由调用方注入，E2E 用录制实现）。
  pub fn new(
    source: S,
    session: CliSession,
    renderer: Box<dyn Renderer + Send>,
  ) -> Self {
    let current_key = session.default_key.clone();
    // 占位通道：回合发起时整体重建（Sender 立即丢弃不影响 recv 语义）
    let (_tx, rx) = mpsc::channel(1);
    Self {
      source,
      session,
      renderer,
      turn: TurnState::Idle,
      current_key,
      new_counter: 0,
      turn_done_rx: rx,
    }
  }

  /// 主循环：三方 `select`（回合完成 / 输入行 / 事件流）。
  ///
  /// `biased` 保证回合完成优先于输入处理：回合自然收尾时，即使下一行
  /// 输入已就绪，也先复位再放行（避免「回合已结束但仍拒绝新 Chat」的
  /// 误报窗口）。
  ///
  /// # Errors
  ///
  /// 当前实现始终返回 `Ok`；保留 `RiboResult` 签名与装配/落盘错误
  /// 上浮路径对齐（后续阶段扩展）。
  pub async fn run(mut self) -> RiboResult<()> {
    banner(&self.session);
    loop {
      tokio::select! {
        biased;
        // ===== 回合任务完成：复位 TurnState 的唯一权威 =====
        result = self.turn_done_rx.recv(),
          if matches!(self.turn, TurnState::Running { .. }) =>
        {
          self.reset_turn(result);
        }
        // ===== 输入行 =====
        line = self.source.next_line() => match line {
          // EOF：请求取消在途回合后退出（不等待收尾——进程即将结束）
          None => {
            self.request_stop_if_running().await;
            break;
          }
          Some(line) => match parse_line(&line) {
            Ok(Command::Chat(text)) if !text.is_empty() => {
              self.handle_chat(text).await;
            }
            Ok(Command::Chat(_)) => {} // 空行忽略
            Ok(Command::Exit) => break,
            Ok(cmd) => self.dispatch(cmd).await,
            Err(e) => println!("{e}"),
          },
        },
        // ===== 事件流：仅渲染权威；复位见回合完成分支 =====
        recv = self.session.events.recv() => match recv {
          Ok(event) => self.renderer.on_event(&event),
          Err(broadcast_lagged) => {
            // broadcast 满载丢帧：丢弃基线等重同步（概设 v2.4 规则 4）
            let skipped = match broadcast_lagged {
              tokio::sync::broadcast::error::RecvError::Lagged(n) => n,
              tokio::sync::broadcast::error::RecvError::Closed => {
                self.renderer.on_closed();
                break;
              }
            };
            self.renderer.on_lagged(skipped);
          }
        },
      }
    }
    Ok(())
  }

  /// 发起用户回合：在途时拒绝（决策 6——串行回合，进度可观察）。
  async fn handle_chat(&mut self, text: String) {
    if matches!(self.turn, TurnState::Running { .. }) {
      println!("回合进行中：/stop 取消后可继续。");
      return;
    }
    // ===== 阶段 1：spawn prompt 任务（结果经通道回传）=====
    let key = self.current_key.clone();
    let (tx, rx) = mpsc::channel(1);
    let harness = self.session.harness.clone();
    let task_key = key.clone(); // 闭包持有副本，原值留给在途状态
    let task = tokio::spawn(async move {
      // Vec<Message> → ()：复位只关心成败
      let result = harness.prompt(&task_key, &text).await.map(|_| ());
      // 任务 panic/被杀时不发送：recv 返回 None，三分支兜底覆盖
      let _ = tx.send(result).await;
    });
    // ===== 阶段 2：登记在途状态（key 与当前会话键解耦快照）=====
    self.turn_done_rx = rx;
    self.turn = TurnState::Running { key, task };
  }

  /// `/stop`：触发取消并等待复位（终止渲染先于提示符）。
  ///
  /// spawn 的回合任务可能尚未注册运行标志（会话未建 / `running` 未
  /// 置位），`stop` 未命中时让出调度权重试；回合自然收尾的结果经
  /// `try_recv` 探测，避免死等。
  async fn handle_stop(&mut self) {
    let TurnState::Running { key, .. } = &self.turn else {
      println!("没有进行中的回合。");
      return;
    };
    let key = key.clone();

    // ===== 阶段 1：触发取消（重试直至命中或回合已自行收尾）=====
    loop {
      if self.session.harness.stop(&key).await {
        break;
      }
      // 未命中：让出调度权让回合任务推进（注册运行或收尾）
      tokio::task::yield_now().await;
      match self.turn_done_rx.try_recv() {
        // 回合已自然收尾（结果缓冲在通道）
        Ok(result) => {
          self.reset_turn(Some(result));
          return;
        }
        // 任务 panic / 被杀（Sender 全部释放且无值）
        Err(mpsc::error::TryRecvError::Disconnected) => {
          self.reset_turn(None);
          return;
        }
        Err(mpsc::error::TryRecvError::Empty) => {}
      }
    }

    // ===== 阶段 2：等待复位（取消收敛后 prompt 返回 Ok）=====
    while matches!(self.turn, TurnState::Running { .. }) {
      // 先取值再复位（避免 recv 与 reset_turn 的借用重叠）
      let result = self.turn_done_rx.recv().await;
      self.reset_turn(result);
    }
  }

  /// `/new`：轮换会话键；在途回合时拒绝（概设 B3——在途 key 与当前
  /// 会话键解耦，轮换只影响后续回合，但串行回合期间轮换会让「在途」
  /// 与「当前」分离，v1 直接拒绝保持语义简单）。
  fn handle_new(&mut self, name: Option<String>) {
    if matches!(self.turn, TurnState::Running { .. }) {
      println!("回合进行中：/stop 取消后再轮换会话。");
      return;
    }
    self.current_key = name.unwrap_or_else(|| {
      self.new_counter += 1;
      format!("{}-{}", self.session.default_key, self.new_counter)
    });
    println!("已切换到会话 {}", self.current_key);
  }

  /// 复位在途回合：`turn_done` 结果三分支（复位唯一权威）。
  fn reset_turn(
    &mut self,
    result: Option<Result<(), ra_core::error::RiboError>>,
  ) {
    match result {
      Some(Ok(())) => {} // 正常收敛（含 Aborted；AgentEnd 已渲染）
      Some(Err(e)) => println!("落盘失败：{e}"),
      None => println!("回合异常终止"), // 任务 panic / 被杀兜底
    }
    self.turn = TurnState::Idle;
  }

  /// EOF 退出前请求取消在途回合并等待任务收尾（进程退出态干净）。
  async fn request_stop_if_running(&mut self) {
    if let TurnState::Running { key, .. } = &self.turn {
      let key = key.clone();
      let _ = self.session.harness.stop(&key).await;
    }
    // 取任务句柄并等待：prompt 任务在取消收敛后自然退出；
    // 未在途（Idle）时 replace 返回 Idle，无需等待
    if let TurnState::Running { task, .. } =
      std::mem::replace(&mut self.turn, TurnState::Idle)
    {
      let _ = task.await;
    }
  }

  /// 命令分派（`Chat` 与 `Exit` 已在主循环分流）。
  async fn dispatch(&mut self, cmd: Command) {
    match cmd {
      Command::Chat(_) => unreachable!("Chat 已在主循环分流"),
      Command::Help => print_help(),
      Command::New(name) => self.handle_new(name),
      Command::Stop | Command::Cancel => self.handle_stop().await,
      Command::Skills => self.print_skills(),
      Command::Sessions => self.print_sessions().await,
      Command::Status => self.print_status(),
      Command::History(n) => self.print_history(n).await,
      Command::Exit => unreachable!("Exit 已在主循环分流"),
    }
  }

  /// `/status`：状态摘要（会话键 / 模式 / 在途回合）。
  ///
  /// token 累计与上下文占比为 TUI 状态行专属（CLI 事件流不做累计），
  /// 此处仅输出 REPL 已持有的事实。
  fn print_status(&self) {
    let in_flight = matches!(self.turn, TurnState::Running { .. });
    let mode = match self.session.mode {
      ConfigMode::Real => "real",
      ConfigMode::Mock => "mock",
    };
    println!(
      "session: {} | mode: {mode} | in_flight: {in_flight} | warnings: {}",
      self.current_key,
      self.session.warnings.len()
    );
  }

  /// `/skills`：技能名 + 装配期诊断。
  fn print_skills(&self) {
    if self.session.skills.is_empty()
      && self.session.skill_diagnostics.is_empty()
    {
      println!("（无技能）");
      return;
    }
    for (name, origin) in self
      .session
      .skills
      .iter()
      .zip(self.session.skill_origins.iter())
    {
      let origin_label = match origin.as_str() {
        "project" => "来自项目".to_string(),
        "global" => "来自全局".to_string(),
        other if other.starts_with("plugin:") => {
          format!("来自插件域 {}", &other["plugin:".len()..])
        }
        other => other.to_string(),
      };
      println!("skill: {name}（{origin_label}）");
    }
    for diagnostic in &self.session.skill_diagnostics {
      println!("diagnostic: {diagnostic}");
    }
  }

  /// `/sessions`：列出全部会话键（内存 ∪ 磁盘）。
  ///
  /// `&mut self`：async 方法跨 await 持有引用时，`&self` 会要求
  /// `Repl: Sync`（`Box<dyn Renderer + Send>` 非 Sync），导致 run 的
  /// future 失去 `Send`（E2E 需 spawn）。全部 async 方法统一 `&mut self`。
  async fn print_sessions(&mut self) {
    let keys = self.session.harness.list_sessions().await;
    if keys.is_empty() {
      println!("（无会话）");
      return;
    }
    println!("{}", keys.join(", "));
  }

  /// `/history [n]`：当前会话最近 n 条转录。
  async fn print_history(&mut self, n: usize) {
    let messages = match self
      .session
      .harness
      .session_messages(&self.current_key)
      .await
    {
      Ok(messages) => messages,
      Err(e) => {
        println!("读取历史失败：{e}");
        return;
      }
    };
    if messages.is_empty() {
      println!("（会话 {} 无历史）", self.current_key);
      return;
    }
    let skip = messages.len().saturating_sub(n);
    for message in &messages[skip..] {
      println!("{}", summarize_message(message));
    }
  }
}

/// 启动横幅：模式 + 降级原因（概设 D3——模式显式化）。
fn banner(session: &CliSession) {
  let mode = match session.mode {
    ConfigMode::Real => "real",
    ConfigMode::Mock => "mock",
  };
  println!("ribo v0.0.1（{mode} 模式）");
  for warning in &session.warnings {
    println!("  - {warning}");
  }
  println!("输入 /help 查看命令。");
}

/// `/help` 命令清单（与 TUI 帮助浮层共用 `COMMANDS` 元数据表，16 §10）。
fn print_help() {
  println!("命令：");
  for meta in crate::commands::COMMANDS {
    let alias = if meta.aliases.is_empty() {
      String::new()
    } else {
      format!("({})", meta.aliases.join(" "))
    };
    let args = if meta.args.is_empty() {
      String::new()
    } else {
      format!(" {} ", meta.args)
    };
    println!(
      "{:<16} {}",
      format!("{}{alias}{args}", meta.name),
      meta.description
    );
  }
  println!("其余输入作为消息发送给模型。");
}

/// 转录消息单行摘要：角色 + 首个文本块（`/history` 输出）。
fn summarize_message(message: &ra_core::message::Message) -> String {
  match message {
    ra_core::message::Message::User(user) => {
      let text = match &user.content {
        ra_core::message::UserContent::Text(text) => text.clone(),
        ra_core::message::UserContent::Blocks(blocks) => blocks
          .iter()
          .find_map(|b| match b {
            ra_core::message::Content::Text(t) => Some(t.content.clone()),
            _ => None,
          })
          .unwrap_or_default(),
      };
      format!("user: {text}")
    }
    ra_core::message::Message::Assistant(assistant) => {
      let text = assistant
        .content
        .iter()
        .find_map(|b| match b {
          ra_core::message::Content::Text(t) => Some(t.content.clone()),
          _ => None,
        })
        .unwrap_or_else(|| match assistant.stop_reason {
          ra_core::message::StopReason::Aborted => "（已中止）".to_string(),
          ra_core::message::StopReason::Error => "（失败）".to_string(),
          _ => "（无文本）".to_string(),
        });
      format!("assistant: {text}")
    }
    ra_core::message::Message::ToolResult(result) => {
      format!("tool_result: {}", result.tool_call_id)
    }
  }
}

/// 标准输入行源（生产入口）。
pub struct StdinLineSource {
  lines: tokio::io::Lines<tokio::io::BufReader<tokio::io::Stdin>>,
}

impl StdinLineSource {
  /// 创建标准输入行源。
  pub fn new() -> Self {
    Self {
      lines: tokio::io::BufReader::new(tokio::io::stdin()).lines(),
    }
  }
}

impl Default for StdinLineSource {
  fn default() -> Self {
    Self::new()
  }
}

impl LineSource for StdinLineSource {
  async fn next_line(&mut self) -> Option<String> {
    // IO 错误（stdin 关闭等）按 EOF 处理：交互进程不因管道断裂 panic
    self.lines.next_line().await.ok().flatten()
  }
}
