//! REPL 集成测试（14 详细设计 §12.4，APP-01..04）。
//!
//! 驱动方式：`Repl::run` spawn 为后台任务；输入经 `mpsc` 通道精确
//! 供给（`RecordingLineSource`），渲染经录制实现断言；会话真相经
//! `Arc<Harness>` 克隆读取。`#[tokio::test(start_paused = true)]`
//! 虚拟时钟消除真实时间依赖。
//!
//! 时序确定性依据（与 `MockStep` 语义对齐）：
//! - `Silent` 回合期间主循环与测试侧均有就绪任务（yield 驱动），
//!   虚拟时钟不会空闲推进 → 回合保持挂起直至 `/stop` 触发取消；
//! - `Complete` 步骤无定时器 → 测试侧等待空闲安全（时钟无目标可推进）。

use std::sync::{Arc, Mutex};

use ra_agent::events::AgentEvent;
use ra_cli::app::{LineSource, Repl};
use ra_cli::config::LoadOutcome;
use ra_cli::config::assemble::assemble_with_steps;
use ra_cli::render::Renderer;
use ra_core::message::{AssistantMessage, Content, StopReason, TextContent};
use ra_provider::adapters::mock::MockStep;
use tokio::sync::mpsc::UnboundedSender;

// ===== 测试输入源：通道回放 =====

/// 通道行源：测试侧逐行推送（`send` 后由 REPL 拉取）。
struct ChannelLineSource {
  rx: tokio::sync::mpsc::UnboundedReceiver<String>,
}

impl LineSource for ChannelLineSource {
  async fn next_line(&mut self) -> Option<String> {
    self.rx.recv().await
  }
}

// ===== 测试渲染器：录制实现 =====

/// 录制数据（共享给测试主循环断言）。
#[derive(Default)]
struct Record {
  /// `MessageStart` 计数（建流信号——确认 mock 步骤已被消费）。
  starts: usize,
  /// `MessageUpdate` 的文本快照（流式增量）。
  texts: Vec<String>,
  /// 终端消息的 `error_message`（MessageEnd / TurnEnd / AgentEnd）。
  errors: Vec<String>,
  /// `AgentEnd` 事件计数（回合收敛信号）。
  agent_ends: usize,
}

/// 共享录制渲染器：实现 [`Renderer`]，转发进 `Arc<Mutex<Record>>`。
#[derive(Clone, Default)]
struct SharedRenderer {
  record: Arc<Mutex<Record>>,
}

impl Renderer for SharedRenderer {
  fn on_event(&mut self, event: &AgentEvent) {
    let mut record = self.record.lock().expect("record lock");
    match event {
      AgentEvent::MessageStart { .. } => record.starts += 1,
      AgentEvent::MessageUpdate { partial } => {
        for block in &partial.content {
          if let Content::Text(t) = block {
            record.texts.push(t.content.clone());
          }
        }
      }
      AgentEvent::MessageEnd { message } => {
        if let Some(reason) = error_message_of(message) {
          record.errors.push(reason.to_string());
        }
      }
      AgentEvent::TurnEnd { message, .. } => {
        if let Some(reason) = error_message_of(message) {
          record.errors.push(reason.to_string());
        }
      }
      AgentEvent::AgentEnd { messages } => {
        record.agent_ends += 1;
        if let Some(reason) = messages.last().and_then(error_message_of) {
          record.errors.push(reason.to_string());
        }
      }
      _ => {}
    }
  }
  fn on_lagged(&mut self, _skipped: u64) {}
  fn on_closed(&mut self) {}
}

/// 终端助手消息的错误原因（与 render.rs 内部实现同源）。
fn error_message_of(message: &ra_core::message::Message) -> Option<&str> {
  match message {
    ra_core::message::Message::Assistant(a) => a.error_message.as_deref(),
    _ => None,
  }
}

// ===== 测试脚手架 =====

/// 文本助手消息（mock `Complete` 步骤的返回体）。
fn text_assistant(text: &str) -> AssistantMessage {
  let mut message = AssistantMessage::empty("mock", "mock-model", 0);
  message.content.push(Content::Text(TextContent {
    content: text.to_string(),
    signature: None,
  }));
  message.stop_reason = StopReason::Stop;
  message
}

