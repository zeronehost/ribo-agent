//! ra-agent 主循环完整示例：mock Provider + 内置工具 + 多轮工具调用 + steering 注入 + abort。
//!
//! 本示例不依赖任何真实模型或密钥，全部通过 `ra-provider` 的脚本化 MockProvider
//! 驱动，适合新开发者理解「一次 prompt 从输入到落盘视图」的完整链路。
//! 演示分为三个独立场景，各自使用独立的会话与 agent 实例：
//!
//! 1. **多轮工具调用**：模拟模型先后调用内置 `echo` 与 `calculator`，两轮工具后自然收敛。
//! 2. **steering 注入**：工具执行期间向会话注入一条用户消息，验证其在下一轮 LLM 请求前生效。
//! 3. **abort 中止**：用 `Silent` step 制造运行窗口，调用 `stop()` 触发取消，循环以 Aborted 收敛。
//!
//! 运行：
//!
//! ```bash,ignore
//! cargo run -p ra-agent --example agent_mock_loop
//! ```

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ra_agent::{
  adapter::ToolManagerAdapter,
  agent::BaseAgent,
  events::AgentEvent,
  traits::{Agent, SessionProvider, user_text_message},
  types::LoopDeps,
};
use ra_core::error::{RiboError, RiboResult};
use ra_core::message::{
  AssistantMessage, Content, Message, Schema, StopReason, TextContent,
  ToolCall, ToolSchema, UserContent,
};
use ra_provider::{
  adapters::mock::{MockProvider, MockStep},
  context::Context,
  gateway::{Gateway, GatewayConfig},
  ids::{ModelId, ProviderId},
  model::Model,
};
use ra_tools::{
  builtin::{create_calculator_tool, create_echo_tool},
  context::ToolContext,
  error::CANCELLED,
  manager::ToolManager,
  traits::AgentTool,
  types::{ToolExecutionResult, ToolOutput},
};

/// mock 脚本里使用的模型 ID；与 LoopDeps.model 保持一致，网关才能路由到 mock。
const MODEL: &str = "mock-model";
/// 场景 2 注入的 steering 文本，用于验证其在下一轮请求中出现。
const STEER_TEXT: &str = "【注入】请用一句话总结刚才的工具结果";

// ---------------------------------------------------------------------------
// 通用 step 构造
// ---------------------------------------------------------------------------

/// 构造一条「纯文本并结束回合」的 mock step。
/// MockStep::Complete 的闭包在每次请求时调用，返回停止原因与最终助手消息；
/// provider / model / timestamp 由适配器改写为 mock 的真实归属。
fn text_step(text: &str) -> MockStep {
  let owned = text.to_string();
  MockStep::Complete(Arc::new(move |_ctx: &Context| {
    (StopReason::Stop, text_assistant(&owned))
  }))
}

/// 构造一条「请求调用工具」的 mock step（回合未结束，StopReason::ToolUse）。
/// 循环执行完工具批后会自动发起下一次 LLM 请求，消费脚本中的下一个 step。
fn tool_call_step(id: &str, name: &str, args: serde_json::Value) -> MockStep {
  let id = id.to_string();
  let name = name.to_string();
  MockStep::Complete(Arc::new(move |_ctx: &Context| {
    let mut msg = AssistantMessage::empty("mock", MODEL, 0);
    msg.content = vec![Content::ToolCall(ToolCall {
      id: id.clone(),
      name: name.clone(),
      arguments: args.clone(),
      signature: None,
      namespace: None,
    })];
    // empty() 的初始停止原因为 Pending，必须显式改为 ToolUse。
    msg.stop_reason = StopReason::ToolUse;
    (StopReason::ToolUse, msg)
  }))
}

/// 纯文本助手消息（停止原因 Stop），供 text_step 复用。
fn text_assistant(text: &str) -> AssistantMessage {
  let mut msg = AssistantMessage::empty("mock", MODEL, 0);
  msg.content = vec![Content::Text(TextContent {
    content: text.to_string(),
    signature: None,
  })];
  msg.stop_reason = StopReason::Stop;
  msg
}

