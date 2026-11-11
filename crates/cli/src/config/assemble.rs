//! schema → 运行时对象装配：Gateway / LoopDeps / Harness / CliSession
//!（14 详细设计 §6）。

use std::{sync::Arc, time::Duration};

use ra_agent::{
  adapter::ToolManagerAdapter, agent::BaseAgent, types::LoopDeps,
};
use ra_core::error::RiboResult;
use ra_harness::{
  CompactionConfig, Compactor, ContextConfig, Harness, HarnessContext,
  JsonlSessionStore, PersistentSessionProvider, SkillsLoader,
  SystemPromptBuilder,
};
use ra_provider::{
  adapters::{
    anthropic::AnthropicProvider, mock::MockStep,
    openai::completion::OpenAiCompatibleAdapter,
    openai::responses::OpenAiResponsesAdapter,
  },
  gateway::{Gateway, GatewayConfig, RouteMap},
  ids::{ModelId, ProviderId},
  model::Model,
  retry::RetryPolicy,
};
use ra_tools::manager::ToolManager;

use super::{ConfigMode, LoadOutcome};

/// CLI 内置系统提示模板（v1 不提供配置入口）。
const DEFAULT_CLI_TEMPLATE: &str = "\
You are a capable AI agent operating in a terminal workspace. \
Use the available tools to inspect files and run commands; \
prefer minimal, verifiable changes and report what you did.";

/// mock 模式的模型名（与 `ProviderSchema::default()` 对齐）。
const MOCK_MODEL: &str = "mock-model";

/// REPL 唯一运行时句柄（命名避开 `ra_agent::session::Session`）。
/// `harness` 为 `Arc<Harness>`：`prompt(&self)` 需跨 spawn 任务共享
///（概设 v2.4 所有权拆解——Harness 全 Arc 字段，包装零成本）。
pub struct CliSession {
  pub harness: Arc<Harness>,
  pub events: tokio::sync::broadcast::Receiver<ra_agent::events::AgentEvent>,
  pub mode: ConfigMode,
  pub warnings: Vec<String>,
  /// `/history` 默认条数等 REPL 行为参数。
  pub history_lines: usize,
  /// 初始会话键（session 层 `default_key`；`/new` 缺省自动轮换的前缀）。
  pub default_key: String,
  /// 装配期加载的技能名（`/skills` 输出）。
  pub skills: Vec<String>,
  /// 装配期技能诊断（已格式化为单行，`/skills` 输出）。
  pub skill_diagnostics: Vec<String>,
}

/// 解析示例用模型 ID。
fn mid(raw: &str) -> ModelId {
  ModelId::parse(raw).expect("valid model id")
}

/// 解析示例用提供方 ID。
fn pid(raw: &str) -> ProviderId {
  ProviderId::parse(raw).expect("valid provider id")
}

/// 构造满足网关注册校验的模型。
fn model_of(
  provider_id: &str,
  model_name: &str,
  base_url: impl Into<String>,
) -> Model {
  Model::new(
    pid(provider_id),
    mid(model_name),
    model_name.to_string(),
    base_url.into(),
  )
  .with_context_window(8192)
  .with_max_tokens(1024)
}

/// 真实模式网关：逐条构造适配器注册 + 跨提供方路由 + retry 参数。
fn build_real_gateway(
  outcome: &LoadOutcome,
) -> RiboResult<(Arc<Gateway>, Model)> {
  let cfg = GatewayConfig {
    retry: RetryPolicy {
      initial_backoff: Duration::from_millis(outcome.retry.initial_backoff_ms),
      max_retries: outcome.retry.max_retries,
      jitter_ratio: outcome.retry.jitter_ratio,
      ..RetryPolicy::default()
    },
    ..GatewayConfig::default()
  };
  let mut gateway = Gateway::new(cfg);
  for entry in &outcome.provider.providers {
    let adapter_model =
      model_of(&entry.provider_id, &entry.model, entry.base_url.clone());
    let api_key = Some(entry.api_key.clone());
    // 三个分支构造签名一致（id/models/http/api_key），差异在协议端点与
    // 请求/响应编解码——按 adapter 声明走各自的适配器
    let adapter = match entry.adapter.as_str() {
      "openai-chat" => Arc::new(OpenAiCompatibleAdapter::new(
        pid(&entry.provider_id),
        vec![adapter_model],
        reqwest::Client::new(),
        api_key,
      )) as Arc<dyn ra_provider::traits::Provider>,
      "openai-responses" => Arc::new(OpenAiResponsesAdapter::new(
        pid(&entry.provider_id),
        vec![adapter_model],
        reqwest::Client::new(),
        api_key,
      )) as Arc<dyn ra_provider::traits::Provider>,
      "anthropic" => Arc::new(AnthropicProvider::new(
        pid(&entry.provider_id),
        vec![adapter_model],
        reqwest::Client::new(),
        api_key,
      )) as Arc<dyn ra_provider::traits::Provider>,
      other => unreachable!("adapter 白名单已在 load 阶段拦截：{other}"),
    };
    gateway.register(adapter)?;
  }
  let mut routes = RouteMap::default();
  for (from, chain) in &outcome.provider.routing.cross_provider {
    routes
      .insert_cross_provider(pid(from), chain.iter().map(|c| pid(c)).collect());
  }
  let gateway = Arc::new(gateway.with_routes(routes));
  let primary = outcome
    .provider
    .providers
    .iter()
    .find(|p| p.model == outcome.provider.query_model)
    .expect("query_model 已在 load 阶段校验");
  let model = model_of(
    &primary.provider_id,
    &primary.model,
    primary.base_url.clone(),
  );
  Ok((gateway, model))
}

