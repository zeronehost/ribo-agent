//! Agent 主循环：LLM 调用 → ToolCall 收集 → 工具批执行 → 结果回灌 →
//! 终止判定。
//!
//! 关键语义：
//!
//! - **重入双检查**：`running` CAS 快速拒绝 + `run_lock.try_lock` 兜底
//!   竞态窗口，两者任一失败均返回 `agent.session_busy`；
//! - **截断失败不变式**：`StopReason::Length` 且带工具调用时全部原位
//!   失败回灌，不执行任何工具（照常续跑）；
//! - **事件序解耦**：并行批 `ToolExecutionEnd` 按完成序发射，
//!   回灌消息按源顺序发射；
//! - **错误回灌**：工具失败以 `is_error: true` 结果回灌模型，
//!   循环继续；`tool.cancelled` 触发批量终止；
//! - **错误路径也写回转录**：入口收尾统一执行（包括
//!   `max_iterations` 失败），已产出消息不丢失。

use std::sync::{Arc, atomic::Ordering};

use ra_core::{
  error::{RiboError, RiboResult},
  message::{
    AssistantMessage, Content, Message, StopReason, TextContent, ToolCall,
    ToolResultMessage,
  },
  utils::time::now_ms,
};
use ra_provider::{
  options::{CancelToken, StreamOptions},
  stream::ProviderEvent,
};
use ra_tools::{
  context::ToolContext,
  types::{ToolExecutionMode, ToolExecutionResult, ToolOutput},
};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::{
  error::{context_build_error, max_iterations, session_busy, tool_exec_error},
  events::AgentEvent,
  session::Session,
  traits::{AgentEmitter, ToolProvider},
  types::{AgentContext, LoopConfig, LoopDeps, LoopOutcome, LoopReport},
};

/// 单条工具调用的终结产物：结果或错误（含源顺序索引，供回灌排序）。
struct FinalizedToolCall {
  /// 在源 `ToolCall` 列表中的下标（回灌按源顺序排列）。
  index: usize,
  tool_call_id: String,
  tool_name: String,
  result: RiboResult<ToolExecutionResult>,
}

/// 一次工具批的执行产物。
struct ExecutedToolBatch {
  /// 回灌消息（按源顺序排列）。
  messages: Vec<ToolResultMessage>,
  /// 是否应终结整个循环（`tool.cancelled` 或结果声明 `terminate`）。
  terminated: bool,
}

/// 由一批输入消息驱动一次完整运行（pi-rust `run_agent_loop` 同构）。
///
/// `input` 为空时按续跑语义校验转录（非空且最后一条不是助手消息），
/// 等价 [`run_agent_loop_continue`]。
///
/// # Errors
///
/// - 会话忙（重入拒绝，CAS / `try_lock` 双检查）：`agent.session_busy`；
/// - 空 input 且转录不可续跑：`agent.context_build_error`；
/// - 上下文构建失败：透传 `agent.context_build_error`；
/// - 超过 `max_iterations`：`agent.max_iterations`。
///
/// 失败路径同样会把已产出消息写回转录（消息不丢失），并复位会话
/// 运行状态。
pub async fn run_agent_loop(
  deps: &LoopDeps,
  session: &Session,
  input: Vec<Message>,
  config: &LoopConfig,
  emitter: Arc<dyn AgentEmitter>,
) -> RiboResult<LoopReport> {
  // ===== 阶段 1：重入双检查 =====
  // 快速路径：running CAS 置位失败即已有活动运行
  if session
    .running
    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
    .is_err()
  {
    tracing::warn!(session_key = %session.key, "reentry rejected by CAS gate");
    return Err(session_busy(&session.key));
  }

  // 兜底路径：try_lock 探测失败说明 CAS 与锁之间存在未覆盖的竞态窗口，
  // 此时必须先复位 running 再返回，避免会话永久卡死
  let run_guard = match session.run_lock.try_lock() {
    Ok(guard) => guard,
    Err(_) => {
      session.running.store(false, Ordering::Release);
      tracing::warn!(session_key = %session.key, "reentry rejected by run_lock probe");
      return Err(session_busy(&session.key));
    }
  };

  // ===== 阶段 2：续跑前置校验（仅空 input 时） =====
  // 续跑要求转录非空且最后一条不是助手消息，否则 provider 将拒绝请求
  if input.is_empty() {
    let snapshot = session.snapshot();
    let continuable = match snapshot.last() {
      None => false,
      Some(Message::Assistant(_)) => false,
      Some(_) => true,
    };

    if !continuable {
      // 校验失败也走清理：复位运行状态再返回
      drop(run_guard);
      session.running.store(false, Ordering::Release);
      // 本次调用已持有运行权：换发取消根（上一次运行的 stop 可能已 abort 旧根）
      session.refresh_cancel();
      return Err(context_build_error(
        &session.key,
        "empty input requires a continuable transcript (non-empty, last not assistant)",
      ));
    }
  }

  // ===== 阶段 3：构建请求上下文 =====
  // 构建失败必须手动复位运行状态：此处在统一收尾之前，
  // `?` 直通会泄漏 running 标志导致会话永久卡死
  let mut context =
    match deps.context_provider.build(session, input.clone()).await {
      Ok(context) => context,
      Err(err) => {
        drop(run_guard);
        session.running.store(false, Ordering::Release);
        // 本次调用已持有运行权：换发取消根（上一次运行的 stop 可能已 abort 旧根）
        session.refresh_cancel();
        return Err(err);
      }
    };

  // ===== 阶段 4：发射起始事件并进入主循环 =====

  // 压缩在 build 阶段（早于此处）发生。摘要成功发射
  // `Compaction { failed: false }`；失败回退发射 `{ failed: true }`
  //（失败也留痕）。冷却抑制 / 复用摘要不发射。
  if context.compaction_triggered {
    tracing::info!(
      session_key = %session.key,
      "compaction succeeded; emitting completion event"
    );
    emitter.emit(AgentEvent::Compaction { failed: false }).await;
  } else if context.compaction_failed {
    tracing::warn!(
      session_key = %session.key,
      "compaction failed; emitting failure event"
    );
    emitter.emit(AgentEvent::Compaction { failed: true }).await;
  }

  tracing::info!(session_key = %session.key, turns_limit = config.max_iterations, "agent run start");
  emitter.emit(AgentEvent::AgentStart).await;
  emitter.emit(AgentEvent::TurnStart).await;
  let mut msgs: Vec<Message> = Vec::with_capacity(input.len() + 8);
  for message in &input {
    emitter
      .emit(AgentEvent::MessageStart {
        message: message.clone(),
      })
      .await;
    emitter
      .emit(AgentEvent::MessageEnd {
        message: message.clone(),
      })
      .await;
    msgs.push(message.clone());
  }

  let outcome = run_loop(&mut context, &mut msgs, deps, config, &emitter).await;

  // ===== 阶段 5：收尾（成功与失败路径统一执行） =====
  // 已产出消息写回转录（错误路径也不丢失），再复位运行状态
  session.append_messages(&msgs);
  drop(run_guard);
  session.running.store(false, Ordering::Release);
  // 换发取消根：本回合的 stop 已永久 abort 旧根（AbortHandle 无复位
  // 语义），不复位会导致会话此后所有回合在 pre-stream 阶段被旧信号
  // 立即取消（/stop 后无法继续对话）
  session.refresh_cancel();
  session.touch();

  match outcome {
    Ok(outcome) => {
      tracing::info!(session_key = %session.key, outcome = ?outcome, "agent run finish");
      Ok(LoopReport {
        outcome,
        new_messages: msgs,
      })
    }
    Err(err) => {
      tracing::warn!(session_key = %session.key, error_code = %err.code(), "agent run failed");
      Err(err)
    }
  }
}
/// 无新 prompt 续跑（pi-rust `run_agent_loop_continue` 同构）。
///
/// 前置校验与错误语义同 [`run_agent_loop`]（等价于 `input` 为空的调用）。
pub async fn run_agent_loop_continue(
  deps: &LoopDeps,
  session: &Session,
  config: &LoopConfig,
  emitter: Arc<dyn AgentEmitter>,
) -> RiboResult<LoopReport> {
  run_agent_loop(deps, session, Vec::new(), config, emitter).await
}

