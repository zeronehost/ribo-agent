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

/// 组装一轮 Harness（共享同一 store 目录）。
fn build_harness(
  store: Arc<JsonlSessionStore>,
  steps: Vec<MockStep>,
) -> (Harness, Arc<PersistentSessionProvider>) {
  let provider = Arc::new(PersistentSessionProvider::new(store.clone()));
  let deps = LoopDeps {
    gateway: mock_gateway(steps),
    model: ModelId::parse("mock-model").expect("mock 模型 ID 合法"),
    tools: Arc::new(NoTools),
    context_provider: Arc::new(PassthroughContextProvider::default()),
  };
  let agent = BaseAgent::new(deps, provider.clone());
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
