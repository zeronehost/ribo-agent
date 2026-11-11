//! 测试专用基础设施（仅 `cfg(test)` 编译，不进入生产构建）。
//!
//! 提供 mock 网关、可编程工具桩与常用消息构造器，供各单元测试模块
//! 以 `use crate::test_support::*` 复用。

#![cfg(test)]

use std::{
  collections::HashMap,
  sync::{Arc, Mutex},
  time::Duration,
};

use ra_core::{
  error::{RiboError, RiboResult},
  message::{
    AssistantMessage, Content, Schema, StopReason, TextContent, ToolCall,
    ToolSchema,
  },
};
use ra_provider::{
  adapters::mock::{MockProvider, MockStep},
  context::Context,
  gateway::{Gateway, GatewayConfig},
  ids::{ModelId, ProviderId},
  model::Model,
};
use ra_tools::{
  context::ToolContext,
  types::{ToolExecutionMode, ToolExecutionResult, ToolOutput},
};

use crate::{
  session::Session,
  traits::{PassthroughContextProvider, ToolProvider, user_text_message},
  types::LoopDeps,
};

/// 构造注册了 mock 提供方的网关（无需断言调用记录时使用）。
///
/// `steps` 为按调用顺序消费的脚本步骤；模型 ID 固定为 `mock-model`。
pub fn mock_gateway(steps: Vec<MockStep>) -> Arc<Gateway> {
  mock_gateway_with_recorder(steps).0
}

/// 构造网关并返回 mock 提供方句柄（`record()` 断言 LLM 调用次数用）。
///
/// # Panics
///
/// 提供方 ID 解析失败或网关注册被拒时 panic（测试环境不应发生）。
pub fn mock_gateway_with_recorder(
  steps: Vec<MockStep>,
) -> (Arc<Gateway>, Arc<MockProvider>) {
  let provider_id = ProviderId::parse("mock").expect("mock 提供方 ID 合法");
  let model = Model::new(
    provider_id.clone(),
    ModelId::parse("mock-model").expect("mock 模型 ID 合法"),
    "Mock Model",
    "http://localhost",
  )
  // 网关注册校验要求窗口与输出限额非零
  .with_context_window(8192)
  .with_max_tokens(1024);
  let mock = Arc::new(MockProvider::new(provider_id, vec![model], steps));
  let mut gateway = Gateway::new(GatewayConfig::default());
  gateway.register(mock.clone()).expect("mock 提供方注册成功");
  (Arc::new(gateway), mock)
}

/// 构造循环依赖集合：mock 网关 + 指定工具边界 + 直通上下文。
pub fn make_deps(
  gateway: Arc<Gateway>,
  tools: Arc<dyn ToolProvider>,
) -> LoopDeps {
  LoopDeps {
    gateway,
    model: ModelId::parse("mock-model").expect("mock 模型 ID 合法"),
    tools,
    context_provider: Arc::new(PassthroughContextProvider::new(Vec::new())),
  }
}

/// 纯文本助手消息（停止原因 `Stop`）。
pub fn text_assistant(text: &str) -> AssistantMessage {
  let mut message = AssistantMessage::empty("mock", "mock-model", 0);
  message.content = vec![Content::Text(TextContent {
    content: text.to_string(),
    signature: None,
  })];
  message.stop_reason = StopReason::Stop;
  message
}

/// 携带工具调用的助手消息（停止原因 `ToolUse`）。
pub fn assistant_with_tool_calls(calls: Vec<ToolCall>) -> AssistantMessage {
  let mut message = AssistantMessage::empty("mock", "mock-model", 0);
  message.content = calls.into_iter().map(Content::ToolCall).collect();
  message.stop_reason = StopReason::ToolUse;
  message
}

/// 截断（`Length`）且携带工具调用的助手消息——触发原位失败不变式。
pub fn truncated_assistant_with_tool_calls(
  calls: Vec<ToolCall>,
) -> AssistantMessage {
  let mut message = assistant_with_tool_calls(calls);
  message.stop_reason = StopReason::Length;
  message
}