/// 主循环：双层结构（外层 follow-up 续跑，内层工具调用续轮）。
///
/// 终止路径：
/// - 判定 A：`stop_reason ∈ {Error, Aborted}` → 立即收敛；
/// - 判定 B：取消信号已触发且 steering 排空 → 以 `Aborted` 收敛
///   （先排水再查取消，保证阻塞期排队消息进入转录）；
/// - `max_iterations` 超限 → `Err`（消息由入口收尾写回转录）；
/// - 内层自然停止后 follow-up 非空 → 续跑新一轮；
/// - 否则 `Completed`。
///
/// `messages` / `context` 就地追加（入口收尾统一写回会话转录）。
async fn run_loop(
  context: &mut AgentContext,
  new_messages: &mut Vec<Message>,
  deps: &LoopDeps,
  config: &LoopConfig,
  emitter: &Arc<dyn AgentEmitter>,
) -> RiboResult<LoopOutcome> {
  // ===== 初始化：首次进入前先排水一次 steering（用户可能在等待期输入） =====
  let mut pending: Vec<Message> = drain_steering(config);

  let mut has_more_tool_calls = true;
  let mut turns: usize = 0;

  // ===== 外层 follow-up 循环 =====
  // 内层条件：仍有工具调用待续（模型继续请求工具）或尚有排队注入
  'outer: loop {
    while has_more_tool_calls || !pending.is_empty() {
      // 轮计数超限：已产出消息保留在转录（由入口收尾统一写回）
      turns += 1;
      if turns > config.max_iterations {
        return Err(max_iterations(turns, config.max_iterations));
      }

      if turns > 1 {
        emitter.emit(AgentEvent::TurnStart).await;
      }

      // ===== 注入排队消息：逐条发射进入转录事件 =====
      for message in pending.drain(..) {
        emitter
          .emit(AgentEvent::MessageStart {
            message: message.clone(),
          })
          .await;
        emitter
          .emit(AgentEvent::MessageEnd {
            message: message.clone(),
          })
          .await;
        context.messages.push(message.clone());
        new_messages.push(message.clone());
      }

      let assistant =
        stream_assistant_response(context, deps, config, emitter).await;
      let assistant_message = Message::from(assistant.clone());
      context.messages.push(assistant_message.clone());
      new_messages.push(assistant_message.clone());

      // ===== 终止判定 A：错误 / 取消立即收敛（不再执行工具） =====
      match assistant.stop_reason {
        StopReason::Error => {
          emitter
            .emit(AgentEvent::TurnEnd {
              message: Box::new(assistant_message),
              tool_results: Vec::new(),
            })
            .await;
          emitter
            .emit(AgentEvent::AgentEnd {
              messages: new_messages.clone(),
            })
            .await;
          return Ok(LoopOutcome::Failed);
        }
        StopReason::Aborted => {
          emitter
            .emit(AgentEvent::TurnEnd {
              message: Box::new(assistant_message),
              tool_results: Vec::new(),
            })
            .await;
          emitter
            .emit(AgentEvent::AgentEnd {
              messages: new_messages.clone(),
            })
            .await;
          return Ok(LoopOutcome::Aborted);
        }
        _ => {}
      }

      // ===== 工具批：Length 截断原位失败，否则按执行模式分派 =====
      let calls = collect_tool_calls(&assistant);
      let truncated =
        assistant.stop_reason == StopReason::Length && !calls.is_empty();
      let batch =
        execute_tool_batch(deps, &calls, truncated, config, emitter).await;

      for result_message in &batch.messages {
        let message = Message::from(result_message.clone());
        emitter
          .emit(AgentEvent::MessageStart {
            message: message.clone(),
          })
          .await;
        emitter.emit(AgentEvent::MessageEnd { message }).await;
      }

      let result_messages: Vec<Message> =
        batch.messages.iter().cloned().map(Message::from).collect();
      context.messages.extend(result_messages.iter().cloned());
      new_messages.extend(result_messages);
      emitter
        .emit(AgentEvent::TurnEnd {
          message: Box::new(assistant_message),
          tool_results: batch.messages,
        })
        .await;

      has_more_tool_calls = !calls.is_empty() && !batch.terminated;
      if batch.terminated {
        pending.clear();
      }

      // ===== 终止判定 B：先排水 steering 再查取消 =====
      // 保证阻塞工具执行期间排队的消息进入本次转录，而非滞留队列；
      // 下一次流调用观察到取消信号后产出 Aborted 终端消息（pi-rust 同序）
      let steering_pending = drain_steering(config);
      if config.signal.is_cancelled() && steering_pending.is_empty() {
        emitter
          .emit(AgentEvent::AgentEnd {
            messages: new_messages.clone(),
          })
          .await;
        return Ok(LoopOutcome::Aborted);
      }
      pending = steering_pending;
    }

    // 内层自然停止：follow-up 非空则续跑新一轮，否则正常完成
    let follow_ups = drain_follow_up(config);
    if follow_ups.is_empty() {
      break 'outer;
    }

    pending = follow_ups;
    has_more_tool_calls = true;
  }

  emitter
    .emit(AgentEvent::AgentEnd {
      messages: new_messages.clone(),
    })
    .await;
  Ok(LoopOutcome::Completed)
}

/// 排空 steering 队列（锁中毒语义与会话注入接口一致）。
fn drain_steering(config: &LoopConfig) -> Vec<Message> {
  config
    .steering
    .lock()
    .expect("steering queue lock poisoned")
    .try_drain()
}

/// 排空 follow-up 队列。
fn drain_follow_up(config: &LoopConfig) -> Vec<Message> {
  config
    .follow_up
    .lock()
    .expect("follow-up queue lock poisoned")
    .try_drain()
}

/// 从助手消息按源顺序收集全部工具调用。
fn collect_tool_calls(assistant: &AssistantMessage) -> Vec<ToolCall> {
  assistant
    .content
    .iter()
    .filter_map(|content| match content {
      Content::ToolCall(call) => Some(call.clone()),
      _ => None,
    })
    .collect()
}

/// 单轮流式助手响应：组装请求上下文并折叠 `ProviderEvent` 为最终消息。
///
/// 折叠规则（利用 ra-provider 自带聚合快照，无须自行拼接增量）：
/// `Start` 发射 `MessageStart`；内容事件转发 `MessageUpdate { partial }`；
/// `Done` / `Error` 发射 `MessageEnd` 并返回最终消息；流早夭（无终结
/// 事件即 `None`）折叠为终端错误消息。
async fn stream_assistant_response(
  context: &AgentContext,
  deps: &LoopDeps,
  config: &LoopConfig,
  emitter: &Arc<dyn AgentEmitter>,
) -> AssistantMessage {
  // ===== 组装请求：转录直通 + 当前工具声明 + 子令牌取消 =====
  let tools = deps.tools.get_tool_schemas().await;
  let request = ra_provider::context::Context {
    system_prompt: context.system_prompt.clone(),
    message: context.messages.clone(),
    tools,
    tool_choice: ra_core::message::ToolChoice::Auto,
  };

  let stream_token = config.signal.child_token();
  // 降级候选窗口过滤下限 = 当前转录估算 token × 1.1
  //（余量应对估算偏低）；0 或小转录时过滤自然不生效
  let estimated_tokens =
    ra_core::message::estimate_context_tokens(&context.messages);
  let min_context_window = (estimated_tokens as f64 * 1.1) as usize;
  let options = StreamOptions {
    cancel: Some(CancelToken::from_token(stream_token)),
    min_context_window,
    ..Default::default()
  };

  // ===== 建流：失败折叠为终端消息 =====
  // 取消不是错误路径：建流被取消（CANCELLED）折叠为
  // Aborted 终端消息，循环以 Aborted 收敛；其余失败仍为 Error。
  let mut stream =
    match deps.gateway.stream(&deps.model, &request, options).await {
      Ok(stream) => stream,
      Err(err) if err.code() == ra_provider::error::CANCELLED => {
        tracing::info!(
          model = %deps.model.as_str(),
          "gateway stream cancelled before terminal event"
        );
        let terminal =
          terminal_error_message(deps, StopReason::Aborted, err.message());
        emitter
          .emit(AgentEvent::MessageEnd {
            message: Message::from(terminal.clone()),
          })
          .await;
        return terminal;
      }
      Err(err) => {
        tracing::warn!(
          model = %deps.model.as_str(),
          error_code = %err.code(),
          "gateway stream failed"
        );
        let terminal =
          terminal_error_message(deps, StopReason::Error, err.message());
        emitter
          .emit(AgentEvent::MessageEnd {
            message: Message::from(terminal.clone()),
          })
          .await;
        return terminal;
      }
    };

  // ===== 消费流：内容事件转发增量快照，终结事件返回最终消息 =====
  while let Some(event) = stream.next().await {
    match event {
      ProviderEvent::Done { message, .. } => {
        emitter
          .emit(AgentEvent::MessageEnd {
            message: Message::from(message.clone()),
          })
          .await;
        return message;
      }
      ProviderEvent::Error { message, code, .. } => {
        tracing::warn!(code = ?code, "provider stream error event");
        emitter
          .emit(AgentEvent::MessageEnd {
            message: Message::from(message.clone()),
          })
          .await;
        return message;
      }
      ProviderEvent::Start { partial } => {
        emitter
          .emit(AgentEvent::MessageStart {
            message: Message::from((*partial).clone()),
          })
          .await;
      }
      ProviderEvent::ReasoningStart { partial, .. }
      | ProviderEvent::ReasoningDelta { partial, .. }
      | ProviderEvent::ReasoningEnd { partial, .. }
      | ProviderEvent::TextStart { partial, .. }
      | ProviderEvent::TextDelta { partial, .. }
      | ProviderEvent::TextEnd { partial, .. }
      | ProviderEvent::ToolCallStart { partial, .. }
      | ProviderEvent::ToolCallDelta { partial, .. }
      | ProviderEvent::ToolCallEnd { partial, .. } => {
        emitter.emit(AgentEvent::MessageUpdate { partial }).await;
      }
    }
  }

  let terminal = terminal_error_message(
    deps,
    StopReason::Error,
    "stream ended without a terminal event",
  );
  emitter
    .emit(AgentEvent::MessageEnd {
      message: Message::from(terminal.clone()),
    })
    .await;
  terminal
}

