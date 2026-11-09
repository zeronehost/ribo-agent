//! ra-harness 完整会话示例（EX-01）。
//!
//! 演示：技能加载 → 系统提示组装 → Harness 门面两轮对话 → 模拟重启回放无重复。
//!
//! 运行：
//!
//! ```bash,ignore
//! cargo run -p ra-harness --example harness_session
//! ```

use std::sync::Arc;

use ra_agent::{
  adapter::ToolManagerAdapter, agent::BaseAgent,
  traits::PassthroughContextProvider, types::LoopDeps,
};
use ra_core::message::{AssistantMessage, Content, Message, StopReason};
use ra_provider::{
  adapters::mock::{MockProvider, MockStep},
  context::Context,
  gateway::{Gateway, GatewayConfig},
  ids::{ModelId, ProviderId},
  model::Model,
};
use ra_tools::manager::ToolManager;

use ra_harness::{
  Harness, JsonlSessionStore, PersistentSessionProvider, SkillsLoader,
  SystemPromptBuilder, format_skill_invocation,
};

/// 构造注册了 mock 提供方的网关，返回 `Arc<Gateway>` + `Arc<MockProvider>`。
fn build_mock_gateway(
  provider_id: ProviderId,
  model_id: ModelId,
  model_name: &str,
  model_url: &str,
  steps: Vec<MockStep>,
) -> (Arc<Gateway>, Arc<MockProvider>) {
  let model =
    Model::new(provider_id.clone(), model_id.clone(), model_name, model_url)
      .with_context_window(4096)
      .with_max_tokens(512);
  let mock =
    Arc::new(MockProvider::new(provider_id.clone(), vec![model], steps));
  let mut gateway = Gateway::new(GatewayConfig::default());
  gateway
    .register(mock.clone())
    .expect("register mock provider");
  (Arc::new(gateway), mock)
}

/// 构造一轮完整文本回复的 step。
fn text_step(model_id: &str, text: &str) -> MockStep {
  let mid = model_id.to_string();
  let t = text.to_string();
  MockStep::Complete(Arc::new(move |_ctx: &Context| {
    let mut msg = AssistantMessage::empty("mock", &mid, 0);
    msg.content = vec![Content::Text(ra_core::message::TextContent {
      content: t.clone(),
      signature: None,
    })];
    msg.stop_reason = StopReason::Stop;
    (StopReason::Stop, msg)
  }))
}