/// 把一轮运行产生的事件按类型打印出来，便于观察循环内部时序。
/// 传入的 receiver 必须在 prompt 之前订阅完成，否则先前的事件已被丢弃。
fn print_events(rx: &mut tokio::sync::broadcast::Receiver<AgentEvent>) {
  while let Ok(event) = rx.try_recv() {
    println!("    [event] {}", event.type_tag());
  }
}

// ---------------------------------------------------------------------------
// Agent 装配
// ---------------------------------------------------------------------------

/// 用给定的 mock 脚本与工具表构造一个 Agent，返回它和 mock 提供方句柄。
/// 返回 Arc<BaseAgent> 是因为 abort 场景需要把 agent 克隆进后台任务。
fn build_agent(
  steps: Vec<MockStep>,
  tools: Arc<ToolManager>,
) -> (Arc<BaseAgent>, Arc<MockProvider>) {
  // 1) 网关：注册一个脚本化 mock 提供方。上下文窗口与输出限额必须非零，
  //    否则网关注册校验会拒绝。
  let provider_id = ProviderId::parse("mock").expect("mock 提供方 ID 合法");
  let model = Model::new(
    provider_id.clone(),
    ModelId::parse(MODEL).expect("mock 模型 ID 合法"),
    "Mock Model",
    "http://localhost",
  )
  .with_context_window(8192)
  .with_max_tokens(1024);
  let mock =
    Arc::new(MockProvider::new(provider_id.clone(), vec![model], steps));
  let mut gateway = Gateway::new(GatewayConfig::default());
  gateway.register(mock.clone()).expect("mock 提供方注册成功");

  // 2) 依赖：工具边界由 ToolManagerAdapter 把 ra-tools 的 ToolManager 桥接为
  //    agent 侧的 ToolProvider；上下文用直通实现（无 system prompt、无压缩）。
  let deps = LoopDeps {
    gateway: Arc::new(gateway),
    model: ModelId::parse(MODEL).expect("mock 模型 ID 合法"),
    tools: Arc::new(ToolManagerAdapter::new(tools)),
    context_provider: Arc::new(
      ra_agent::traits::PassthroughContextProvider::new(Vec::new()),
    ),
  };

  // 3) 会话表：SessionManager 是纯内存实现，本示例不涉及持久化。
  let agent =
    BaseAgent::new(deps, Arc::new(ra_agent::session::SessionManager::new()));
  (Arc::new(agent), mock)
}

// ---------------------------------------------------------------------------
// 场景 1：多轮工具调用（内置 echo + calculator）
// ---------------------------------------------------------------------------

/// 演示模型连续两轮调用内置工具：
/// 第一轮请求 echo 回显文本，第二轮请求 calculator 求值，第三轮文本收敛。
/// 脚本的 step 顺序即调用顺序，由 MockProvider 按 FIFO 消费。
async fn demo_multi_turn() -> RiboResult<()> {
  println!("\n=== 场景 1：多轮工具调用（echo → calculator） ===");

  let steps = vec![
    tool_call_step("c1", "echo", serde_json::json!({ "text": "hello ribo" })),
    tool_call_step(
      "c2",
      "calculator",
      serde_json::json!({ "expression": "12*3+4" }),
    ),
    text_step("两轮工具调用完成"),
  ];

  let manager = ToolManager::new();
  manager.register(create_echo_tool())?;
  manager.register(create_calculator_tool())?;

  let (agent, mock) = build_agent(steps, Arc::new(manager));
  // 订阅必须在 prompt 之前：先建立 receiver，事件才会被 broadcast 通道保留。
  let mut rx = agent.subscribe();
  let messages = agent
    .prompt("demo-multi", "依次调用 echo 和 calculator")
    .await?;

  print_events(&mut rx);
  println!(
    "  新增消息数: {}（user + asst(tool) + tool_result ×2 + asst(final)）",
    messages.len()
  );
  println!("  LLM 调用次数: {}", mock.record().len());
  assert_eq!(mock.record().len(), 3, "三轮 LLM 请求各消费一个 step");

  // 回灌的工具结果应包含 echo / calculator 的输出。
  let tool_results = messages
    .iter()
    .filter(|m| matches!(m, Message::ToolResult(_)))
    .count();
  assert_eq!(tool_results, 2, "echo 与 calculator 各回灌一条结果");
  Ok(())
}