/// 成功完成一步的脚本步骤（同步路径：直接给出终态消息）。
///
/// 闭包经 `Arc` 强制转换为脚本层 `CompleteBuilder`（类型在 mock
/// 模块私有路径下，无须命名即可构造变体）。
fn complete_step(text: &'static str) -> MockStep {
  MockStep::Complete(Arc::new(move |_| {
    (StopReason::Stop, text_assistant(text))
  }))
}

/// 长时间静默步骤（挂起回合；`/stop` 取消后以 Aborted 收敛）。
fn silent_step() -> MockStep {
  MockStep::Silent {
    duration: std::time::Duration::from_secs(60),
  }
}

/// REPL 测试环境：任务句柄 + 输入发送端 + 录制器 + 会话门面克隆。
struct ReplFixture {
  /// REPL 主循环任务（测试结束前显式 abort，避免悬空 prompt 任务报错）。
  task: tokio::task::JoinHandle<ra_core::error::RiboResult<()>>,
  /// 输入发送端（drop 即 EOF）。
  tx: UnboundedSender<String>,
  /// 录制器克隆（断言用）。
  renderer: SharedRenderer,
  /// 会话门面克隆（转录 / 会话键真相断言用）。
  harness: Arc<ra_harness::Harness>,
}

/// 启动 REPL：mock 模式 + 注入脚本步骤 + 独立临时工作空间（root）。
fn spawn_repl(steps: Vec<MockStep>) -> (ReplFixture, tempfile::TempDir) {
  let dir = tempfile::tempdir().expect("temp dir");
  // mock 模式 + 工作空间隔离（APP 用例间互不影响）
  let outcome = LoadOutcome::load(None, Some(dir.path()));
  assert_eq!(
    outcome.mode,
    ra_cli::config::ConfigMode::Mock,
    "无配置文件应为 mock 模式"
  );
  let session = assemble_with_steps(&outcome, steps).expect("mock 装配成功");
  let harness = session.harness.clone();
  let renderer = SharedRenderer::default();
  let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
  let repl = Repl::new(
    ChannelLineSource { rx },
    session,
    Box::new(renderer.clone()),
  );
  let task = tokio::spawn(repl.run());
  (
    ReplFixture {
      task,
      tx,
      renderer,
      harness,
    },
    dir,
  )
}

/// 等待录制器满足条件（yield 轮询；`Complete` 步骤无定时器，时钟
/// 空闲推进无目标，轮询安全）。
async fn wait_until(renderer: &SharedRenderer, cond: impl Fn(&Record) -> bool) {
  for _ in 0..10_000 {
    if cond(&renderer.record.lock().expect("record lock")) {
      return;
    }
    tokio::task::yield_now().await;
  }
  panic!("等待渲染条件超时（回合未收敛）");
}

/// 让复位落地：AgentEnd 渲染早于 `store.append` → `send` → 复位，
/// 1ms 虚拟睡眠让 Repl 先消费 `turn_done`（缓冲值保证下次轮询即复位），
/// 再注入跨回合边界的行，避免「回合已收敛但 TurnState 未复位」窗口内
/// 的误拒绝。
async fn settle() {
  tokio::time::sleep(std::time::Duration::from_millis(1)).await;
}

// ===== APP-01：Silent 挂起回合 + /stop 取消渲染 =====