/// 终态助手消息（`Error` / `Aborted` / `Deferred` 等收敛场景）。
pub fn terminal_assistant(
  reason: StopReason,
  error_text: &str,
) -> AssistantMessage {
  AssistantMessage::terminal("mock", "mock-model", reason, error_text, 0)
}

/// 完成步骤：固定返回指定停止原因与消息（闭包无副作用）。
pub fn complete_step(
  reason: StopReason,
  message: AssistantMessage,
) -> MockStep {
  MockStep::Complete(Arc::new(move |_ctx: &Context| {
    (reason.clone(), message.clone())
  }))
}

/// 完成步骤：闭包可观测本次请求上下文（注入断言 / 调用计数用）。
pub fn complete_with<F>(build: F) -> MockStep
where
  F: Fn(&Context) -> (StopReason, AssistantMessage) + Send + Sync + 'static,
{
  MockStep::Complete(Arc::new(build))
}

/// 构造一次工具调用。
pub fn tool_call(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
  ToolCall {
    id: id.to_string(),
    name: name.to_string(),
    arguments: args,
    signature: None,
    namespace: None,
  }
}

/// 工具桩的可编程行为（全部变体可 `Copy`，按调用 ID 查表派发）。
#[derive(Debug, Clone, Copy)]
pub enum ToolBehavior {
  /// 返回成功文本结果。
  Ok,
  /// 返回业务失败（`tool.exec_failed`，载荷为错误文本）。
  Err(&'static str),
  /// 返回取消错误（`tool.cancelled`，触发批量终止）。
  Cancel,
  /// 返回带 `terminate: true` 的成功结果（终结整个循环）。
  Terminate,
  /// 执行中 panic（仅并行批可被看护层折叠为失败结果）。
  Panic,
  /// 执行期间向会话注入一条 steering 消息（文本 `inject`）。
  Steer,
  /// 执行期间触发会话取消根（abort 树级联到循环信号）。
  CancelRoot,
  /// 挂起至取消信号触发或 60 秒超时（重入拒绝 / stop 测试用）。
  Hang,
  /// 真实睡眠指定毫秒后成功（并行完成序 / 顺序执行测试用）。
  Sleep(u64),
}

/// 可编程工具桩：替代真实 `ToolManager`，记录调用并按调用 ID 派发行为。
///
/// 未显式指定行为的调用走构造时的默认行为；未注册执行模式的工具名
/// 查表失败时保守返回 `Sequential`（与生产适配器语义一致）。
pub struct MockTools {
  /// 工具名 → 执行模式（未知名返回 Sequential）。
  modes: HashMap<String, ToolExecutionMode>,
  /// 调用 ID → 行为（优先于默认行为）。
  behaviors: Mutex<HashMap<String, ToolBehavior>>,
  /// 未指定行为时的兜底行为。
  default_behavior: ToolBehavior,
  /// 已发生的调用记录。
  calls: Mutex<Vec<ToolCall>>,
  /// 会话句柄（Steer / CancelRoot 行为注入点）。
  session: Option<Arc<Session>>,
}

impl MockTools {
  /// 构造工具桩：默认注册并行模式工具 `echo`，全部调用走默认行为。
  pub fn new(default_behavior: ToolBehavior) -> Self {
    let mut modes = HashMap::new();
    modes.insert("echo".to_string(), ToolExecutionMode::Parallel);
    Self {
      modes,
      behaviors: Mutex::new(HashMap::new()),
      default_behavior,
      calls: Mutex::new(Vec::new()),
      session: None,
    }
  }

  /// 绑定会话句柄（Steer / CancelRoot 行为需要）。
  pub fn with_session(mut self, session: Arc<Session>) -> Self {
    self.session = Some(session);
    self
  }

  /// 覆盖指定工具的执行模式（未知名会一并注册进 schema 列表）。
  pub fn with_mode(mut self, name: &str, mode: ToolExecutionMode) -> Self {
    self.modes.insert(name.to_string(), mode);
    self
  }