/// 构造终端助手消息（provider 记为 "gateway"，错误文本入 `error_message`）。
///
/// `stop_reason` 由调用方决定：一般失败为 `Error`；建流被取消时为
/// `Aborted`（取消不是错误路径）。
fn terminal_error_message(
  deps: &LoopDeps,
  stop_reason: StopReason,
  reason: &str,
) -> AssistantMessage {
  AssistantMessage::terminal(
    "gateway",
    deps.model.as_str(),
    stop_reason,
    reason.to_string(),
    now_ms(),
  )
}

/// 执行一批工具调用并折叠为回灌消息。
///
/// 分派规则：`truncated` 时全部原位失败不执行；批内任一工具为
/// `Sequential` 模式则整批顺序执行，否则 `JoinSet` 并发（外层 spawn
/// 看护内层，内层 panic 被折叠为失败结果）。终结条件：任一结果为
/// `tool.cancelled` 或声明 `terminate` → 置批量终止标志，
/// 循环收敛（其余并行调用随子令牌级联中断）。
async fn execute_tool_batch(
  deps: &LoopDeps,
  calls: &[ToolCall],
  truncated: bool,
  config: &LoopConfig,
  emitter: &Arc<dyn AgentEmitter>,
) -> ExecutedToolBatch {
  // ===== 截断失败不变式：Length + 工具调用 → 原位失败，不执行 =====
  if truncated {
    tracing::warn!(count = calls.len(), "truncated tool calls fail in place");
    let messages = fail_tool_calls_from_truncation(calls, emitter).await;
    return ExecutedToolBatch {
      messages,
      terminated: false,
    };
  }
  if calls.is_empty() {
    return ExecutedToolBatch {
      messages: Vec::new(),
      terminated: false,
    };
  }

  // ===== 分派：批内任一顺序工具即整批顺序（保守并行边界） =====
  let sequential = calls.iter().any(|call| {
    deps.tools.execution_mode(&call.name) == ToolExecutionMode::Sequential
  });

  let mut finalized: Vec<FinalizedToolCall> = Vec::with_capacity(calls.len());
  if sequential {
    for (index, call) in calls.iter().enumerate() {
      emitter
        .emit(AgentEvent::ToolExecutionStart {
          tool_call_id: call.id.clone(),
          tool_name: call.name.clone(),
          args: call.arguments.clone(),
        })
        .await;
      let item = execute_one_tool_inner(
        Arc::clone(&deps.tools),
        call.clone(),
        index,
        config.signal.clone(),
      )
      .await;
      emit_tool_end(emitter, &item).await;
      finalized.push(item);
    }
  } else {
    for call in calls {
      emitter
        .emit(AgentEvent::ToolExecutionStart {
          tool_call_id: call.id.clone(),
          tool_name: call.name.clone(),
          args: call.arguments.clone(),
        })
        .await;
    }
    let mut join_set: JoinSet<FinalizedToolCall> = JoinSet::new();
    for (index, call) in calls.iter().enumerate() {
      let tools = Arc::clone(&deps.tools);
      let call = call.clone();
      let signal = config.signal.clone();
      let tool_call_id = call.id.clone();
      let tool_name = call.name.clone();
      join_set.spawn(async move {
        let inner =
          tokio::spawn(execute_one_tool_inner(tools, call, index, signal));
        let result = match inner.await {
          Ok(result) => result.result,
          Err(join_error) => {
            // 内层 panic：折叠为失败结果，id 由看护层保持
            tracing::warn!(tool = %tool_name, "tool task panicked");
            Err(tool_exec_error(format!("tool task panicked: {join_error}")))
          }
        };
        FinalizedToolCall {
          index,
          tool_call_id,
          tool_name,
          result,
        }
      });
    }
    while let Some(joined) = join_set.join_next().await {
      // 看护层保证不 panic；防御性忽略 Err（运行时关闭等极端场景，
      // 缺失结果由下一轮 LLM 以 not-found 类错误自我修正）
      if let Ok(item) = joined {
        emit_tool_end(emitter, &item).await;
        finalized.push(item);
      }
    }
  }
  // ===== 记账与折叠：按源顺序生成回灌消息，提取终结标志 =====
  finalized.sort_by_key(|item| item.index);
  let mut terminated = false;
  let mut messages = Vec::with_capacity(finalized.len());
  for item in finalized {
    match &item.result {
      // 取消折叠：tool.cancelled → is_error: true 回灌 + 批量终止
      //（其余并行调用随子令牌级联中断）
      Err(err) if err.code() == ra_tools::error::CANCELLED => {
        terminated = true;
      }
      // 熔断折叠：tool.rate_limited =
      // 工具面已被 ToolManager 熔断拒绝（连续失败达阈值）——原始错误
      // 反复回灌只会诱导 LLM 无限重试。改为：回灌「工具不可用」语义
      // 的友好提示（不含原始错误栈，见 fold_tool_error），并终结整个
      // 循环——重试次数上界 = 熔断打开的那一轮。
      Err(err) if err.code() == ra_tools::error::RATE_LIMITED => {
        terminated = true;
      }
      // terminate 消费：任一结果声明终局 →
      // 终结整个循环（"exit 工具"语义）
      Ok(result) if result.terminate => {
        terminated = true;
      }
      _ => {}
    }
    messages.push(fold_tool_result(&item));
  }
  ExecutedToolBatch {
    messages,
    terminated,
  }
}

/// 发射 `ToolExecutionEnd`：错误折叠为含错误文本的结果载荷。
///
/// 熔断拒绝（`tool.rate_limited`）的载荷为「工具不可用」友好提示——
/// 事件是 UI 渲染给最终用户的主要来源，提示文案不得携带原始错误栈与内部细节。
async fn emit_tool_end(
  emitter: &Arc<dyn AgentEmitter>,
  item: &FinalizedToolCall,
) {
  let (result, is_error) = match &item.result {
    Ok(result) => (Arc::new(result.clone()), false),
    Err(err) => (
      Arc::new(ToolExecutionResult {
        content: vec![ToolOutput::Text(TextContent {
          content: user_facing_tool_error_text(err),
          signature: None,
        })],
        ..ToolExecutionResult::default()
      }),
      true,
    ),
  };
  emitter
    .emit(AgentEvent::ToolExecutionEnd {
      tool_call_id: item.tool_call_id.clone(),
      tool_name: item.tool_name.clone(),
      result,
      is_error,
    })
    .await;
}

/// 执行单条工具调用：派生子取消令牌构造执行上下文。
async fn execute_one_tool_inner(
  tools: Arc<dyn ToolProvider>,
  call: ToolCall,
  index: usize,
  signal: CancellationToken,
) -> FinalizedToolCall {
  let child = signal.child_token();
  let ctx = ToolContext::new(child, Arc::new(|_| {}));
  let result = tools.execute(call.clone(), ctx).await;
  FinalizedToolCall {
    index,
    tool_call_id: call.id.clone(),
    tool_name: call.name.clone(),
    result,
  }
}

/// 截断失败：逐条生成 `is_error` 回灌消息并发射结束事件
/// （不执行任何工具，不变式）。
async fn fail_tool_calls_from_truncation(
  calls: &[ToolCall],
  emitter: &Arc<dyn AgentEmitter>,
) -> Vec<ToolResultMessage> {
  const REASON: &str =
    "assistant response truncated before tool arguments completed";
  let mut messages = Vec::with_capacity(calls.len());
  for call in calls {
    emitter
      .emit(AgentEvent::ToolExecutionEnd {
        tool_call_id: call.id.clone(),
        tool_name: call.name.clone(),
        result: Arc::new(ToolExecutionResult {
          content: vec![ToolOutput::Text(TextContent {
            content: REASON.to_string(),
            signature: None,
          })],
          ..ToolExecutionResult::default()
        }),
        is_error: true,
      })
      .await;
    messages.push(ToolResultMessage {
      tool_call_id: call.id.clone(),
      tool_name: call.name.clone(),
      is_error: true,
      timestamp: now_ms(),
      content: vec![Content::Text(TextContent {
        content: REASON.to_string(),
        signature: None,
      })],
      details: None,
      usage: None,
      added_tool_names: Vec::new(),
    });
  }
  messages
}

/// 把终结产物折叠为回灌消息（成功转内容块，错误转错误文本）。
fn fold_tool_result(item: &FinalizedToolCall) -> ToolResultMessage {
  match &item.result {
    Ok(result) => ToolResultMessage {
      tool_call_id: item.tool_call_id.clone(),
      tool_name: item.tool_name.clone(),
      is_error: false,
      timestamp: now_ms(),
      content: result
        .content
        .iter()
        .map(|o| Content::from(o.clone()))
        .collect(),
      details: result.details.clone(),
      usage: result.usage.clone(),
      added_tool_names: Vec::new(),
    },
    Err(err) => fold_tool_error(&item.tool_call_id, &item.tool_name, err),
  }
}

