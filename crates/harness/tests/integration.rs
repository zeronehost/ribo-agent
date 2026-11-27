//! ra-harness 集成测试：会话跨「进程重启」持久化与回放。

use std::sync::Arc;

use ra_agent::{
  agent::BaseAgent,
  traits::{PassthroughContextProvider, ToolProvider, user_text_message},
  types::LoopDeps,
};
use ra_core::{
  error::RiboResult,
  message::{AssistantMessage, Content, StopReason, TextContent, ToolSchema},
};
use ra_provider::{
  adapters::mock::{MockProvider, MockStep},
  gateway::{Gateway, GatewayConfig},
  ids::{ModelId, ProviderId},
  model::Model,
};
use ra_tools::{
  context::ToolContext,
  types::{ToolExecutionMode, ToolExecutionResult},
};

use ra_harness::{
  Harness, HarnessConfig, JsonlSessionStore, PersistentSessionProvider,
  test_util::test_dir,
};

/// 最小工具边界：无工具、执行永不被调用（mock 直接 Stop）。
struct NoTools;

#[async_trait::async_trait]
impl ToolProvider for NoTools {
  async fn get_tool_schemas(&self) -> Vec<ToolSchema> {
    Vec::new()
  }

  async fn execute(
    &self,
    _call: ra_core::message::ToolCall,
    _ctx: ToolContext,
  ) -> RiboResult<ToolExecutionResult> {
    // mock provider 始终返回 Stop，工具执行不会被调用
    unreachable!("NoTools::execute should not be called")
  }

  fn execution_mode(&self, _name: &str) -> ToolExecutionMode {
    ToolExecutionMode::Sequential
  }
}

/// 构造注册了 mock 提供方的网关（模型 ID 固定为 `mock-model`）。
fn mock_gateway(steps: Vec<MockStep>) -> Arc<Gateway> {
  let provider_id = ProviderId::parse("mock").expect("mock 提供方 ID 合法");
  let model = Model::new(
    provider_id.clone(),
    ModelId::parse("mock-model").expect("mock 模型 ID 合法"),
    "Mock Model",
    "http://localhost",
  )
  .with_context_window(8192)
  .with_max_tokens(1024);
  let mock = Arc::new(MockProvider::new(provider_id, vec![model], steps));
  let mut gateway = Gateway::new(GatewayConfig::default());
  gateway.register(mock).expect("mock 提供方注册成功");
  Arc::new(gateway)
}

/// 回显工具边界：执行 echo 调用并返回成功结果（用于制造多轮 tool_use）。
struct EchoTools;

#[async_trait::async_trait]
impl ToolProvider for EchoTools {
  async fn get_tool_schemas(&self) -> Vec<ToolSchema> {
    Vec::new()
  }

  async fn execute(
    &self,
    call: ra_core::message::ToolCall,
    _ctx: ToolContext,
  ) -> RiboResult<ToolExecutionResult> {
    Ok(ToolExecutionResult {
      content: vec![ra_tools::types::ToolOutput::Text(TextContent {
        content: call.arguments["text"].as_str().unwrap_or("ok").to_string(),
        signature: None,
      })],
      ..Default::default()
    })
  }

  fn execution_mode(&self, _name: &str) -> ToolExecutionMode {
    ToolExecutionMode::Sequential
  }
}

/// 组装一轮 Harness（共享同一 store 目录）。
fn build_harness(
  store: Arc<JsonlSessionStore>,
  steps: Vec<MockStep>,
) -> (Harness, Arc<PersistentSessionProvider>) {
  build_harness_tuned(store, steps, None)
}

/// 可注入 max_iterations 的构造器（None = 用默认轮次）。
fn build_harness_tuned(
  store: Arc<JsonlSessionStore>,
  steps: Vec<MockStep>,
  max_iterations: Option<usize>,
) -> (Harness, Arc<PersistentSessionProvider>) {
  build_harness_full(store, steps, max_iterations, Arc::new(NoTools))
}

/// 完整构造器：可替换工具边界（超限场景需要 EchoTools）。
fn build_harness_full(
  store: Arc<JsonlSessionStore>,
  steps: Vec<MockStep>,
  max_iterations: Option<usize>,
  tools: Arc<dyn ToolProvider>,
) -> (Harness, Arc<PersistentSessionProvider>) {
  let provider = Arc::new(PersistentSessionProvider::new(store.clone()));
  let deps = LoopDeps {
    gateway: mock_gateway(steps),
    model: ModelId::parse("mock-model").expect("mock 模型 ID 合法"),
    tools,
    context_provider: Arc::new(PassthroughContextProvider::default()),
  };
  let mut agent = BaseAgent::new(deps, provider.clone());
  if let Some(limit) = max_iterations {
    agent = agent.with_max_iterations(limit);
  }
  (Harness::new(agent, provider.clone(), store), provider)
}

/// 构造 Stop 终止的纯文本助手步骤。
fn stop_step(text: &'static str) -> MockStep {
  let text = text.to_string();
  MockStep::Complete(Arc::new(move |_ctx| {
    let mut msg = AssistantMessage::empty("mock", "mock-model", 0);
    msg.content = vec![Content::Text(TextContent {
      content: text.clone(),
      signature: None,
    })];
    msg.stop_reason = StopReason::Stop;
    (StopReason::Stop, msg)
  }))
}

/// 构造 ToolUse 助手步骤（触发下一轮，配合极小 max_iterations 制造超限）。
fn tool_use_step(call_id: &'static str) -> MockStep {
  MockStep::Complete(Arc::new(move |_ctx| {
    let mut msg = AssistantMessage::empty("mock", "mock-model", 0);
    msg.content = vec![Content::ToolCall(ra_core::message::ToolCall {
      id: call_id.to_string(),
      name: "echo".to_string(),
      arguments: serde_json::json!({ "text": "hi" }),
      signature: None,
      namespace: None,
    })];
    msg.stop_reason = StopReason::ToolUse;
    (StopReason::ToolUse, msg)
  }))
}