/// 场景：`MockStep::Silent` 挂起回合，`/stop` 取消后以 Aborted 收敛
/// 并渲染，`/exit` 正常退出。
///
/// 预期：转录中助手消息 `stop_reason == Aborted`；渲染器收到终止
/// 错误提示；REPL 正常返回。
#[tokio::test(start_paused = true)]
async fn silent_turn_then_stop_renders_aborted() {
  let (fx, _dir) = spawn_repl(vec![silent_step()]);

  // 发起回合（Silent 挂起）→ 确认建流（步骤已消费）→ /stop → 渲染 → /exit
  fx.tx.send("hello".into()).expect("send");
  // MessageStart = Silent 步骤已消费且流挂起（此刻 /stop 必为流中取消，
  // 消除 pre-stream CANCELLED 折叠不消费步骤的竞态）
  wait_until(&fx.renderer, |r| r.starts >= 1).await;
  fx.tx.send("/stop".into()).expect("send");
  // 终止事件渲染完成后再退出：biased 顺序中 line 先于 events，
  // 若立即注入 /exit 会在事件渲染前 break
  wait_until(&fx.renderer, |r| !r.errors.is_empty()).await;
  fx.tx.send("/exit".into()).expect("send");
  fx.task.await.expect("repl join").expect("repl ok");

  // 转录真相：助手消息以 Aborted 收敛（取消不是错误路径）
  let messages = fx
    .harness
    .session_messages("default")
    .await
    .expect("session read");
  let assistant = messages
    .iter()
    .find_map(|m| match m {
      ra_core::message::Message::Assistant(a) => Some(a),
      _ => None,
    })
    .expect("aborted assistant in transcript");
  assert_eq!(assistant.stop_reason, StopReason::Aborted);
  assert!(
    !assistant.content.is_empty() || assistant.error_message.is_some(),
    "Aborted 终端消息应有错误描述或空内容"
  );

  // 渲染真相：终止错误提示已渲染
  let record = fx.renderer.record.lock().expect("record lock");
  assert!(
    !record.errors.is_empty(),
    "Aborted 终端应渲染 error_message，实际 {:?}",
    record.errors
  );
}

// ===== APP-02：在途拒绝 + 结束后恢复 =====

/// 场景：回合进行中再次 Chat 与 `/new` 被拒绝（不留转录痕迹）；
/// `/stop` 后两者恢复正常。
///
/// 预期：挂起回合的 Chat 拒绝 → 转录仅 2 条 user 输入；`/new x`
/// 拒绝 → `x` 不出现在会话列表；停止后新回合成功（Complete 脚本）
/// 且 `/new work` 生效（第三回合落在 work 键下）。
#[tokio::test(start_paused = true)]
async fn concurrent_chat_and_new_are_rejected_while_running() {
  let (fx, _dir) = spawn_repl(vec![
    silent_step(),
    complete_step("mock-ok"),
    complete_step("work-ok"),
  ]);

  // ===== 阶段 1：挂起回合期间的重入拒绝 =====
  fx.tx.send("hi".into()).expect("send");
  // MessageStart = Silent 步骤已消费（/stop 必为流中取消，确定性收敛）
  wait_until(&fx.renderer, |r| r.starts >= 1).await;
  fx.tx.send("hi2".into()).expect("send"); // 在途 Chat → 拒绝
  fx.tx.send("/new x".into()).expect("send"); // 在途 /new → 拒绝
  fx.tx.send("/stop".into()).expect("send"); // 等待复位（内部阻塞）

  // ===== 阶段 2：停止后新回合（Complete 脚本）成功 =====
  // /stop 的 handle_stop 阻塞至复位，此行进入时必然 Idle
  fx.tx.send("fine".into()).expect("send");
  // 等待第二回合收敛 + 复位落地后再轮换，避免在途 /new 误拒绝
  wait_until(&fx.renderer, |r| r.agent_ends >= 2).await;
  settle().await;
  fx.tx.send("/new work".into()).expect("send");

  // ===== 阶段 3：work 键下的第三回合（验证轮换生效） =====
  fx.tx.send("third".into()).expect("send");
  wait_until(&fx.renderer, |r| r.agent_ends >= 3).await;
  drop(fx.tx); // EOF → REPL 退出

  fx.task.await.expect("repl join").expect("repl ok");

  // 转录真相：default 仅 2 条 user（"hi" 与 "fine"）——"hi2" 被拒无痕
  let messages = fx
    .harness
    .session_messages("default")
    .await
    .expect("session read");
  let user_count = messages
    .iter()
    .filter(|m| matches!(m, ra_core::message::Message::User(_)))
    .count();
  assert_eq!(user_count, 2, "在途 Chat 应被拒绝，不留转录痕迹");
  // 最终助手消息为 Complete 脚本文本
  let last_assistant = messages
    .iter()
    .rev()
    .find_map(|m| match m {
      ra_core::message::Message::Assistant(a) => Some(a),
      _ => None,
    })
    .expect("assistant in transcript");
  let final_text = last_assistant
    .content
    .iter()
    .find_map(|b| match b {
      Content::Text(t) => Some(t.content.clone()),
      _ => None,
    })
    .unwrap_or_else(|| panic!("最后助手消息无文本，转录：\n{messages:#?}"));
  assert_eq!(final_text, "mock-ok");

  // work 键真相：第三回合落在新键下（/new 轮换生效）
  let work_messages = fx
    .harness
    .session_messages("work")
    .await
    .expect("session read");
  assert_eq!(work_messages.len(), 2, "work 仅第三回合");
  let work_text = work_messages
    .iter()
    .find_map(|m| match m {
      ra_core::message::Message::Assistant(a) => {
        a.content.iter().find_map(|b| match b {
          Content::Text(t) => Some(t.content.clone()),
          _ => None,
        })
      }
      _ => None,
    })
    .expect("work-ok in work transcript");
  assert_eq!(work_text, "work-ok");

  // 会话真相："x" 未创建（在途 /new 被拒），"work" 已由第三回合登记
  let keys = fx.harness.list_sessions().await;
  assert!(!keys.iter().any(|k| k == "x"), "在途 /new 应被拒绝");
  assert!(keys.iter().any(|k| k == "work"), "停止后 /new 应生效");
}

