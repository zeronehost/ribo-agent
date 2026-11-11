//! 主循环的运行时类型：请求上下文、运行配置、结果报告与依赖集合。

use std::sync::{Arc, Mutex};

use ra_core::message::{Content, Message};
use ra_provider::{gateway::Gateway, ids::ModelId};
use tokio_util::sync::CancellationToken;

use crate::{
  queue::PendingMessageQueue,
  session::Session,
  traits::{ContextProvider, ToolProvider},
};

/// 主循环的请求上下文：每轮 LLM 请求前的完整状态。
///
/// 由 [`ContextProvider`](crate::traits::ContextProvider) 构建并逐轮演进：
/// `messages` 随助手消息、工具结果回灌与队列注入不断追加。
#[derive(Debug, Clone, Default)]
pub struct AgentContext {
  /// 系统提示（多模态，空表示无）。
  pub system_prompt: Vec<Content>,
  /// 对话转录（含 prompt、注入消息、助手消息、工具结果）。
  pub messages: Vec<Message>,
}

/// 主循环运行配置。
///
/// 队列与取消信号按会话注入：[`LoopConfig::for_session`] 复用会话内聚的
/// 队列与取消根，使外部注入（steer / follow_up）与停止（stop）能
/// 直接影响运行中的循环。
#[derive(Debug, Clone)]
pub struct LoopConfig {
  /// 最大迭代轮数（一"轮" = 一次助手响应 + 其工具批），超限返回
  /// `agent.max_iterations` 错误。
  pub max_iterations: usize,
  /// steering 队列：轮间注入（工具批落定后、下次 LLM 调用前排水）。
  pub steering: Arc<Mutex<PendingMessageQueue>>,
  /// follow-up 队列：本应停止时排水，续跑新一轮。
  pub follow_up: Arc<Mutex<PendingMessageQueue>>,
  /// 取消信号（abort 树根，来自会话的取消根 `AbortHandle`）。
  pub signal: CancellationToken,
}

impl LoopConfig {
  /// 默认最大迭代轮数。
  pub const DEFAULT_MAX_ITERATIONS: usize = 25;

  /// 由会话构造配置：复用会话内聚的队列与取消根。
  ///
  /// steering / follow-up 队列与取消令牌均克隆自会话字段，
  /// 保证循环内排水与会话上的注入接口操作同一状态。
  pub fn for_session(session: &Session) -> Self {
    Self {
      max_iterations: Self::DEFAULT_MAX_ITERATIONS,
      steering: session.steering_queue.clone(),
      follow_up: session.follow_up_queue.clone(),
      signal: session.cancel_token(),
    }
  }

  /// 覆盖最大迭代轮数（builder 风格，消费并返回自身）。
  pub fn with_max_iterations(mut self, max_iterations: usize) -> Self {
    self.max_iterations = max_iterations;
    self
  }
}

/// 一次运行结束的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopOutcome {
  /// 正常完成：助手消息给出 Stop / ToolUse 尽头 / Length(无工具) 等
  /// 非错误停止。
  Completed,
  /// 被取消：终止助手消息为 `StopReason::Aborted` 或取消信号触发。
  Aborted,
  /// 失败：终止助手消息为 `StopReason::Error`。
  Failed,
}

/// 一次运行的产出报告。
#[derive(Debug, Clone)]
pub struct LoopReport {
  /// 结束原因。
  pub outcome: LoopOutcome,
  /// 本次运行新增的消息（prompt + 注入 + 助手 + 工具结果），转录序。
  pub new_messages: Vec<Message>,
}

/// 循环依赖集合：LLM 边界、工具边界与上下文构建边界（D2/D3）。
///
/// 手工实现 [`Debug`]：trait 对象字段（`Arc<dyn ToolProvider>` 等）
/// 不可打印，只输出模型标识等确定性字段，避免泄露依赖内部状态。
#[derive(Clone)]
pub struct LoopDeps {
  /// LLM 网关（内含并发准入 / 熔断 / 重试 / 降级，不装箱为 Provider，D2）。
  pub gateway: Arc<Gateway>,
  /// 本轮请求的模型。
  pub model: ModelId,
  /// 工具边界（ra-tools `ToolManager` 经 `ToolManagerAdapter` 注入）。
  pub tools: Arc<dyn ToolProvider>,
  /// 上下文构建边界（本期直通实现见 `PassthroughContextProvider`）。
  pub context_provider: Arc<dyn ContextProvider>,
}