/// 会话跨「重启」持久化，回放无重复，continue_run 成功。
///
/// 场景：两轮 prompt 落盘 → 丢弃全部内存结构（模拟进程重启）→
/// 用同一磁盘目录重建 Provider/Agent → 回放校验 → 模拟一条未处理的
/// user 消息（中断状态）→ continue_run → 再落盘后可二次冷启动。
#[tokio::test]
async fn session_survives_restart_without_duplicates() {
  let dir = test_dir("session_survives_restart_without_duplicates");
  let config = HarnessConfig::new(dir.clone());
  let store = Arc::new(JsonlSessionStore::new(config.sessions_dir()));

  // 第一轮：两步 mock（首轮 prompt + 续跑用）
  let (harness, _provider) = build_harness(
    store.clone(),
    vec![stop_step("reply-1"), stop_step("reply-2")],
  );

  // 首轮 prompt：user1 + assistant1
  let round1 = harness.prompt("s1", "hi").await.unwrap();
  assert_eq!(round1.len(), 2);

  // 二轮 prompt：user2 + assistant2
  let round2 = harness.prompt("s1", "how are you").await.unwrap();
  assert_eq!(round2.len(), 2);

  // 内存转录共 4 条
  let snapshot = harness.session_messages("s1").await.unwrap();
  assert_eq!(snapshot.len(), 4);

  // 模拟重启：丢弃 harness / agent / provider，仅保留磁盘
  drop(harness);

  // 用同一目录重建
  let (harness2, provider2) =
    build_harness(store.clone(), vec![stop_step("reply-continue")]);

  // 冷启动回放：4 条消息，无重复
  let replayed = harness2.session_messages("s1").await.unwrap();
  assert_eq!(replayed.len(), 4, "回放应还原 4 条消息，无重复");

  // 模拟中断状态：转录尾条为 assistant，需追加一条 user 才能 continue_run
  // （对应「进程在用户输入后、助手响应前被杀」的真实场景）
  {
    use ra_agent::traits::SessionProvider;
    let session = provider2.get_or_create("s1").await.unwrap();
    session.append_messages(&[user_text_message("continue?")]);
  }

  // continue_run 应成功并返回新增助手消息
  let continued = harness2.continue_run("s1").await.unwrap();
  assert_eq!(continued.len(), 1);
  assert!(matches!(
    continued[0],
    ra_core::message::Message::Assistant(_)
  ));

  // 落盘后再次冷启动：5 条（4 + continue_run 产出的 1 条）
  drop(harness2);
  let (harness3, _provider3) = build_harness(store.clone(), Vec::new());
  let final_snapshot = harness3.session_messages("s1").await.unwrap();
  assert_eq!(final_snapshot.len(), 5, "二次冷启动应含续跑产物");
}

/// 轮次耗尽的已产出消息不丢失：错误路径落盘 → 重启回放保留 → 可续跑。
///
/// 场景：max_iterations=1 + 首轮 tool_use（产出 user + 助手 + 工具结果后超限）
/// → prompt 返回 agent.max_iterations → 磁盘含 3 条 → 重建后回放 3 条
/// → 追加 user 后续跑成功并再落盘。
#[tokio::test]
async fn max_iterations_output_persists_and_can_continue() {
  let dir = test_dir("max_iterations_output_persists_and_can_continue");
  let config = HarnessConfig::new(dir.clone());
  let store = Arc::new(JsonlSessionStore::new(config.sessions_dir()));

  // 首轮 tool_use 产出工具结果，随后第 2 轮前触发超限（limit=1）。
  let (harness, _provider) = build_harness_full(
    store.clone(),
    vec![tool_use_step("c1")],
    Some(1),
    Arc::new(EchoTools),
  );
  let err = harness
    .prompt("s1", "analyze")
    .await
    .expect_err("应因轮次超限失败");
  assert_eq!(err.code(), "agent.max_iterations");

  // 内存转录含 3 条：user + 助手(tool_use) + 工具结果
  let in_memory = harness.session_messages("s1").await.unwrap();
  assert_eq!(in_memory.len(), 3);

  // 关键：失败路径也已落盘（此前 ? 短路会跳过，重启后丢失）
  drop(harness);
  let (harness2, provider2) =
    build_harness(store.clone(), vec![stop_step("resumed")]);
  let replayed = harness2.session_messages("s1").await.unwrap();
  assert_eq!(replayed.len(), 3, "超限产出应在重启后回放保留");

  // 尾条为工具结果（非助手），可直接续跑
  {
    use ra_agent::traits::SessionProvider;
    let session = provider2.get_or_create("s1").await.unwrap();
    session.append_messages(&[user_text_message("continue")]);
  }
  let continued = harness2.continue_run("s1").await.unwrap();
  assert_eq!(continued.len(), 1);
  assert!(matches!(
    continued[0],
    ra_core::message::Message::Assistant(_)
  ));

  // 续跑产物落盘，再次冷启动含 4 条：超限产出 3 + 续跑助手 1。
  // 手动 append 的 user 未走 harness 落盘路径，故不计入磁盘。
  drop(harness2);
  let (harness3, _provider3) = build_harness(store.clone(), Vec::new());
  let final_snapshot = harness3.session_messages("s1").await.unwrap();
  assert_eq!(final_snapshot.len(), 4, "续跑产物应落盘");
  assert!(matches!(
    final_snapshot.last().expect("非空转录"),
    ra_core::message::Message::Assistant(_)
  ));
}