// ---------------------------------------------------------------------------
// 场景 2：steering 注入
// ---------------------------------------------------------------------------

/// 演示用慢工具：execute 期间先等待（模拟耗时），随后向会话注入 steering 消息。
/// 持有 SessionManager 与会话键，调用其公开的 enqueue_steering 入队；
/// 这与运行中调用 `agent.steer(...)` 等价（后者额外要求会话处于活动态）。
/// 等待期同时监听取消信号，保证 abort 时工具能及时收敛。
struct SteerEchoTool {
  schema: ToolSchema,
  sessions: Arc<ra_agent::session::SessionManager>,
  key: String,
  delay: Duration,
}

#[async_trait::async_trait]
impl AgentTool for SteerEchoTool {
  fn schema(&self) -> &ToolSchema {
    &self.schema
  }

  fn label(&self) -> &str {
    "Steering Echo"
  }

  async fn execute(
    &self,
    _tool_call_id: &str,
    args: serde_json::Value,
    ctx: &ToolContext,
  ) -> RiboResult<ToolExecutionResult> {
    let text = args
      .get("text")
      .and_then(serde_json::Value::as_str)
      .unwrap_or("")
      .to_string();

    // 等待期即 steering 注入窗口；取消信号到达时立即以取消错误收敛。
    tokio::select! {
      _ = ctx.signal().cancelled() => {
        return Err(RiboError::new(
          "tool",
          CANCELLED,
          "steering echo cancelled",
        ));
      }
      _ = tokio::time::sleep(self.delay) => {}
    }

    // 工具执行期间入队：循环在本轮工具批结束后排水，下一轮 LLM 请求前注入。
    let session = self.sessions.get_or_create(&self.key).await?;
    session.enqueue_steering(user_text_message(STEER_TEXT.to_string()));

    Ok(ToolExecutionResult {
      content: vec![ToolOutput::Text(TextContent {
        content: text,
        signature: None,
      })],
      ..Default::default()
    })
  }
}

/// 构造 SteerEchoTool 的工具声明（名称须与 mock 脚本里的 ToolCall.name 一致）。
fn steer_echo_tool(
  sessions: Arc<ra_agent::session::SessionManager>,
  key: &str,
  delay: Duration,
) -> SteerEchoTool {
  SteerEchoTool {
    schema: ToolSchema {
      name: "steer_echo".to_string(),
      description: "回显文本并在执行期间注入一条 steering 消息".to_string(),
      input_schema: Schema(serde_json::json!({
        "type": "object",
        "properties": { "text": { "type": "string" } },
        "required": ["text"]
      })),
      constrained_sampling: None,
    },
    sessions,
    key: key.to_string(),
    delay,
  }
}