/// mock 模式网关：按脚本步骤构造（生产入口空脚本——回合以
/// INVALID_REQUEST 终结属预期，mock 模式定位为「克隆即跑」的装配演示
/// 与 E2E 基座；E2E 经 [`assemble_with_steps`] 精确供给脚本）。
fn build_mock_gateway(
  steps: Vec<MockStep>,
) -> RiboResult<(Arc<Gateway>, Model)> {
  let provider_id = pid("mock");
  let model = model_of("mock", MOCK_MODEL, "http://localhost");
  let mock = ra_provider::adapters::mock::MockProvider::new(
    provider_id.clone(),
    vec![model.clone()],
    steps,
  );
  let mut gateway = Gateway::new(GatewayConfig::default());
  gateway.register(Arc::new(mock))?;
  Ok((Arc::new(gateway), model))
}

/// 回合在途句柄：TUI 与 CLI 共用 prompt 发起逻辑（16 详细设计 §13.1）。
///
/// `done` 的类型为 `RiboResult<()>`（即 `Result<(), RiboError>`），
/// **不是** `Result<(), RiboResult<()>>`——避免嵌套 Result。
///
/// `done` 通道用于测试或主循环**可选**地观察回合结果（成功 / 失败原因）；
/// **Ctrl-C 退出流程直接 `await handle.task` 等待落盘，不消费 `done`**——
/// 任务本身在 `prompt` 返回后才结束，任务结束即等于「一轮对话落盘完成」。
pub struct TurnHandle {
  /// prompt 任务句柄（任务结束 = 落盘完成）。
  pub task: tokio::task::JoinHandle<()>,
  /// 回合结果通道（容量 1；任务在 `prompt` 返回后先投递再结束）。
  pub done: tokio::sync::mpsc::Receiver<RiboResult<()>>,
}

impl CliSession {
  /// 发起一轮对话并返回在途句柄（结果通道为复位权威，对齐 Repl.turn_done_rx）。
  ///
  /// **Send 边界**：`tokio::spawn` 要求闭包 Future 为 `Send`；`harness`
  /// 为 `Arc<Harness>`，`Harness::prompt` 的 await 链均为 `Send`，故闭包
  /// 满足 `Send`。若未来 `prompt` 引入非 `Send` 局部（如 `Rc`/`RefCell`
  /// 跨 await），须改用 `spawn_local` 或 `LocalSet`。
  ///
  /// # Examples
  ///
  /// ```no_run
  /// # use ra_cli::config::LoadOutcome;
  /// # async fn demo(outcome: LoadOutcome) -> Result<(), ra_core::error::RiboError> {
  /// let session = ra_cli::config::assemble::assemble(&outcome)?;
  /// let handle = session.spawn_turn("default", "hello");
  /// // 等待落盘完成：任务结束即等于一轮对话完成
  /// handle.task.await.expect("prompt 任务不 panic");
  /// # Ok(())
  /// # }
  /// ```
  pub fn spawn_turn(&self, key: &str, text: &str) -> TurnHandle {
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let harness = self.harness.clone(); // Arc<Harness>，Send
    let key = key.to_string();
    let text = text.to_string();
    // prompt 返回后先把结果塞进 done（容量 1，缓冲即返回，不阻塞任务
    // 结束），再让 task 结束——任务结束即「落盘完成」
    let task = tokio::spawn(async move {
      let result = harness.prompt(&key, &text).await.map(|_| ());
      let _ = tx.send(result).await;
    });
    TurnHandle { task, done: rx }
  }
}