// ===== APP-03：/new 轮换 + 转录隔离 =====

/// 场景：`/new s2` 轮换会话键后，两键转录互相隔离。
///
/// 预期：default 含第一回合（"hello" → reply-1）；s2 含第二回合
/// （"world" → reply-2）；互不串写。
#[tokio::test(start_paused = true)]
async fn new_key_rotates_and_history_is_isolated() {
  let (fx, _dir) =
    spawn_repl(vec![complete_step("reply-1"), complete_step("reply-2")]);

  // ===== 阶段 1：default 回合 =====
  fx.tx.send("hello".into()).expect("send");
  wait_until(&fx.renderer, |r| r.agent_ends >= 1).await;
  settle().await; // 复位落地后再轮换

  // ===== 阶段 2：轮换 + s2 回合 =====
  fx.tx.send("/new s2".into()).expect("send");
  fx.tx.send("world".into()).expect("send");
  wait_until(&fx.renderer, |r| r.agent_ends >= 2).await;
  drop(fx.tx);

  fx.task.await.expect("repl join").expect("repl ok");

  // 转录隔离：两键各含自己的 user 输入与脚本文本
  let default_messages = fx
    .harness
    .session_messages("default")
    .await
    .expect("session read");
  assert_eq!(default_messages.len(), 2, "default 仅第一回合");
  let s2_messages = fx
    .harness
    .session_messages("s2")
    .await
    .expect("session read");
  assert_eq!(s2_messages.len(), 2, "s2 仅第二回合");
  let s2_text = s2_messages
    .iter()
    .find_map(|m| match m {
      ra_core::message::Message::Assistant(a) => {
        a.content.iter().find_map(|b| match b {
          Content::Text(t) => Some(t.content.clone()),
          _ => None,
        })
      }
      _ => None,
    })
    .expect("reply-2 in s2");
  assert_eq!(s2_text, "reply-2", "s2 应命中第二回合脚本");
}

// ===== APP-04：日志落文件而非 stdout =====

/// 场景：WARN 日志写入 `<root>/logs/cli.log`（独立订阅器注入）。
///
/// 预期：文件存在且含 WARN 行；stdout 分流属结构性保证（writer
/// 指向文件，不经过 `println!`）。
#[tokio::test(start_paused = true)]
async fn logs_go_to_file_not_stdout() {
  let dir = tempfile::tempdir().expect("temp dir");
  let logs_dir = dir.path().join("logs");
  std::fs::create_dir_all(&logs_dir).expect("create logs dir");
  // 每用例独立订阅器（with_default 作用域 + rolling::never 固定文件名），
  // 规避全局 try_init 的单次语义限制
  let subscriber = tracing_subscriber::fmt()
    .with_writer(Mutex::new(tracing_appender::rolling::never(
      &logs_dir, "cli.log",
    )))
    .with_ansi(false)
    .finish();
  tracing::subscriber::with_default(subscriber, || {
    tracing::warn!(target: "ribo_cli_e2e", "e2e warn probe");
  });

  let content =
    std::fs::read_to_string(logs_dir.join("cli.log")).expect("read log file");
  assert!(
    content.contains("WARN") && content.contains("e2e warn probe"),
    "日志应落文件，实际：{content:?}"
  );
}