/// 演示 steering：第 1 轮工具执行期间注入消息，第 2 轮请求应包含该消息。
/// 第 2 个 step 的闭包把本次请求的完整消息列表存到共享变量供断言。
async fn demo_steering() -> RiboResult<()> {
  println!("\n=== 场景 2：steering 注入（工具执行期入队，下一轮生效） ===");

  let key = "demo-steer";
  let sessions = Arc::new(ra_agent::session::SessionManager::new());

  // 捕获第 2 轮 LLM 请求的上下文，验证注入消息已进入。
  let captured: Arc<Mutex<Vec<Message>>> = Arc::new(Mutex::new(Vec::new()));
  let recorder = captured.clone();
  let steps = vec![
    tool_call_step("s1", "steer_echo", serde_json::json!({ "text": "已回显" })),
    MockStep::Complete(Arc::new(move |ctx: &Context| {
      *recorder.lock().expect("捕获锁有效") = ctx.message.clone();
      (StopReason::Stop, text_assistant("已处理注入消息"))
    })),
  ];

  let manager = ToolManager::new();
  manager.register(Arc::new(steer_echo_tool(
    sessions.clone(),
    key,
    Duration::from_millis(300),
  )))?;

  // 工具需要会话表引用，而 build_agent 内部会新建另一个会话表；
  // 这里手动装配以确保工具与主循环共享同一个 SessionManager。
  let provider_id = ProviderId::parse("mock").expect("mock 提供方 ID 合法");
  let model = Model::new(
    provider_id.clone(),
    ModelId::parse(MODEL).expect("mock 模型 ID 合法"),
    "Mock Model",
    "http://localhost",
  )
  .with_context_window(8192)
  .with_max_tokens(1024);
  let mock = Arc::new(MockProvider::new(provider_id, vec![model], steps));
  let mut gateway = Gateway::new(GatewayConfig::default());
  gateway.register(mock.clone())?;
  let deps = LoopDeps {
    gateway: Arc::new(gateway),
    model: ModelId::parse(MODEL).expect("mock 模型 ID 合法"),
    tools: Arc::new(ToolManagerAdapter::new(Arc::new(manager))),
    context_provider: Arc::new(
      ra_agent::traits::PassthroughContextProvider::new(Vec::new()),
    ),
  };
  let agent = Arc::new(BaseAgent::new(deps, sessions));

  let mut rx = agent.subscribe();
  let _ = agent.prompt(key, "开始").await?;
  print_events(&mut rx);

  let snapshot = captured.lock().expect("捕获锁有效").clone();
  let injected = snapshot.iter().any(|m| {
    matches!(m, Message::User(u) if matches!(&u.content, UserContent::Text(t) if t == STEER_TEXT))
  });
  println!("  第 2 轮请求消息数: {}", snapshot.len());
  println!("  注入消息已进入下一轮请求: {injected}");
  assert!(injected, "steering 消息应在下一轮 LLM 请求前注入");
  Ok(())
}

// ---------------------------------------------------------------------------
// 场景 3：abort 中止
// ---------------------------------------------------------------------------

/// 演示 abort：Silent step 建流后静默，后台任务等到会话活动后调用 stop()，
/// 取消信号级联到 Silent step，循环以 Aborted 收敛（prompt 仍返回 Ok）。
async fn demo_abort() -> RiboResult<()> {
  println!("\n=== 场景 3：abort 中止（Silent + stop） ===");

  // 静默 2 秒提供中止窗口；若 stop 未赶上，Silent 到期会以 Stop 收敛空消息。
  let steps = vec![MockStep::Silent {
    duration: Duration::from_secs(2),
  }];
  let manager = ToolManager::new();
  let (agent, mock) = build_agent(steps, Arc::new(manager));

  let key = "demo-abort";
  let stopper = {
    let agent = agent.clone();
    let mock = mock.clone();
    tokio::spawn(async move {
      // 等到第一次 LLM 调用已经发起（record 非空即 stream() 已进入），
      // 再触发取消。若仅等 has_active_tasks，stop 可能早于建流发生，
      // 取消会在 pre-stream 阶段生效，演示不到「Silent 流被取消」的路径。
      let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
      while mock.record().is_empty() {
        assert!(
          tokio::time::Instant::now() < deadline,
          "LLM 调用应在 1 秒内发起"
        );
        tokio::task::yield_now().await;
      }
      let ok = agent.stop(key).await;
      println!("    stop() 返回: {ok}");
      ok
    })
  };

  let mut rx = agent.subscribe();
  let messages = agent.prompt(key, "开始一个会被中止的任务").await?;
  let stopped = stopper.await.expect("stop 任务不应 panic");

  print_events(&mut rx);
  println!("  中止后返回消息数: {}", messages.len());
  println!(
    "  中止后仍有活动任务: {}",
    agent.has_active_tasks(key).await
  );
  assert!(stopped, "活动会话的 stop 应返回 true");
  assert!(!agent.has_active_tasks(key).await, "中止后运行标志应复位");
  assert_eq!(mock.record().len(), 1, "仅发起一次 LLM 请求");
  Ok(())
}

#[tokio::main]
async fn main() -> RiboResult<()> {
  demo_multi_turn().await?;
  demo_steering().await?;
  demo_abort().await?;
  println!("\nagent_mock_loop 三个场景全部通过");
  Ok(())
}