/// 错误折叠：错误文本进 `content`，`is_error` 置位。
///
/// 熔断拒绝（`tool.rate_limited`）替换为「工具不可用」友好提示：
/// 原始错误（"circuit open..."）反复回灌会诱导 LLM 无限重试，
/// 因此提示文案不携带原始错误栈与内部细节。
fn fold_tool_error(
  tool_call_id: &str,
  tool_name: &str,
  err: &RiboError,
) -> ToolResultMessage {
  ToolResultMessage {
    tool_call_id: tool_call_id.to_string(),
    tool_name: tool_name.to_string(),
    is_error: true,
    timestamp: now_ms(),
    content: vec![Content::Text(TextContent {
      content: user_facing_tool_error_text(err),
      signature: None,
    })],
    details: None,
    usage: None,
    added_tool_names: Vec::new(),
  }
}

/// 用户侧可见的工具错误文案（事件与回灌消息共用）。
///
/// `tool.rate_limited`（熔断拒绝）映射为「工具不可用」语义提示并
/// 终结循环（由 `execute_tool_batch` 的折叠分支完成终结）；其余错误
/// 透传原始 message（模型需要错误细节来自我修正）。原始错误全文
/// 仍经 `tracing` 记录，不丢失可观测性。
fn user_facing_tool_error_text(err: &RiboError) -> String {
  if err.code() == ra_tools::error::RATE_LIMITED {
    "该工具当前不可用：连续失败已触发保护熔断，请稍后再试，或改用其它方式完成请求。"
      .to_string()
  } else {
    err.message().to_string()
  }
}

#[cfg(test)]
mod tests {
  use std::sync::{Arc, Mutex};

  use ra_core::message::{StopReason, UserContent};
  use ra_provider::{
    adapters::mock::{MockProvider, MockStep},
    context::Context as ProviderContext,
    ids::ModelId,
  };
  use ra_tools::types::ToolExecutionMode;

  use super::*;
  use crate::{
    adapter::ToolManagerAdapter,
    emit::CollectorEmitter,
    error::{CONTEXT_BUILD_ERROR, MAX_ITERATIONS, SESSION_BUSY},
    test_support::*,
    traits::{ContextProvider, PassthroughContextProvider, user_text_message},
  };
  use ra_tools::manager::ToolManager;

  /// 一次循环测试的运行环境。
  struct TestRun {
    /// 会话（工具桩持有同一条 Arc 以支持 Steer / CancelRoot 行为）。
    session: Arc<Session>,
    /// 循环配置。
    config: LoopConfig,
    /// 事件收集器。
    collector: Arc<CollectorEmitter>,
    /// 循环依赖。
    deps: LoopDeps,
    /// 工具桩句柄（调用记录断言用）。
    tools: Arc<MockTools>,
    /// mock 提供方句柄（LLM 调用次数断言用）。
    provider: Arc<MockProvider>,
  }

  /// 建立运行环境；`tools_builder` 可定制工具桩，`tune` 可调整循环配置。
  ///
  /// 工具桩构造时统一拿到会话句柄（Steer / CancelRoot 行为的注入点）。
  fn test_run_tuned(
    steps: Vec<MockStep>,
    tools_builder: impl FnOnce(Arc<Session>) -> MockTools,
    tune: impl FnOnce(LoopConfig) -> LoopConfig,
  ) -> TestRun {
    let session = Arc::new(Session::new("s1"));
    let config = tune(LoopConfig::for_session(&session));
    let collector = Arc::new(CollectorEmitter::new());
    let (gateway, provider) = mock_gateway_with_recorder(steps);
    let tools = Arc::new(tools_builder(session.clone()));
    let deps = make_deps(gateway, tools.clone());
    TestRun {
      session,
      config,
      collector,
      deps,
      tools,
      provider,
    }
  }

  /// 建立默认配置的运行环境（全部调用走指定默认行为）。
  fn test_run(steps: Vec<MockStep>, default_behavior: ToolBehavior) -> TestRun {
    test_run_tuned(
      steps,
      |session| MockTools::new(default_behavior).with_session(session),
      |config| config,
    )
  }

  /// 驱动一次循环（收集器已在环境中挂载）。
  async fn drive(run: &TestRun, input: Vec<Message>) -> RiboResult<LoopReport> {
    let emitter: Arc<dyn AgentEmitter> = run.collector.clone();
    run_agent_loop(&run.deps, &run.session, input, &run.config, emitter).await
  }

  /// 单条用户文本输入。
  fn input() -> Vec<Message> {
    vec![user_text_message("hi")]
  }

  /// 构造单工具调用的助手消息步骤（echo 工具）。
  fn tool_use_step(call_id: &str) -> MockStep {
    complete_step(
      StopReason::ToolUse,
      assistant_with_tool_calls(vec![tool_call(
        call_id,
        "echo",
        serde_json::json!({ "text": "hi" }),
      )]),
    )
  }

  /// 取 User 消息的文本载荷（断言辅助）。
  fn user_text(message: &Message) -> String {
    match message {
      Message::User(user) => match &user.content {
        UserContent::Text(text) => text.clone(),
        other => panic!("应为纯文本内容，实际 {other:?}"),
      },
      other => panic!("应为 User 消息，实际 {other:?}"),
    }
  }

  /// 取新增消息中下标为 index 的工具结果。
  fn tool_result_at(messages: &[Message], index: usize) -> &ToolResultMessage {
    let Message::ToolResult(result) = &messages[index] else {
      panic!("第 {index} 条应为工具结果消息");
    };
    result
  }

  // 场景：重入拒绝——running CAS 快速路径。
  // 预期：running 已置位时返回 agent.session_busy，不进入循环（无事件发射）。
  #[tokio::test]
  async fn reentry_rejected_by_cas_gate() {
    let session = Arc::new(Session::new("s1"));
    let collector = Arc::new(CollectorEmitter::new());
    let (gateway, provider) = mock_gateway_with_recorder(vec![complete_step(
      StopReason::Stop,
      text_assistant("x"),
    )]);
    let deps = make_deps(gateway, Arc::new(MockTools::new(ToolBehavior::Ok)));
    let config = LoopConfig::for_session(&session);
    session
      .running
      .store(true, std::sync::atomic::Ordering::Release);

    let emitter: Arc<dyn AgentEmitter> = collector.clone();
    let err = run_agent_loop(&deps, &session, input(), &config, emitter)
      .await
      .expect_err("重入应被拒绝");

    assert_eq!(err.code(), SESSION_BUSY);
    assert!(collector.snapshot().is_empty(), "拒绝路径不应发射任何事件");
    assert_eq!(provider.record().len(), 0, "拒绝路径不应发起 LLM 调用");
  }

  // 场景：重入拒绝——run_lock 探测兜底分支（CAS 与锁之间的竞态窗口）。
  // 预期：CAS 通过但锁被占用时返回 session_busy，且 running 复位为 false。
  #[tokio::test]
  async fn reentry_rejected_by_run_lock_probe() {
    let session = Arc::new(Session::new("s1"));
    let collector = Arc::new(CollectorEmitter::new());
    let (gateway, _provider) = mock_gateway_with_recorder(vec![complete_step(
      StopReason::Stop,
      text_assistant("x"),
    )]);
    let deps = make_deps(gateway, Arc::new(MockTools::new(ToolBehavior::Ok)));
    let config = LoopConfig::for_session(&session);

    // 强制构造「CAS 可通过但锁被占用」的窗口：持有锁并复位运行标志
    let guard = session.run_lock.try_lock().expect("初始应可锁定");
    session
      .running
      .store(false, std::sync::atomic::Ordering::Release);

    let emitter: Arc<dyn AgentEmitter> = collector.clone();
    let err = run_agent_loop(&deps, &session, input(), &config, emitter)
      .await
      .expect_err("锁被占用应拒绝重入");

    assert_eq!(err.code(), SESSION_BUSY);
    assert!(!session.is_running(), "探测失败必须复位运行标志");
    assert!(collector.snapshot().is_empty());
    drop(guard);
  }

  // 场景：上下文构建失败的复位不变式。
  // 预期：build 返回 Err 时错误透传，running 复位为 false（不会话卡死）。
  #[tokio::test]
  async fn context_build_failure_resets_running() {
    struct FailingContextProvider;

    #[async_trait::async_trait]
    impl ContextProvider for FailingContextProvider {
      async fn build(
        &self,
        _session: &Session,
        _input: Vec<Message>,
      ) -> RiboResult<AgentContext> {
        Err(context_build_error("s1", "injected failure"))
      }
    }

    let session = Arc::new(Session::new("s1"));
    let collector = Arc::new(CollectorEmitter::new());
    let deps = LoopDeps {
      gateway: mock_gateway(Vec::new()),
      model: ModelId::parse("mock-model").expect("mock 模型 ID 合法"),
      tools: Arc::new(MockTools::new(ToolBehavior::Ok)),
      context_provider: Arc::new(FailingContextProvider),
    };
    let config = LoopConfig::for_session(&session);

    let emitter: Arc<dyn AgentEmitter> = collector.clone();
    let err = run_agent_loop(&deps, &session, input(), &config, emitter)
      .await
      .expect_err("构建失败应上抛");

    assert_eq!(err.code(), CONTEXT_BUILD_ERROR);
    assert!(!session.is_running(), "失败路径必须复位运行标志");
    assert!(
      collector.snapshot().is_empty(),
      "构建失败发生在起始事件之前"
    );
  }