  /// 指定单个调用 ID 的行为（优先于默认行为；内部仅经锁写入，无需可变借用）。
  pub fn with_call_behavior(
    self,
    call_id: &str,
    behavior: ToolBehavior,
  ) -> Self {
    self
      .behaviors
      .lock()
      .expect("mock tools lock poisoned")
      .insert(call_id.to_string(), behavior);
    self
  }

  /// 取已发生的调用记录快照。
  pub fn calls(&self) -> Vec<ToolCall> {
    self.calls.lock().expect("mock tools lock poisoned").clone()
  }
}

#[async_trait::async_trait]
impl ToolProvider for MockTools {
  /// 与 ra-tools 一致按名升序输出声明。
  async fn get_tool_schemas(&self) -> Vec<ToolSchema> {
    let mut names: Vec<String> = self.modes.keys().cloned().collect();
    names.sort();
    names
      .into_iter()
      .map(|name| ToolSchema {
        name,
        description: "mock tool for agent tests".to_string(),
        input_schema: Schema(serde_json::json!({})),
        constrained_sampling: None,
      })
      .collect()
  }

  /// 先记录调用再派发行为：panic 场景下记录依然完整。
  async fn execute(
    &self,
    call: ToolCall,
    ctx: ToolContext,
  ) -> RiboResult<ToolExecutionResult> {
    self
      .calls
      .lock()
      .expect("mock tools lock poisoned")
      .push(call.clone());
    let behavior = self
      .behaviors
      .lock()
      .expect("mock tools lock poisoned")
      .get(&call.id)
      .copied()
      .unwrap_or(self.default_behavior);

    match behavior {
      ToolBehavior::Ok => Ok(text_result("mock tool ok")),
      ToolBehavior::Err(message) => Err(RiboError::new(
        "tool",
        ra_tools::error::EXEC_FAILED,
        message,
      )),
      ToolBehavior::Cancel => Err(RiboError::new(
        "tool",
        ra_tools::error::CANCELLED,
        "mock tool cancelled",
      )),
      ToolBehavior::Terminate => Ok(ToolExecutionResult {
        terminate: true,
        ..text_result("terminate")
      }),
      ToolBehavior::Panic => panic!("mock tool panic"),
      ToolBehavior::Steer => {
        // 执行期间向会话注入 steering，验证轮间排水语义
        if let Some(session) = &self.session {
          session.enqueue_steering(user_text_message("inject"));
        }
        Ok(text_result("steered"))
      }
      ToolBehavior::CancelRoot => {
        // 执行期间触发会话取消根：abort 树级联到循环信号
        if let Some(session) = &self.session {
          session.abort_cancel();
        }
        Ok(text_result("root aborted"))
      }
      ToolBehavior::Hang => {
        // 与取消信号竞争的挂起：stop 触发后随信号收敛
        tokio::select! {
          _ = ctx.signal().cancelled() => Err(RiboError::new(
            "tool",
            ra_tools::error::CANCELLED,
            "mock tool hung then cancelled",
          )),
          _ = tokio::time::sleep(Duration::from_secs(60)) => {
            Ok(text_result("unhung"))
          }
        }
      }
      ToolBehavior::Sleep(millis) => {
        tokio::time::sleep(Duration::from_millis(millis)).await;
        Ok(text_result("slept"))
      }
    }
  }

  /// 查表返回执行模式；未知工具保守返回 Sequential。
  fn execution_mode(&self, name: &str) -> ToolExecutionMode {
    self
      .modes
      .get(name)
      .copied()
      .unwrap_or(ToolExecutionMode::Sequential)
  }
}

/// 单段文本的成功工具结果。
fn text_result(text: &str) -> ToolExecutionResult {
  ToolExecutionResult {
    content: vec![ToolOutput::Text(TextContent {
      content: text.to_string(),
      signature: None,
    })],
    ..ToolExecutionResult::default()
  }
}