/// 由 LoadOutcome 装配 REPL 会话（完整前链见 14 §6 装配映射表）。
///
/// mock 模式使用空脚本；E2E 需要按回合供给脚本时走
/// [`assemble_with_steps`]。
///
/// # Errors
///
/// 网关装配失败（provider 注册校验不通过等）时返回错误。
pub fn assemble(outcome: &LoadOutcome) -> RiboResult<CliSession> {
  assemble_with_steps(outcome, Vec::new())
}

/// E2E 专用装配入口：mock 网关注入脚本步骤（步骤按 `stream` 调用
/// 次序逐回合消费）。
///
/// # Errors
///
/// 同 [`assemble`]。
pub fn assemble_with_steps(
  outcome: &LoadOutcome,
  steps: Vec<MockStep>,
) -> RiboResult<CliSession> {
  // ===== 阶段 1：网关 =====
  let (gateway, model) = match outcome.mode {
    ConfigMode::Real => build_real_gateway(outcome)?,
    ConfigMode::Mock => build_mock_gateway(steps)?,
  };

  // ===== 阶段 2：持久化 + 技能 =====
  let store = Arc::new(JsonlSessionStore::new(
    outcome.harness.root.join("sessions"),
  ));
  let provider = Arc::new(PersistentSessionProvider::new(store.clone()));
  let skill_roots: Vec<std::path::PathBuf> = outcome
    .harness
    .skills_roots
    .iter()
    .map(|r| outcome.harness.root.join(r))
    .collect();
  let (skills, diagnostics) = SkillsLoader::new(skill_roots).load();
  for d in &diagnostics {
    tracing::warn!(code = ?d.code, path = ?d.location, "skill diagnostic");
  }
  // 诊断格式化留给输出层：/skills 逐条单行展示
  let skill_diagnostics = diagnostics
    .iter()
    .map(|d| {
      let location = d
        .location
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<root>".to_string());
      format!("{:?} {location}: {}", d.code, d.message)
    })
    .collect::<Vec<_>>();
  let skill_names = skills.iter().map(|s| s.name.clone()).collect();

  // ===== 阶段 3：上下文装配（compression 层 → ContextConfig.compaction 唯一真值）=====
  let ctx_cfg = ContextConfig {
    compaction: CompactionConfig {
      enabled: outcome.compression.enabled,
      trigger_ratio: outcome.compression.trigger_ratio,
      keep_recent_turns: outcome.compression.keep_recent_turns,
      summarizer_model: outcome
        .compression
        .summarizer_model
        .as_ref()
        .map(|m| mid(m)),
      max_summary_chars: outcome.compression.max_summary_chars,
    },
    include_environment: outcome.harness.include_environment,
  };
  let compactor = Arc::new(Compactor::new(gateway.clone(), model.clone()));
  let context = HarnessContext::new(
    SystemPromptBuilder::new().with_template(DEFAULT_CLI_TEMPLATE),
    Some(compactor),
    skills,
    ctx_cfg,
  );

  // ===== 阶段 4：内置工具 =====
  // 沙箱根：tools.fs_root 相对 root 解析（加载期已校验不含逃逸组件），
  // 未设置即 root 本身——沙箱恒位于工作空间之下
  let manager = ToolManager::new();
  if outcome.tools.builtin_enabled {
    let fs_root = match outcome.tools.fs_root.as_ref() {
      Some(sub) => outcome.harness.root.join(sub),
      None => outcome.harness.root.clone(),
    };
    ra_tools::builtin::register_builtin(&manager, &fs_root)?;
  }
  let tools = Arc::new(ToolManagerAdapter::new(Arc::new(manager)));

  // ===== 阶段 5：BaseAgent + Harness + CliSession =====
  let deps = LoopDeps {
    gateway: gateway.clone(),
    model: mid(&outcome.provider.query_model),
    tools,
    context_provider: Arc::new(context),
  };
  let agent = BaseAgent::new(deps, provider.clone());
  let harness = Arc::new(Harness::new(agent, provider, store));
  let events = harness.subscribe(); // §3 透传
  Ok(CliSession {
    harness,
    events,
    mode: outcome.mode,
    warnings: outcome.warnings.clone(),
    history_lines: outcome.session.history_lines,
    default_key: outcome.session.default_key.clone(),
    skills: skill_names,
    skill_diagnostics,
  })
}