impl core::fmt::Debug for LoopDeps {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    // trait 对象不可打印，只输出模型标识等确定性字段
    f.debug_struct("LoopDeps")
      .field("model", &self.model.as_str())
      .finish_non_exhaustive()
  }
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use super::*;
  use crate::{session::Session, test_support::*};

  // 场景：AgentContext 默认值。
  // 预期：系统提示与转录均为空（Clone/Default 派生语义正确）。
  #[test]
  fn agent_context_default_is_empty() {
    let context = AgentContext::default();

    assert!(context.system_prompt.is_empty());
    assert!(context.messages.is_empty());
  }

  // 场景：LoopConfig::for_session 复用会话内聚的队列与取消根。
  // 预期：默认 25 轮；steering 队列与会话共享（会话入队后 config 侧可观测）；
  // abort 会话取消根后 config 信号同步触发。
  #[test]
  fn for_session_shares_queues_and_signal() {
    let session = Session::new("s1");
    let config = LoopConfig::for_session(&session);

    assert_eq!(config.max_iterations, LoopConfig::DEFAULT_MAX_ITERATIONS);
    assert_eq!(LoopConfig::DEFAULT_MAX_ITERATIONS, 25);

    // 队列共享：会话入队后 config 侧锁内可见（非克隆副本）
    session.enqueue_steering(crate::traits::user_text_message("inject"));
    assert_eq!(config.steering.lock().expect("steering 锁有效").len(), 1);

    // 信号联动：会话取消根触发后 config 信号同步取消
    assert!(!config.signal.is_cancelled());
    session.abort_cancel();
    assert!(config.signal.is_cancelled());
  }

  // 场景：with_max_iterations builder 覆盖默认轮数。
  // 预期：消费并返回自身，新值生效且不影响其他字段。
  #[test]
  fn with_max_iterations_overrides_limit() {
    let session = Session::new("s1");
    let config = LoopConfig::for_session(&session).with_max_iterations(1);

    assert_eq!(config.max_iterations, 1);
  }

  // 场景：LoopOutcome 三变体的相等性。
  // 预期：同变体相等、异变体不等（收敛判定依赖 PartialEq）。
  #[test]
  fn loop_outcome_variants_compare() {
    assert_eq!(LoopOutcome::Completed, LoopOutcome::Completed);
    assert_eq!(LoopOutcome::Aborted, LoopOutcome::Aborted);
    assert_eq!(LoopOutcome::Failed, LoopOutcome::Failed);
    assert_ne!(LoopOutcome::Completed, LoopOutcome::Aborted);
    assert_ne!(LoopOutcome::Aborted, LoopOutcome::Failed);
    assert_ne!(LoopOutcome::Completed, LoopOutcome::Failed);
  }

  // 场景：LoopDeps 手工 Debug 输出（trait 对象字段不可打印）。
  // 预期：输出含结构名与模型标识，不含 trait 对象内部状态。
  #[test]
  fn loop_deps_debug_contains_model_id() {
    let deps = make_deps(
      mock_gateway(Vec::new()),
      Arc::new(MockTools::new(ToolBehavior::Ok)),
    );
    let debug = format!("{deps:?}");

    assert!(debug.contains("LoopDeps"), "调试输出应含结构名: {debug}");
    assert!(
      debug.contains("mock-model"),
      "调试输出应含模型标识: {debug}"
    );
  }

  // 场景：LoopReport 构造与 Clone（AgentEnd 载荷复用）。
  // 预期：字段原样携带，克隆体与原件相等。
  #[test]
  fn loop_report_clones_with_messages() {
    let report = LoopReport {
      outcome: LoopOutcome::Completed,
      new_messages: vec![crate::traits::user_text_message("hi")],
    };
    let clone = report.clone();

    assert_eq!(clone.outcome, LoopOutcome::Completed);
    assert_eq!(clone.new_messages, report.new_messages);
  }
}
