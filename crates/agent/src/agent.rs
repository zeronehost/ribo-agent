//! `BaseAgent` 门面：组装依赖、重入拒绝与注入 / 取消入口。
//!
//! 持有 [`LoopDeps`]（LLM + 工具 + 上下文边界）、会话表与广播出口；
//! 对上层暴露 prompt / steer / follow_up / stop / subscribe。
//! 同会话重入拒绝由会话的 `running` CAS 门禁承担（见 `agent_loop.rs`）。

use std::sync::Arc;

use ra_core::{error::RiboResult, message::Message};
use tokio::sync::broadcast;

use crate::{
  agent_loop::{run_agent_loop, run_agent_loop_continue},
  emit::BroadcastEmitter,
  error::session_not_found,
  events::AgentEvent,
  traits::{Agent, AgentEmitter, SessionProvider, user_text_message},
  types::{LoopConfig, LoopDeps},
};

/// 默认 Agent 实现：组装 Gateway + ToolProvider + ContextProvider +
/// SessionManager + 事件广播。
pub struct BaseAgent {
  deps: LoopDeps,
  sessions: Arc<dyn SessionProvider>,
  emitter: BroadcastEmitter,
}

impl BaseAgent {
  /// 组装门面；广播缓冲区固定 1024（满载覆盖最旧、不反压循环）。
  pub fn new(deps: LoopDeps, sessions: Arc<dyn SessionProvider>) -> Self {
    let (emitter, _) = BroadcastEmitter::new(1024);
    Self {
      deps,
      sessions,
      emitter,
    }
  }

  /// 提交一批消息并驱动循环跑完，返回本次新增消息。
  ///
  /// # Errors
  ///
  /// 会话创建失败、会话忙（重入拒绝）、上下文构建失败或迭代超限时
  /// 返回 `agent.*` 域错误。
  pub async fn prompt_messages(
    &self,
    session_key: &str,
    msgs: Vec<Message>,
  ) -> RiboResult<Vec<Message>> {
    let session = self.sessions.get_or_create(session_key).await?;

    let config = LoopConfig::for_session(&session);

    let emitter: Arc<dyn AgentEmitter> = Arc::new(self.emitter.clone());
    let report =
      run_agent_loop(&self.deps, &session, msgs, &config, emitter).await?;
    Ok(report.new_messages)
  }

  /// 续跑入口（无新 prompt）：会话不存在时返回 `agent.session_not_found`。
  ///
  /// # Errors
  ///
  /// 同 [`BaseAgent::prompt_messages`]；另加会话不存在错误。
  pub async fn continue_run(
    &self,
    session_key: &str,
  ) -> RiboResult<Vec<Message>> {
    let session = self
      .sessions
      .get(session_key)
      .await
      .ok_or_else(|| session_not_found(session_key))?;
    let config = LoopConfig::for_session(&session);
    let emitter: Arc<dyn AgentEmitter> = Arc::new(self.emitter.clone());
    let report =
      run_agent_loop_continue(&self.deps, &session, &config, emitter).await?;
    Ok(report.new_messages)
  }

  /// 向运行中会话插队一条 steering 消息；会话不存在或无活动运行时
  /// 返回 `false`（不排队）。
  pub async fn steer(&self, session_key: &str, message: Message) -> bool {
    let Some(session) = self.sessions.get(session_key).await else {
      return false;
    };
    // 仅活动运行可插队：空闲会话应走 follow_up（下次运行语义）
    if !session.is_running() {
      return false;
    }
    session.enqueue_steering(message);
    tracing::info!(session_key = %session_key, "steering message enqueued");
    true
  }

  /// 排入 follow-up：本次运行停止后驱动续跑；会话不存在则创建。
  ///
  /// # Panics
  ///
  /// 会话创建失败时 panic（内存实现不会发生；持久化实现随阶段 4
  /// 改为错误返回）。
  pub async fn follow_up(&self, session_key: &str, message: Message) {
    let session = self
      .sessions
      .get_or_create(session_key)
      .await
      .expect("get_or_create cannot fail for in-memory provider");
    session.enqueue_follow_up(message);
    tracing::info!(session_key = %session_key, "follow-up message enqueued");
  }