  // 场景：空 input 且转录为空（续跑前置不满足）。
  // 预期：返回 agent.context_build_error，运行标志复位，无事件发射。
  #[tokio::test]
  async fn empty_input_with_empty_transcript_is_rejected() {
    let run = test_run(
      vec![complete_step(StopReason::Stop, text_assistant("x"))],
      ToolBehavior::Ok,
    );

    let err = drive(&run, Vec::new()).await.expect_err("空转录不可续跑");

    assert_eq!(err.code(), CONTEXT_BUILD_ERROR);
    assert!(!run.session.is_running());
    assert!(run.collector.snapshot().is_empty());
    assert_eq!(run.provider.record().len(), 0);
  }

  // 场景：空 input 且转录尾条为助手消息（provider 将拒绝续跑请求）。
  // 预期：同样返回 agent.context_build_error。
  #[tokio::test]
  async fn empty_input_ending_with_assistant_is_rejected() {
    let run = test_run(
      vec![complete_step(StopReason::Stop, text_assistant("x"))],
      ToolBehavior::Ok,
    );
    run
      .session
      .append_messages(&[Message::from(text_assistant("done"))]);

    let err = drive(&run, Vec::new())
      .await
      .expect_err("尾条为助手不可续跑");

    assert_eq!(err.code(), CONTEXT_BUILD_ERROR);
    assert!(!run.session.is_running());
    assert_eq!(run.provider.record().len(), 0);
  }