#[tokio::main]
async fn main() -> ra_core::error::RiboResult<()> {
  // ===== 阶段 0：临时目录 + 技能文件 =====
  // 产物落 workspace 的 tmp/harness_session/ 下（与 src/test_util.rs 的
  // tmp/ 约定一致）。用 CARGO_MANIFEST_DIR（crates/harness）向上两级定位
  // workspace 根——与运行 cargo run 时的 shell cwd 无关；每次运行前清理
  // 上次产物，退出后保留供人工检查。
  let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
    .ancestors()
    .nth(2)
    .expect("manifest has two ancestors")
    .join("tmp/harness_session");
  let _ = std::fs::remove_dir_all(&root);
  let data_dir = root.join("data");
  let skills_dir = root.join("skills");
  std::fs::create_dir_all(&data_dir).expect("create data dir");
  std::fs::create_dir_all(&skills_dir).expect("create skills dir");

  // 写一个目录型技能：<skills_dir>/terminal/SKILL.md
  let terminal_dir = skills_dir.join("terminal");
  std::fs::create_dir_all(&terminal_dir).expect("create terminal dir");
  std::fs::write(
    terminal_dir.join("SKILL.md"),
    "---\n\
     name: terminal\n\
     description: Execute shell commands safely\n\
     ---\n\
     You have access to the terminal. Prefer simple, single-command invocations.",
  )
  .expect("write terminal skill");

  // 写一个扁平型技能：<skills_dir>/note.md
  std::fs::write(
    skills_dir.join("note.md"),
    "---\n\
     name: note\n\
     description: Take structured notes during the session\n\
     ---\n\
     Always log key decisions in a structured format.",
  )
  .expect("write note skill");

  tracing::info!(out_dir = %root.display(), "output directory ready");

  // ===== 阶段 1：加载技能 =====
  let loader = SkillsLoader::new(vec![skills_dir.clone()]);
  let (skills, diagnostics) = loader.load();
  tracing::info!(
    skill_count = skills.len(),
    diagnostic_count = diagnostics.len(),
    "skills loaded"
  );

  for d in &diagnostics {
    tracing::warn!(
      location = ?d.location,
      code = ?d.code,
      message = %d.message,
      "skill diagnostic"
    );
  }

  assert_eq!(skills.len(), 2, "terminal + note skills");
  tracing::info!(
    "skill names: {}",
    skills
      .iter()
      .map(|s| s.name.as_str())
      .collect::<Vec<_>>()
      .join(", ")
  );

  // ===== 阶段 2：系统提示组装 =====
  let system_prompt = SystemPromptBuilder::new()
    .with_template("You are a helpful assistant.")
    .with_environment(true)
    .build(&skills, None);

  assert_eq!(system_prompt.len(), 1, "single text content");
  tracing::info!("system prompt built");

  // 技能显式调用格式示例（展示用）
  let invocation = format_skill_invocation(
    &skills[0],
    Some("Use this skill for shell tasks."),
  );
  tracing::info!("skill invocation preview:\n{}", invocation);

  // ===== 阶段 3：Harness 装配 =====
  let provider_id = ProviderId::parse("mock").unwrap();
  let model_id = ModelId::parse("mock-model").unwrap();

  // 两轮对话 step：先回 "Hello, world!"，再回 "Goodbye!"
  let steps = vec![
    text_step("mock-model", "Hello, world!"),
    text_step("mock-model", "Goodbye!"),
  ];
  let (gateway, _mock) = build_mock_gateway(
    provider_id.clone(),
    model_id.clone(),
    "Mock Model",
    "http://localhost",
    steps,
  );

  let tools = Arc::new(ToolManagerAdapter::new(Arc::new(ToolManager::new())));
  let context_provider =
    Arc::new(PassthroughContextProvider::new(system_prompt));

  let deps = LoopDeps {
    gateway,
    model: model_id,
    tools,
    context_provider,
  };

  let store = Arc::new(JsonlSessionStore::new(data_dir.join("sessions")));
  let provider = Arc::new(PersistentSessionProvider::new(store.clone()));
  let agent = BaseAgent::new(deps, provider.clone());
  let harness = Harness::new(agent, provider.clone(), store.clone());

  // ===== 阶段 4：两轮对话 =====
  let msgs = harness.prompt("demo", "Hello").await?;
  tracing::info!("round 1: {} new messages", msgs.len());
  for m in &msgs {
    let tag = match m {
      Message::User(_) => "user",
      Message::Assistant(_) => "assistant",
      Message::ToolResult(_) => "tool_result",
    };
    tracing::info!("  {tag}");
  }

  let msgs = harness.prompt("demo", "Bye").await?;
  tracing::info!("round 2: {} new messages", msgs.len());

  let transcript = harness.session_messages("demo").await?;
  tracing::info!("transcript after 2 rounds: {} messages", transcript.len());
  assert_eq!(transcript.len(), 4, "user1 + asst1 + user2 + asst2");

  // ===== 阶段 5：模拟重启（drop → 重建 Harness，回放无重复）=====
  drop(harness);
  drop(provider);
  drop(store);

  tracing::info!("--- simulating process restart ---");

  let store2 = Arc::new(JsonlSessionStore::new(data_dir.join("sessions")));
  let provider2 = Arc::new(PersistentSessionProvider::new(store2.clone()));
  let agent2 = BaseAgent::new(
    LoopDeps {
      // 重启后 gateway 里没 steps 了，但我们只读取，不调用
      gateway: Arc::new(Gateway::new(GatewayConfig::default())),
      model: ModelId::parse("mock-model").unwrap(),
      tools: Arc::new(ToolManagerAdapter::new(Arc::new(ToolManager::new()))),
      context_provider: Arc::new(PassthroughContextProvider::new(Vec::new())),
    },
    provider2.clone(),
  );
  let harness2 = Harness::new(agent2, provider2, store2.clone());

  let replay = harness2.session_messages("demo").await?;
  tracing::info!("replay after restart: {} messages", replay.len());
  assert_eq!(
    replay.len(),
    4,
    "replay should match original, no duplicates"
  );
  // 验证转录首条是 user: "Hello"、尾条是 asst: "Goodbye!"
  assert!(matches!(&replay[0], ra_core::message::Message::User(_)));
  assert!(matches!(
    &replay[3],
    ra_core::message::Message::Assistant(_)
  ));

  tracing::info!("EX-01 completed successfully");
  Ok(())
}