  /// 会话的注入队列是否仍有待处理消息。
  pub async fn has_queued_messages(&self, session_key: &str) -> bool {
    self
      .sessions
      .get(session_key)
      .await
      .is_some_and(|s| s.has_queued_messages())
  }

  /// 请求取消指定会话的当前运行；会话不存在或无活动运行时返回 `false`。
  ///
  /// 仅触发会话取消根（abort 树级联到 LLM 流与工具执行），
  /// 不直接 kill 任务；取消不是错误路径，循环以 `Aborted` 收敛。
  pub async fn stop(&self, session_key: &str) -> bool {
    let Some(session) = self.sessions.get(session_key).await else {
      return false;
    };
    if !session.is_running() {
      return false;
    }
    session.abort_cancel();
    tracing::info!(session_key = %session_key, "abort requested for active run");
    true
  }
}

#[async_trait::async_trait]
impl Agent for BaseAgent {
  async fn prompt(
    &self,
    session_key: &str,
    text: &str,
  ) -> RiboResult<Vec<Message>> {
    self
      .prompt_messages(session_key, vec![user_text_message(text)])
      .await
  }

  async fn stop(&self, session_key: &str) -> bool {
    BaseAgent::stop(self, session_key).await
  }

  async fn has_active_tasks(&self, session_key: &str) -> bool {
    self
      .sessions
      .get(session_key)
      .await
      .is_some_and(|s| s.is_running())
  }

  fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
    self.emitter.subscribe()
  }
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use ra_core::{
    error::Severity,
    message::{Message, StopReason},
  };
  use ra_provider::adapters::mock::MockStep;

  use super::*;
  use crate::{
    error::{CONTEXT_BUILD_ERROR, SESSION_BUSY, SESSION_NOT_FOUND},
    session::SessionManager,
    test_support::*,
  };

  /// 构造挂载 mock 网关与工具桩的 BaseAgent，附会话表句柄。
  fn test_agent(
    steps: Vec<MockStep>,
    tools: MockTools,
  ) -> (BaseAgent, Arc<SessionManager>) {
    let sessions = Arc::new(SessionManager::new());
    let deps = make_deps(mock_gateway(steps), Arc::new(tools));
    (BaseAgent::new(deps, sessions.clone()), sessions)
  }

  // 场景：prompt 正常完成。
  // 预期：返回本次新增消息（用户输入 + 助手回复），会话运行状态复位。
  #[tokio::test]
  async fn prompt_returns_new_messages() {
    let (agent, sessions) = test_agent(
      vec![complete_step(StopReason::Stop, text_assistant("reply"))],
      MockTools::new(ToolBehavior::Ok),
    );

    let messages = agent.prompt("s1", "hi").await.expect("正常完成");

    assert_eq!(messages.len(), 2);
    assert!(matches!(&messages[0], Message::User(_)));
    assert!(matches!(&messages[1], Message::Assistant(_)));

    let session = sessions.get("s1").await.expect("会话已创建");
    assert_eq!(session.snapshot(), messages);
    assert!(!session.is_running());
  }

  // 场景：subscribe 订阅生命周期事件。
  // 预期：一次运行收到 agent_start..agent_end 全序列，恰好一个终态事件。
  #[tokio::test]
  async fn subscribe_receives_lifecycle_events() {
    let (agent, _sessions) = test_agent(
      vec![complete_step(StopReason::Stop, text_assistant("reply"))],
      MockTools::new(ToolBehavior::Ok),
    );
    let mut rx = agent.subscribe();

    agent.prompt("s1", "hi").await.expect("正常完成");

    let mut tags = Vec::new();
    loop {
      match rx.try_recv() {
        Ok(event) => {
          let terminal = event.is_terminal();
          tags.push(event.type_tag());
          if terminal {
            break;
          }
        }
        Err(broadcast::error::TryRecvError::Empty) => break,
        Err(other) => panic!("订阅端不应出错: {other:?}"),
      }
    }
    assert_eq!(
      tags,
      vec![
        "agent_start",
        "turn_start",
        "message_start",
        "message_end",
        "message_start",
        "message_end",
        "turn_end",
        "agent_end",
      ]
    );
  }

  // 场景：continue_run 会话不存在。
  // 预期：返回 agent.session_not_found（Warning 级）。
  #[tokio::test]
  async fn continue_run_missing_session_is_not_found() {
    let (agent, _sessions) =
      test_agent(Vec::new(), MockTools::new(ToolBehavior::Ok));

    let err = agent
      .continue_run("ghost")
      .await
      .expect_err("会话不存在应报错");

    assert_eq!(err.code(), SESSION_NOT_FOUND);
    assert_eq!(err.severity(), &Severity::Warning);
  }

  // 场景：continue_run 空转录（会话已存在但无消息）。
  // 预期：返回 agent.context_build_error，运行状态复位。
  #[tokio::test]
  async fn continue_run_empty_transcript_fails_context_build() {
    let (agent, sessions) =
      test_agent(Vec::new(), MockTools::new(ToolBehavior::Ok));
    let session = sessions.get_or_create("s1").await.expect("预建会话");

    let err = agent.continue_run("s1").await.expect_err("空转录不可续跑");

    assert_eq!(err.code(), CONTEXT_BUILD_ERROR);
    assert!(!session.is_running());
  }

  // 场景：continue_run 转录尾条为助手消息（续跑前置不满足）。
  // 预期：返回 agent.context_build_error，运行状态复位。
  #[tokio::test]
  async fn continue_run_ending_with_assistant_fails_context_build() {
    let (agent, sessions) =
      test_agent(Vec::new(), MockTools::new(ToolBehavior::Ok));
    let session = sessions.get_or_create("s1").await.expect("预建会话");
    session.append_messages(&[Message::from(text_assistant("done"))]);

    let err = agent
      .continue_run("s1")
      .await
      .expect_err("尾条为助手消息不可续跑");

    assert_eq!(err.code(), CONTEXT_BUILD_ERROR);
    assert!(!session.is_running());
  }

  // 场景：steer 会话不存在。
  // 预期：返回 false（不排队）。
  #[tokio::test]
  async fn steer_missing_session_returns_false() {
    let (agent, _sessions) =
      test_agent(Vec::new(), MockTools::new(ToolBehavior::Ok));

    let queued = agent.steer("ghost", user_text_message("inject")).await;

    assert!(!queued);
  }

  // 场景：steer 会话存在但无活动运行。
  // 预期：返回 false（空闲会话应走 follow-up 语义），队列保持为空。
  #[tokio::test]
  async fn steer_idle_session_returns_false() {
    let (agent, sessions) =
      test_agent(Vec::new(), MockTools::new(ToolBehavior::Ok));
    sessions.get_or_create("s1").await.expect("预建会话");

    let queued = agent.steer("s1", user_text_message("inject")).await;

    assert!(!queued);
    assert!(!agent.has_queued_messages("s1").await);
  }

  // 场景：stop 会话不存在或无活动运行。
  // 预期：均返回 false，取消根不被触发。
  #[tokio::test]
  async fn stop_returns_false_without_active_run() {
    let (agent, sessions) =
      test_agent(Vec::new(), MockTools::new(ToolBehavior::Ok));

    assert!(!agent.stop("ghost").await);

    let session = sessions.get_or_create("s1").await.expect("预建会话");
    assert!(!agent.stop("s1").await);
    assert!(!session.cancel_is_aborted());
  }

  // 场景：follow_up 会话不存在时创建并入队。
  // 预期：has_queued_messages 观测到待处理消息。
  #[tokio::test]
  async fn follow_up_queues_message_and_creates_session() {
    let (agent, _sessions) =
      test_agent(Vec::new(), MockTools::new(ToolBehavior::Ok));

    agent.follow_up("s1", user_text_message("next")).await;

    assert!(agent.has_queued_messages("s1").await);
  }

  // 场景：continue_run 正常续跑（转录尾条非助手消息）。
  // 预期：返回续跑产出的助手回复，运行状态复位。
  #[tokio::test]
  async fn continue_run_completes_from_existing_transcript() {
    let (agent, sessions) = test_agent(
      vec![complete_step(StopReason::Stop, text_assistant("resumed"))],
      MockTools::new(ToolBehavior::Ok),
    );
    // 预置转录：尾条为 User（满足续跑前置校验）
    let session = sessions.get_or_create("s1").await.expect("会话就绪");
    session.append_messages(&[user_text_message("earlier")]);

    let messages = agent.continue_run("s1").await.expect("续跑应正常完成");

    assert_eq!(messages.len(), 1);
    let Message::Assistant(assistant) = &messages[0] else {
      panic!("新增消息应为助手回复，实际 {:?}", messages[0])
    };
    assert_eq!(assistant.stop_reason, StopReason::Stop);
    assert!(!agent.has_active_tasks("s1").await);
  }

  // 场景：steer 向运行中会话插队。
  // 预期：返回 true，消息进入 steering 队列（has_queued_messages 为真）。
  #[tokio::test]
  async fn steer_enqueues_for_running_session() {
    let (agent, sessions) =
      test_agent(Vec::new(), MockTools::new(ToolBehavior::Ok));
    let session = sessions.get_or_create("s1").await.expect("会话就绪");
    // 手动置位运行标志，模拟活动运行中的会话
    session
      .running
      .store(true, std::sync::atomic::Ordering::Release);

    assert!(agent.steer("s1", user_text_message("inject")).await);
    assert!(session.has_queued_messages());
  }

  // 场景：Agent trait 的 stop / has_active_tasks 委托固有方法。
  // 预期：经 trait 对象调用，无会话时 stop 返回 false、无活动任务。
  #[tokio::test]
  async fn agent_trait_stop_delegates_to_inherent_method() {
    let (agent, _sessions) =
      test_agent(Vec::new(), MockTools::new(ToolBehavior::Ok));
    let trait_object: Arc<dyn Agent> = Arc::new(agent);

    assert!(!trait_object.stop("missing").await);
    assert!(!trait_object.has_active_tasks("missing").await);
  }

  // 场景：BaseAgent 级重入拒绝与 stop 取消（挂起工具 + 信号收敛）。
  // 预期：工具挂起期间第二次 prompt 返回 agent.session_busy；stop 触发后
  // 首次运行以 Ok（Aborted 收敛）返回，运行标志复位。
  #[tokio::test]
  async fn reentry_rejected_then_stop_aborts_first_run() {
    let (agent, _sessions) = test_agent(
      vec![complete_step(
        StopReason::ToolUse,
        assistant_with_tool_calls(vec![tool_call(
          "c1",
          "echo",
          serde_json::json!({ "text": "hi" }),
        )]),
      )],
      MockTools::new(ToolBehavior::Hang),
    );
    let agent = Arc::new(agent);

    // ===== 阶段 1：首次运行进入工具挂起 =====
    let first = {
      let agent = agent.clone();
      tokio::spawn(async move { agent.prompt("s1", "hi").await })
    };
    while !agent.has_active_tasks("s1").await {
      tokio::task::yield_now().await;
    }

    // ===== 阶段 2：重入被 CAS 门禁拒绝 =====
    let err = agent
      .prompt("s1", "second")
      .await
      .expect_err("重入应被拒绝");
    assert_eq!(err.code(), SESSION_BUSY);

    // ===== 阶段 3：stop 触发取消，首次运行以中止收敛 =====
    assert!(agent.stop("s1").await);
    let messages = first
      .await
      .expect("任务不应 panic")
      .expect("中止路径应正常返回");
    assert!(!messages.is_empty());
    assert!(!agent.has_active_tasks("s1").await);

    // 运行结束后广播出口仍可正常订阅（emitter 生命周期独立于单次运行）
    let mut rx = agent.subscribe();
    agent
      .prompt("s1", "after stop")
      .await
      .expect("再次运行正常");
    assert!(matches!(rx.try_recv(), Ok(AgentEvent::AgentStart)));
  }
}