  // 场景：单轮正常完成的事件序与转录写回。
  // 预期：事件为 agent_start → turn_start → 消息序 → turn_end → agent_end；
  // 转录与 new_messages 完全一致；恰好一个终态事件；运行标志复位。
  #[tokio::test]
  async fn single_turn_completes_with_event_sequence() {
    let run = test_run(
      vec![complete_step(StopReason::Stop, text_assistant("reply"))],
      ToolBehavior::Ok,
    );

    let report = drive(&run, input()).await.expect("正常完成");

    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_eq!(report.new_messages.len(), 2);
    assert_eq!(run.session.snapshot(), report.new_messages);
    assert!(!run.session.is_running());

    // 事件序：Complete 步骤只有 Start+Done，无 MessageUpdate
    assert_eq!(
      run.collector.tags(),
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
    let events = run.collector.snapshot();
    assert!(events.last().expect("至少一个事件").is_terminal());
    assert_eq!(
      events.iter().filter(|event| event.is_terminal()).count(),
      1,
      "恰好一个终态事件"
    );
  }

  // 场景：max_iterations 超限。
  // 预期：第二轮前返回 agent.max_iterations；已产出消息（助手 + 工具结果）
  // 写回转录；运行标志复位；只发生一次 LLM 调用。
  #[tokio::test]
  async fn max_iterations_returns_error_and_writes_back() {
    let run = test_run_tuned(
      vec![tool_use_step("c1")],
      |session| MockTools::new(ToolBehavior::Ok).with_session(session),
      |config| config.with_max_iterations(1),
    );

    let err = drive(&run, input()).await.expect_err("应超限");

    assert_eq!(err.code(), MAX_ITERATIONS);
    assert_eq!(
      run.provider.record().len(),
      1,
      "超限发生在第二次 LLM 调用之前"
    );
    assert!(!run.session.is_running());

    // 错误路径也写回转录：输入 + 助手 + 工具结果
    let snapshot = run.session.snapshot();
    assert_eq!(snapshot.len(), 3);
    assert!(matches!(&snapshot[1], Message::Assistant(_)));
    let result = tool_result_at(&snapshot, 2);
    assert_eq!(result.tool_call_id, "c1");
    assert!(!result.is_error);
  }

  // 场景：截断失败不变式——Length 且带工具调用时全部原位失败。
  // 预期：工具不执行（calls 为空）；回灌 is_error=true；循环续跑至 Completed。
  #[tokio::test]
  async fn truncated_tool_calls_fail_in_place_without_execution() {
    let run = test_run(
      vec![
        complete_step(
          StopReason::Length,
          truncated_assistant_with_tool_calls(vec![tool_call(
            "c1",
            "echo",
            serde_json::json!({ "text": "hi" }),
          )]),
        ),
        complete_step(StopReason::Stop, text_assistant("recovered")),
      ],
      ToolBehavior::Ok,
    );

    let report = drive(&run, input()).await.expect("应正常收敛");

    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_eq!(run.provider.record().len(), 2);
    assert!(run.tools.calls().is_empty(), "截断路径不得执行任何工具");

    let result = tool_result_at(&report.new_messages, 2);
    assert!(result.is_error);
    assert!(
      matches!(&result.content[0], Content::Text(t) if t.content.contains("truncated")),
      "截断原因应进入回灌文本"
    );
  }

  // 场景：工具轮完整事件序与转录推进。
  // 预期：工具 start/end 紧随助手消息，回灌消息事件在 turn_end 之前，
  // 第二轮 turn_start 后收敛；转录与 new_messages 一致。
  #[tokio::test]
  async fn tool_round_event_sequence_and_transcript() {
    let run = test_run(
      vec![
        tool_use_step("c1"),
        complete_step(StopReason::Stop, text_assistant("done")),
      ],
      ToolBehavior::Ok,
    );

    let report = drive(&run, input()).await.expect("正常收敛");

    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_eq!(run.provider.record().len(), 2);
    assert_eq!(report.new_messages.len(), 4);
    assert_eq!(run.session.snapshot(), report.new_messages);

    let result = tool_result_at(&report.new_messages, 2);
    assert_eq!(result.tool_call_id, "c1");
    assert!(!result.is_error);

    assert_eq!(
      run.collector.tags(),
      vec![
        "agent_start",
        "turn_start",
        "message_start",
        "message_end",
        "message_start",
        "message_end",
        "tool_execution_start",
        "tool_execution_end",
        "message_start",
        "message_end",
        "turn_end",
        "turn_start",
        "message_start",
        "message_end",
        "turn_end",
        "agent_end",
      ]
    );
  }

  // 场景：工具业务失败回灌（错误回灌路径）。
  // 预期：is_error=true 回灌、循环继续（第二次 LLM 调用）、最终 Completed。
  #[tokio::test]
  async fn tool_error_backfills_and_continues() {
    let run = test_run(
      vec![
        tool_use_step("c1"),
        complete_step(StopReason::Stop, text_assistant("self-corrected")),
      ],
      ToolBehavior::Err("injected failure"),
    );

    let report = drive(&run, input()).await.expect("错误应回灌而非上抛");

    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_eq!(run.provider.record().len(), 2, "失败后循环应继续");

    let result = tool_result_at(&report.new_messages, 2);
    assert!(result.is_error);
    assert!(
      matches!(&result.content[0], Content::Text(t) if t.content.contains("injected failure")),
      "错误文本应进入回灌内容"
    );
  }

  // 场景：tool.cancelled 批量终止。
  // 预期：terminated 置位 → 内层退出 → follow-up 为空 → outcome 为
  // Completed（取消标志不再被检查）；只调用一次 LLM；回灌 is_error=true。
  #[tokio::test]
  async fn tool_cancelled_terminates_batch_and_completes() {
    let run = test_run(vec![tool_use_step("c1")], ToolBehavior::Cancel);

    let report = drive(&run, input()).await.expect("取消走回灌路径");

    assert_eq!(
      report.outcome,
      LoopOutcome::Completed,
      "取消标志在批量终止后不再被检查"
    );
    assert_eq!(run.provider.record().len(), 1);

    let result = tool_result_at(&report.new_messages, 2);
    assert!(result.is_error);
  }

  // 场景：terminate 结果终结循环（"exit 工具"语义）。
  // 预期：同批量终止路径——Completed、单次 LLM 调用、回灌非错误。
  #[tokio::test]
  async fn terminate_result_ends_loop_as_completed() {
    let run = test_run(vec![tool_use_step("c1")], ToolBehavior::Terminate);

    let report = drive(&run, input()).await.expect("terminate 应正常收敛");

    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_eq!(run.provider.record().len(), 1);

    let result = tool_result_at(&report.new_messages, 2);
    assert!(!result.is_error);
  }

  // 场景：并行批内工具 panic 被看护层折叠。
  // 预期：折叠为 is_error=true 回灌，循环继续至 Completed（不 panic 整个循环）。
  #[tokio::test]
  async fn parallel_tool_panic_folds_into_error_backfill() {
    let run = test_run(
      vec![
        tool_use_step("c1"),
        complete_step(StopReason::Stop, text_assistant("recovered")),
      ],
      ToolBehavior::Panic,
    );

    let report = drive(&run, input()).await.expect("panic 应被折叠");

    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_eq!(run.provider.record().len(), 2);

    let result = tool_result_at(&report.new_messages, 2);
    assert!(result.is_error);
    assert!(
      matches!(&result.content[0], Content::Text(t) if t.content.contains("panicked")),
      "panic 折叠文本应进入回灌内容"
    );
  }

  // 场景：全并行批的事件完成序与回灌源序解耦。
  // 预期：End 事件按完成序（5ms 的 c2 先于 50ms 的 c1），
  // 回灌消息仍按源顺序（c1 在前），随后一轮收敛 Completed。
  #[tokio::test]
  async fn parallel_batch_decouples_completion_and_source_order() {
    let run = test_run_tuned(
      vec![
        complete_step(
          StopReason::ToolUse,
          assistant_with_tool_calls(vec![
            tool_call("c1", "echo", serde_json::json!({})),
            tool_call("c2", "echo", serde_json::json!({})),
          ]),
        ),
        complete_step(StopReason::Stop, text_assistant("done")),
      ],
      // 按调用 ID 指定差异化睡眠：c1 慢、c2 快
      |session| {
        MockTools::new(ToolBehavior::Ok)
          .with_session(session)
          .with_call_behavior("c1", ToolBehavior::Sleep(50))
          .with_call_behavior("c2", ToolBehavior::Sleep(5))
      },
      |config| config,
    );

    let report = drive(&run, input()).await.expect("正常收敛");

    assert_eq!(report.outcome, LoopOutcome::Completed);

    // 完成序：c2（5ms）先于 c1（50ms）结束
    let ends: Vec<String> = run
      .collector
      .snapshot()
      .iter()
      .filter_map(|event| match event {
        AgentEvent::ToolExecutionEnd { tool_call_id, .. } => {
          Some(tool_call_id.clone())
        }
        _ => None,
      })
      .collect();
    assert_eq!(ends, vec!["c2", "c1"], "End 事件按完成序发射");

    // 回灌按源顺序
    let first = tool_result_at(&report.new_messages, 2);
    let second = tool_result_at(&report.new_messages, 3);
    assert_eq!(first.tool_call_id, "c1");
    assert_eq!(second.tool_call_id, "c2");
  }

  // 场景：批内含 Sequential 工具时整批顺序执行。
  // 预期：事件严格按 start→end→start→end 交替（慢工具执行期间不启动后续）。
  #[tokio::test]
  async fn sequential_mode_runs_batch_in_order() {
    let run = test_run_tuned(
      vec![
        complete_step(
          StopReason::ToolUse,
          assistant_with_tool_calls(vec![
            tool_call("c1", "echo", serde_json::json!({})),
            tool_call("c2", "ordered", serde_json::json!({})),
          ]),
        ),
        complete_step(StopReason::Stop, text_assistant("done")),
      ],
      // "ordered" 声明为 Sequential → 整批降级为顺序执行
      |session| {
        MockTools::new(ToolBehavior::Ok)
          .with_session(session)
          .with_mode("ordered", ToolExecutionMode::Sequential)
          .with_call_behavior("c1", ToolBehavior::Sleep(50))
          .with_call_behavior("c2", ToolBehavior::Sleep(5))
      },
      |config| config,
    );

    let report = drive(&run, input()).await.expect("正常收敛");

    assert_eq!(report.outcome, LoopOutcome::Completed);

    let tool_events: Vec<(&str, String)> = run
      .collector
      .snapshot()
      .iter()
      .filter_map(|event| match event {
        AgentEvent::ToolExecutionStart { tool_call_id, .. } => {
          Some(("start", tool_call_id.clone()))
        }
        AgentEvent::ToolExecutionEnd { tool_call_id, .. } => {
          Some(("end", tool_call_id.clone()))
        }
        _ => None,
      })
      .collect();
    assert_eq!(
      tool_events,
      vec![
        ("start", "c1".to_string()),
        ("end", "c1".to_string()),
        ("start", "c2".to_string()),
        ("end", "c2".to_string()),
      ],
      "顺序批内 start/end 严格交替"
    );
  }

  // 场景：工具执行期间注入 steering（轮间排水语义）。
  // 预期：下一轮 LLM 前注入——第二次请求上下文包含注入消息；注入消息的
  // 进入转录事件位于第一轮 turn_end 之后；最终 Completed 且转录含注入。
  #[tokio::test]
  async fn steering_queued_during_tools_is_injected_next_turn() {
    // 捕获第二次 LLM 请求的完整消息列表（首个 Complete 步骤不含捕获闭包）
    let captured: Arc<Mutex<Vec<Vec<Message>>>> =
      Arc::new(Mutex::new(Vec::new()));
    let recorder = captured.clone();
    let run = test_run(
      vec![
        tool_use_step("c1"),
        complete_with(move |ctx: &ProviderContext| {
          recorder
            .lock()
            .expect("捕获锁有效")
            .push(ctx.message.clone());
          (StopReason::Stop, text_assistant("after-inject"))
        }),
      ],
      ToolBehavior::Steer,
    );

    let report = drive(&run, input()).await.expect("正常收敛");

    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_eq!(run.provider.record().len(), 2);

    // 第二次请求上下文：输入 + 助手1 + 工具结果 + 注入 = 4 条，末条为注入
    let snapshots = captured.lock().expect("捕获锁有效").clone();
    assert_eq!(snapshots.len(), 1, "捕获闭包仅挂在第二次调用上");
    assert_eq!(snapshots[0].len(), 4);
    assert_eq!(user_text(&snapshots[0][3]), "inject");

    // 注入消息进入新增转录
    assert!(
      report
        .new_messages
        .iter()
        .any(|m| matches!(m, Message::User(u) if matches!(&u.content, UserContent::Text(t) if t == "inject"))),
      "注入消息应进入新增消息"
    );

    // 注入的 message_start 位于第一轮 turn_end 之后
    let events = run.collector.snapshot();
    let inject_index = events
      .iter()
      .position(|event| {
        matches!(
          event,
          AgentEvent::MessageStart { message: Message::User(u) }
            if matches!(&u.content, UserContent::Text(t) if t == "inject")
        )
      })
      .expect("注入消息应有进入转录事件");
    let turn_end_index = events
      .iter()
      .position(|event| matches!(event, AgentEvent::TurnEnd { .. }))
      .expect("应有 turn_end");
    assert!(inject_index > turn_end_index, "注入事件应在 turn_end 之后");
  }

  // 场景：工具执行期间触发会话取消根（无排队 steering）。
  // 预期：本轮结束后「取消且无待注入」→ Aborted；仅一次 LLM 调用。
  #[tokio::test]
  async fn cancel_root_during_tools_aborts_run() {
    let run = test_run(vec![tool_use_step("c1")], ToolBehavior::CancelRoot);

    let report = drive(&run, input()).await.expect("取消路径正常返回");

    assert_eq!(report.outcome, LoopOutcome::Aborted);
    assert_eq!(run.provider.record().len(), 1);
    assert!(run.config.signal.is_cancelled());
  }

  // 场景：先排水再取消——第一轮工具排队 steering，第二轮工具触发会话取消根。
  // 预期：排队的 steering 在取消检查前排水注入（到达第二次 LLM 请求、
  // 进入转录）；本轮批结束后「取消且无待注入」→ Aborted。
  #[tokio::test]
  async fn cancel_root_with_queued_steering_drains_before_abort() {
    let captured: Arc<Mutex<Vec<Vec<Message>>>> =
      Arc::new(Mutex::new(Vec::new()));
    let recorder = captured.clone();
    let run = test_run_tuned(
      vec![
        tool_use_step("c1"),
        complete_with(move |ctx: &ProviderContext| {
          recorder
            .lock()
            .expect("捕获锁有效")
            .push(ctx.message.clone());
          (
            StopReason::ToolUse,
            assistant_with_tool_calls(vec![tool_call(
              "c2",
              "echo",
              serde_json::json!({}),
            )]),
          )
        }),
      ],
      // c1 注入 steering（默认行为），c2 触发取消根
      |session| {
        MockTools::new(ToolBehavior::Steer)
          .with_session(session)
          .with_call_behavior("c2", ToolBehavior::CancelRoot)
      },
      |config| config,
    );

    let report = drive(&run, input()).await.expect("取消路径正常返回");

    assert_eq!(report.outcome, LoopOutcome::Aborted);
    assert_eq!(run.provider.record().len(), 2);
    assert!(run.config.signal.is_cancelled());

    // 第二次请求上下文含注入消息（输入 + 助手1 + 工具结果 + 注入）
    let snapshots = captured.lock().expect("捕获锁有效").clone();
    assert_eq!(snapshots.len(), 1, "捕获闭包仅挂在第二次调用上");
    assert_eq!(snapshots[0].len(), 4);
    assert_eq!(user_text(&snapshots[0][3]), "inject");

    // 注入消息进入新增消息（不滞留队列、不丢失）
    assert!(
      report
        .new_messages
        .iter()
        .any(|m| matches!(m, Message::User(u) if matches!(&u.content, UserContent::Text(t) if t == "inject"))),
      "注入消息应进入新增消息"
    );
  }

  // 场景：取消时仍有排队 steering——下一轮流建流被网关取消检查拒绝。
  // 预期：CANCELLED 建流失败折叠为 Aborted 终端消息（取消不是错误路径，
  // 循环以 Aborted 收敛；steering 在建流前已排水注入转录；
  // 网关层拒绝不触达提供方（record 仅 1 次）。
  #[tokio::test]
  async fn cancel_with_pending_steering_converges_aborted_not_failed() {
    let run = test_run_tuned(
      vec![complete_step(
        StopReason::ToolUse,
        assistant_with_tool_calls(vec![
          tool_call("c1", "echo", serde_json::json!({ "text": "hi" })),
          tool_call("c2", "echo", serde_json::json!({ "text": "hi" })),
        ]),
      )],
      |session| {
        MockTools::new(ToolBehavior::Ok)
          .with_session(session)
          .with_call_behavior("c1", ToolBehavior::Steer)
          .with_call_behavior("c2", ToolBehavior::CancelRoot)
      },
      |config| config,
    );

    let report = drive(&run, input()).await.expect("取消路径正常返回");

    // 批内 c1 注入 steering、c2 触发取消根 → 判定 B 因 steering 非空放行
    // → 下一轮流建流被网关取消检查拒绝 → 折叠为 Aborted（而非 Failed）
    assert_eq!(report.outcome, LoopOutcome::Aborted);
    assert_eq!(
      run.provider.record().len(),
      1,
      "第二次建流应在网关层被拒绝，不触达提供方"
    );
    assert!(run.config.signal.is_cancelled());

    // 终端消息为 Aborted 终止原因（而非 Error）
    let last = report.new_messages.last().expect("存在终端消息");
    let Message::Assistant(assistant) = last else {
      panic!("终端消息应为助手消息，实际 {last:?}")
    };
    assert_eq!(assistant.stop_reason, StopReason::Aborted);

    // steering 注入消息进入新增消息（在建流前已排水，不滞留队列）
    assert!(
      report
        .new_messages
        .iter()
        .any(|m| matches!(m, Message::User(u) if matches!(&u.content, UserContent::Text(t) if t == "inject"))),
      "steering 注入应先于取消建流进入转录"
    );
  }

  // 场景：流式内容事件转发与流内 Error 终结（StreamFail 脚本）。
  // 预期：内容事件发射 MessageUpdate；Error 事件折叠为终端消息
  // （stop_reason=Error 且携带错误文本）→ LoopOutcome::Failed，
  // 单次 LLM 调用、不执行工具。
  #[tokio::test]
  async fn stream_content_updates_and_error_event_converge_failed() {
    let events = vec![ProviderEvent::TextStart {
      index: 0,
      partial: Arc::new(text_assistant("partial")),
    }];
    let run = test_run(
      vec![MockStep::StreamFail {
        events,
        reason: StopReason::Error,
        code: Some(ra_provider::error::SERVER_ERROR),
      }],
      ToolBehavior::Ok,
    );

    let report = drive(&run, input()).await.expect("失败路径正常返回");

    assert_eq!(report.outcome, LoopOutcome::Failed);
    assert_eq!(run.provider.record().len(), 1);
    assert!(
      run.collector.tags().contains(&"message_update"),
      "内容事件应转发 MessageUpdate"
    );

    // 终端助手消息携带错误文本
    let last = report.new_messages.last().expect("存在终端消息");
    let Message::Assistant(assistant) = last else {
      panic!("终端消息应为助手消息，实际 {last:?}")
    };
    assert_eq!(assistant.stop_reason, StopReason::Error);
    assert!(assistant.error_message.is_some());
  }

  // 场景：网关建流失败（非取消）折叠为 Error 终端消息。
  // 预期：建流 Err（SERVER_ERROR）→ Error 终端 → LoopOutcome::Failed，
  // 单次调用记录，不执行工具。
  #[tokio::test]
  async fn gateway_stream_failure_folds_to_error_terminal() {
    let run = test_run(
      vec![MockStep::Fail(ra_provider::error::pre_stream(
        ra_provider::error::SERVER_ERROR,
        "upstream down",
      ))],
      ToolBehavior::Ok,
    );

    let report = drive(&run, input()).await.expect("失败路径正常返回");

    assert_eq!(report.outcome, LoopOutcome::Failed);
    // 网关按默认策略可能重试多次；agent 层只关心失败最终被折叠
    assert!(!run.provider.record().is_empty(), "建流失败应触达提供方");

    let last = report.new_messages.last().expect("存在终端消息");
    let Message::Assistant(assistant) = last else {
      panic!("终端消息应为助手消息，实际 {last:?}")
    };
    assert_eq!(assistant.stop_reason, StopReason::Error);
    assert!(assistant.error_message.is_some(), "错误描述应进入终端消息");
  }

  // 场景：九类内容事件全部转发为 MessageUpdate（快照直用，不自行拼接）。
  // 预期：Reasoning*/Text*/ToolCall* 各事件发射 message_update；
  // 脚本自动以 Done(Stop) 收敛 → Completed（partial 不含 ToolCall 块，
  // 不触发工具批）。
  #[tokio::test]
  async fn all_content_event_variants_forward_message_update() {
    let partial = || Arc::new(text_assistant("snapshot"));
    let events = vec![
      ProviderEvent::ReasoningStart {
        index: 0,
        partial: partial(),
      },
      ProviderEvent::ReasoningDelta {
        index: 0,
        delta: "think".to_string(),
        partial: partial(),
      },
      ProviderEvent::ReasoningEnd {
        index: 0,
        content: "think".to_string(),
        partial: partial(),
      },
      ProviderEvent::TextStart {
        index: 0,
        partial: partial(),
      },
      ProviderEvent::TextDelta {
        index: 0,
        delta: "text".to_string(),
        partial: partial(),
      },
      ProviderEvent::TextEnd {
        index: 0,
        content: "text".to_string(),
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
      // ToolCallEnd 携带完整调用，但 partial 快照不含 ToolCall 内容块，
      // 最终消息 stop_reason=Stop → 不触发工具批
      ProviderEvent::ToolCallEnd {
        index: 0,
        tool_call: tool_call("c9", "echo", serde_json::json!({})),
        partial: partial(),
      },
    ];
    let run = test_run(vec![MockStep::Events(events)], ToolBehavior::Ok);

    let report = drive(&run, input()).await.expect("内容回放正常返回");

    assert_eq!(report.outcome, LoopOutcome::Completed);
    // 9 个内容事件 → 9 次 MessageUpdate
    let updates = run
      .collector
      .snapshot()
      .iter()
      .filter(|e| e.type_tag() == "message_update")
      .count();
    assert_eq!(updates, 9, "九类内容事件应各转发一次 MessageUpdate");
    assert_eq!(run.tools.calls().len(), 0, "不应触发工具执行");
  }

  // 场景：终止判定 A——Error 终端消息立即收敛。
  // 预期：LoopOutcome::Failed，单次 LLM 调用，不执行工具。
  #[tokio::test]
  async fn stop_reason_error_converges_failed() {
    let run = test_run(
      vec![complete_step(
        StopReason::Error,
        terminal_assistant(StopReason::Error, "boom"),
      )],
      ToolBehavior::Ok,
    );

    let report = drive(&run, input()).await.expect("Failed 路径正常返回");

    assert_eq!(report.outcome, LoopOutcome::Failed);
    assert_eq!(run.provider.record().len(), 1);
    assert!(run.tools.calls().is_empty());
  }

  // 场景：终止判定 A——Aborted 终端消息立即收敛。
  // 预期：LoopOutcome::Aborted，单次 LLM 调用。
  #[tokio::test]
  async fn stop_reason_aborted_converges_aborted() {
    let run = test_run(
      vec![complete_step(
        StopReason::Aborted,
        terminal_assistant(StopReason::Aborted, "cancelled"),
      )],
      ToolBehavior::Ok,
    );

    let report = drive(&run, input()).await.expect("Aborted 路径正常返回");

    assert_eq!(report.outcome, LoopOutcome::Aborted);
    assert_eq!(run.provider.record().len(), 1);
  }

  // 场景：Deferred 停止原因不触发立即收敛，也无工具可续。
  // 预期：内层自然停止后 follow-up 为空 → Completed。
  #[tokio::test]
  async fn stop_reason_deferred_completes_without_continuation() {
    let run = test_run(
      vec![complete_step(
        StopReason::Deferred,
        terminal_assistant(StopReason::Deferred, "deferred"),
      )],
      ToolBehavior::Ok,
    );

    let report = drive(&run, input()).await.expect("Deferred 应收敛");

    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_eq!(run.provider.record().len(), 1);
  }

  // 场景：Length 但无工具调用——不触发截断失败不变式。
  // 预期：按正常完成收敛（Completed），无工具执行。
  #[tokio::test]
  async fn length_without_tool_calls_completes() {
    let mut message = text_assistant("cut off");
    message.stop_reason = StopReason::Length;
    let run = test_run(
      vec![complete_step(StopReason::Length, message)],
      ToolBehavior::Ok,
    );

    let report = drive(&run, input()).await.expect("应正常收敛");

    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert!(run.tools.calls().is_empty(), "无工具调用不应执行工具");
  }

  // 场景：follow-up 排水续跑（外层循环语义）。
  // 预期：内层自然停止后 follow-up 非空 → 续跑新一轮（第二次 LLM 调用），
  // 注入消息进入转录，最终 Completed。
  #[tokio::test]
  async fn follow_up_drains_into_continuation_turn() {
    let run = test_run(
      vec![
        complete_step(StopReason::Stop, text_assistant("first")),
        complete_step(StopReason::Stop, text_assistant("second")),
      ],
      ToolBehavior::Ok,
    );
    run.session.enqueue_follow_up(user_text_message("follow"));

    let report = drive(&run, input()).await.expect("正常收敛");

    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_eq!(run.provider.record().len(), 2);
    assert_eq!(report.new_messages.len(), 4);
    assert!(
      report
        .new_messages
        .iter()
        .any(|m| matches!(m, Message::User(u) if matches!(&u.content, UserContent::Text(t) if t == "follow"))),
      "follow-up 消息应进入新增消息"
    );
    assert_eq!(run.session.snapshot(), report.new_messages);
  }

  // 场景：run_agent_loop_continue 等价空 input 续跑。
  // 预期：转录尾条为用户消息时可续跑（Completed），新增消息不含历史。
  #[tokio::test]
  async fn continue_loop_runs_from_existing_transcript() {
    let run = test_run(
      vec![complete_step(StopReason::Stop, text_assistant("resumed"))],
      ToolBehavior::Ok,
    );
    run.session.append_messages(&[user_text_message("history")]);

    let emitter: Arc<dyn AgentEmitter> = run.collector.clone();
    let report =
      run_agent_loop_continue(&run.deps, &run.session, &run.config, emitter)
        .await
        .expect("可续跑");

    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_eq!(report.new_messages.len(), 1, "新增消息不含历史");
    assert_eq!(run.session.snapshot().len(), 2);
  }

  // ===== 真实 ToolManager 校验失败的回灌链路 =====

  // 场景：mock 网关返回非法 echo 调用（缺 required "text" 且
  // additionalProperties=false），工具边界为真实 ToolManagerAdapter
  // + create_echo_tool（非 MockTools 桩）。
  // 预期：ToolManager 阶段 4 schema 校验失败 → tool.invalid_arguments →
  // fold_tool_error 折叠为 ToolResultMessage(is_error=true，错误文本在
  // content Text 块) 回灌转录；循环不被中断，消费第二轮脚本正常收敛。
  #[tokio::test]
  async fn int_val_01_schema_validation_failure_returns_tool_result_error() {
    let session = Arc::new(Session::new("s1"));
    let collector = Arc::new(CollectorEmitter::new());
    // ===== 脚本：第一轮要求 echo 非法参数 → 第二轮正常收敛 =====
    let (gateway, provider) = mock_gateway_with_recorder(vec![
      complete_step(
        StopReason::ToolUse,
        assistant_with_tool_calls(vec![tool_call(
          "c1",
          "echo",
          serde_json::json!({ "foo": 1 }),
        )]),
      ),
      complete_step(StopReason::Stop, text_assistant("recovered")),
    ]);

    // ===== 真实工具边界：ToolManager + echo 工具 =====
    let manager = ToolManager::new();
    manager
      .register(ra_tools::builtin::create_echo_tool())
      .expect("echo 工具注册成功");
    let deps = LoopDeps {
      gateway,
      model: ModelId::parse("mock-model").expect("mock 模型 ID 合法"),
      tools: Arc::new(ToolManagerAdapter::new(Arc::new(manager))),
      context_provider: Arc::new(PassthroughContextProvider::new(Vec::new())),
    };
    let config = LoopConfig::for_session(&session);
    let emitter: Arc<dyn AgentEmitter> = collector.clone();

    let report = run_agent_loop(&deps, &session, input(), &config, emitter)
      .await
      .expect("校验失败应折叠为工具结果而非中断循环");

    // ===== 断言：转录含 is_error 工具结果，错误文本在 content Text 块 =====
    let tool_result = report
      .new_messages
      .iter()
      .find_map(|m| match m {
        Message::ToolResult(r) if r.is_error => Some(r),
        _ => None,
      })
      .expect("转录应含 is_error 工具结果");
    assert_eq!(tool_result.tool_name, "echo");
    assert_eq!(tool_result.tool_call_id, "c1");
    let error_text = tool_result
      .content
      .iter()
      .find_map(|c| match c {
        Content::Text(t) => Some(t.content.clone()),
        _ => None,
      })
      .expect("错误文本应为 content 的 Text 块");
    assert!(
      error_text.contains("invalid_arguments") || error_text.contains("text"),
      "错误文本应描述参数校验失败，实际：{error_text}"
    );

    // ===== 断言：循环未中断——消费第二轮脚本正常收敛 =====
    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_eq!(provider.record().len(), 2, "应发起两次 LLM 调用");
    assert!(!session.is_running(), "收尾应复位运行标志");
  }

  /// 恒失败工具（真实 ToolManager 熔断测试用）：始终返回
  /// `tool.exec_failed`，错误文本为可识别的「原始错误栈」标记。
  struct AlwaysFailTool {
    schema: ra_core::message::ToolSchema,
  }

  impl AlwaysFailTool {
    fn new() -> Self {
      Self {
        schema: ra_core::message::ToolSchema {
          name: "boom".to_string(),
          description: "always fails".to_string(),
          input_schema: ra_core::message::Schema(serde_json::json!({
            "type": "object", "properties": {}
          })),
          constrained_sampling: None,
        },
      }
    }
  }

  #[async_trait::async_trait]
  impl ra_tools::traits::AgentTool for AlwaysFailTool {
    fn schema(&self) -> &ra_core::message::ToolSchema {
      &self.schema
    }

    fn label(&self) -> &str {
      "Always Fail"
    }

    async fn execute(
      &self,
      _tool_call_id: &str,
      _args: serde_json::Value,
      _ctx: &ToolContext,
    ) -> RiboResult<ToolExecutionResult> {
      Err(RiboError::new(
        "tool",
        ra_tools::error::EXEC_FAILED,
        "boom-original-stack",
      ))
    }
  }

  // 场景：连续失败触发工具面熔断后——
  // ① 熔断闭合：第 6 次调用被 ToolManager 拒绝（tool.rate_limited）；
  // ② 用户侧消息为「工具不可用」语义提示，不含原始错误栈
  //    （"boom-original-stack" / "circuit open"）；
  // ③ 重试次数有上界：循环在熔断轮终止，不再发起第 7 次 LLM 调用。
  #[tokio::test]
  async fn circuit_open_feeds_back_unavailable_notice_and_stops() {
    // 真实 ToolManager（默认 failure_threshold = 5）+ 恒失败工具
    let manager = ToolManager::new();
    manager
      .register(std::sync::Arc::new(AlwaysFailTool::new()))
      .expect("boom 工具注册成功");
    let tools = Arc::new(ToolManagerAdapter::new(std::sync::Arc::new(manager)));

    // 7 步 LLM 脚本：模型每一轮都请求同一个失败工具
    let step = || {
      complete_step(
        StopReason::ToolUse,
        assistant_with_tool_calls(vec![tool_call(
          "c1",
          "boom",
          serde_json::json!({}),
        )]),
      )
    };
    let session = Arc::new(Session::new("s1"));
    let collector = Arc::new(CollectorEmitter::new());
    let (gateway, provider) = mock_gateway_with_recorder(vec![
      step(),
      step(),
      step(),
      step(),
      step(),
      step(),
      step(),
    ]);
    let deps = LoopDeps {
      gateway,
      model: ModelId::parse("mock-model").expect("mock 模型 ID 合法"),
      tools,
      context_provider: Arc::new(PassthroughContextProvider::new(Vec::new())),
    };
    let config = LoopConfig::for_session(&session);
    let emitter: Arc<dyn AgentEmitter> = collector.clone();

    let report = run_agent_loop(&deps, &session, input(), &config, emitter)
      .await
      .expect("熔断终止应正常返回，而非错误");

    // ③ 重试次数有上界：恰好 6 次 LLM 调用，
    //    循环在熔断轮终结、未消费第 7 步
    assert_eq!(
      provider.record().len(),
      6,
      "熔断轮应终结循环，不再发起下一次 LLM 调用"
    );
    assert!(matches!(
      report.outcome,
      crate::types::LoopOutcome::Completed
    ));

    // ② 回灌消息：最后一条工具结果为「工具不可用」语义提示
    let last_tool_result = report
      .new_messages
      .iter()
      .rev()
      .find_map(|m| match m {
        Message::ToolResult(r) => Some(r),
        _ => None,
      })
      .expect("至少一条工具结果消息");
    let fed_back = match &last_tool_result.content[0] {
      Content::Text(t) => t.content.clone(),
      other => panic!("应为文本内容，实际 {other:?}"),
    };
    assert!(fed_back.contains("不可用"), "got: {fed_back}");
    assert!(
      !fed_back.contains("boom-original-stack"),
      "不得回灌原始错误栈: {fed_back}"
    );
    assert!(
      !fed_back.contains("circuit open"),
      "不得回灌内部熔断细节: {fed_back}"
    );

    // ② ToolExecutionEnd 事件（UI 渲染来源）同样脱敏
    let events = collector.snapshot();
    let end_texts: Vec<String> = events
      .iter()
      .filter_map(|e| match e {
        AgentEvent::ToolExecutionEnd {
          result, is_error, ..
        } if *is_error => {
          let mut text = String::new();
          for out in &result.content {
            if let ToolOutput::Text(t) = out {
              text.push_str(&t.content);
            }
          }
          Some(text)
        }
        _ => None,
      })
      .collect();
    // 仅熔断轮（最后一个错误事件）脱敏：前 5 轮真实失败按 透传
    // 原始错误（供模型自我修正），属预期行为
    let last = end_texts.last().expect("应有错误结束事件");
    assert!(last.contains("不可用"), "got: {last}");
    assert!(
      !last.contains("boom-original-stack") && !last.contains("circuit open"),
      "熔断轮事件不得携带原始错误栈: {last}"
    );
    // 前 5 轮真实失败：错误原样透传，且确实发生了 5 次
    assert_eq!(
      end_texts.len(),
      6,
      "5 次真实失败 + 1 次熔断拒绝，共 6 个错误结束事件"
    );
  }
}
