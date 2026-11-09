//! 真实 provider + 配置驱动 + 跨提供方降级的会话容器示例（EX-02）。
//!
//! 在 [`harness_session`](crate) 的基础上叠加 provider 层真实能力：
//!
//! 1. **配置驱动**：provider 列表与降级路由从 TOML 文件读取
//!    （`harness_fallback.toml`；缺失/不完整时自动回退离线桩，
//!    详见 `harness_fallback_example.toml` 模板）。
//! 2. **真实适配器**：`openai-chat` / `openai-responses` / `anthropic`
//!    三类适配器按配置动态构造并注册网关，不使用 MockProvider。
//! 3. **跨提供方降级**：`[routing.cross_provider]` 声明降级链，
//!    主链建流失败（限流/配额/鉴权）时网关沿链切换候选。
//! 4. **会话容器全套**：[`Harness`] 组合技能加载、系统提示组装、
//!    上下文装配（[`HarnessContext`]）与 JSONL 持久化；
//!    一轮对话后模拟重启，回放转录无重复。
//!
//! 运行（产物固定落 `<workspace>/tmp/harness_fallback/`）：
//!
//! ```text
//! cargo run -p ra-harness --example harness_fallback
//! ```
//!
//! 离线模式的桩只接受一次连接，因此两种模式都只演示**一轮**对话 +
//! 重启回放；多轮对话见 [`harness_session`]。

use std::{
  collections::{HashMap, HashSet},
  path::Path,
  sync::Arc,
  time::Duration,
};

use ra_agent::{
  adapter::ToolManagerAdapter, agent::BaseAgent, types::LoopDeps,
};
use ra_core::{error::RiboResult, message::Message};
use ra_harness::{
  CompactionConfig, Compactor, ContextConfig, Harness, HarnessContext,
  JsonlSessionStore, PersistentSessionProvider, SkillsLoader,
  SystemPromptBuilder,
};
use ra_provider::{
  adapters::{
    anthropic::AnthropicProvider, openai::completion::OpenAiCompatibleAdapter,
  },
  gateway::{Gateway, GatewayConfig, RouteMap},
  ids::{ModelId, ProviderId},
  model::Model,
  retry::RetryPolicy,
};
use ra_tools::manager::ToolManager;
use serde::Deserialize;
use tokio::{
  io::{AsyncReadExt, AsyncWriteExt},
  net::TcpListener,
};

// ============================================================================
// 配置结构（精简自 provider/examples/gateway_fallback.rs）
// ============================================================================

/// `[[providers]]` 数组的单个条目：定义一个提供商适配器。
#[derive(Debug, Deserialize)]
struct ProviderEntry {
  /// 提供方 ID（用于注册与路由引用，例如 `"chat-primary"`）。
  provider_id: String,
  /// 适配器类型：`"openai-chat"` / `"openai-responses"` / `"anthropic"`。
  adapter: String,
  /// 基础端点（适配器自动拼接 `/chat/completions` / `/v1/messages`）。
  base_url: String,
  /// 鉴权密钥。
  api_key: String,
  /// 真实模型名：同时作为网关模型 ID 与请求线格式中的 `model` 字段。
  model: String,
}

/// 路由段：跨提供方降级链表。
#[derive(Debug, Deserialize, Default)]
struct Routing {
  /// 主提供方 ID → 候选提供方 ID 列表。
  #[serde(default)]
  cross_provider: HashMap<String, Vec<String>>,
}

/// 根配置。
#[derive(Debug, Deserialize)]
struct RealConfig {
  /// 网关查询入口模型名：必须是某个 provider 真实暴露的 `model`。
  query_model: String,
  /// 提供方列表（至少一个）。
  providers: Vec<ProviderEntry>,
  /// 可选的降级路由表。
  #[serde(default)]
  routing: Routing,
}

/// 解析示例用的提供方 ID。
fn pid(raw: &str) -> ProviderId {
  ProviderId::parse(raw).expect("valid provider id")
}

/// 解析示例用的模型 ID。
fn mid(raw: &str) -> ModelId {
  ModelId::parse(raw).expect("valid model id")
}

/// 构造满足网关注册校验的模型（上下文窗口与输出限额非零且合理）。
fn model(
  provider_id: &str,
  model_id: &str,
  display: &str,
  base_url: impl Into<String>,
) -> Model {
  Model::new(
    pid(provider_id),
    mid(model_id),
    display.to_string(),
    base_url.into(),
  )
  .with_context_window(8192)
  .with_max_tokens(1024)
}

/// 零退避、零重试的网关配置：示例只演示"单次建流失败触发降级"，
/// 不在重试环路上引入等待；真实部署可按需调整。
fn base_config() -> GatewayConfig {
  GatewayConfig {
    retry: RetryPolicy {
      initial_backoff: Duration::ZERO,
      max_retries: 0,
      jitter_ratio: 0.,
      ..RetryPolicy::default()
    },
    ..GatewayConfig::default()
  }
}

/// 尝试从候选路径读取并解析 `harness_fallback.toml`。
///
/// 文件不存在 / 解析失败 / 任一 provider 字段为空 / `query_model` 未被
/// 任何 provider 暴露 / 路由引用未注册的 provider_id → 返回 `None`
/// （回退离线桩）。
fn load_config() -> Option<RealConfig> {
  let candidates = [
    "crates/harness/examples/harness_fallback.toml",
    "examples/harness_fallback.toml",
    "harness_fallback.toml",
    // 模板文件（占位配置）：命中后字段不完整，自动回退离线桩
    "crates/harness/examples/harness_fallback_example.toml",
    "examples/harness_fallback_example.toml",
  ];
  let loaded = candidates.iter().find_map(|path| {
    std::fs::read_to_string(Path::new(path))
      .ok()
      .map(|t| (*path, t))
  });
  let Some((path, text)) = loaded else {
    tracing::warn!("no harness_fallback.toml found, using offline stubs");
    return None;
  };
  tracing::info!(path = %path, "config file loaded");
  let cfg: RealConfig = toml::from_str(&text)
    .inspect_err(|e| {
      tracing::warn!(error = %e, "config parse failed, using offline stubs");
    })
    .ok()?;

  // ===== 字段完整性校验：任一 provider 不完整 → 整组未配置 =====
  if cfg.providers.is_empty() {
    tracing::warn!("config has no providers, using offline stubs");
    return None;
  }
  for p in &cfg.providers {
    if p.provider_id.trim().is_empty()
      || p.adapter.trim().is_empty()
      || p.base_url.trim().is_empty()
      || p.api_key.trim().is_empty()
      || p.model.trim().is_empty()
    {
      tracing::warn!(
        provider = %p.provider_id,
        "provider entry incomplete, using offline stubs"
      );
      return None;
    }
    if !matches!(
      p.adapter.as_str(),
      "openai-chat" | "openai-responses" | "anthropic"
    ) {
      tracing::warn!(
        provider = %p.provider_id,
        adapter = %p.adapter,
        "unsupported adapter type, using offline stubs"
      );
      return None;
    }
    // 模板占位值检测：`*_example.toml` 的占位字段都非空，会绕过上面的
    // 完整性校验——显式识别占位模式，确保模板永远走离线桩模式。
    if p.api_key.starts_with("your-") || p.base_url.contains("example.com") {
      tracing::warn!(
        provider = %p.provider_id,
        "placeholder config detected, using offline stubs"
      );
      return None;
    }
  }
  if cfg.query_model.trim().is_empty()
    || !cfg.providers.iter().any(|p| p.model == cfg.query_model)
  {
    tracing::warn!(
      query_model = %cfg.query_model,
      "query_model is not provided by any provider, using offline stubs"
    );
    return None;
  }
  // 路由引用的 from / 候选 provider_id 必须都存在于 providers 列表。
  let ids: HashSet<&str> = cfg
    .providers
    .iter()
    .map(|p| p.provider_id.as_str())
    .collect();
  for (from, chain) in &cfg.routing.cross_provider {
    if !ids.contains(from.as_str())
      || chain.iter().any(|c| !ids.contains(c.as_str()))
    {
      tracing::warn!(
        from = %from,
        "routing references unregistered provider, using offline stubs"
      );
      return None;
    }
  }
  tracing::info!(
    providers = cfg.providers.len(),
    routes = cfg.routing.cross_provider.len(),
    query_model = %cfg.query_model,
    "config validated"
  );
  Some(cfg)
}

/// 按适配器类型动态构造 provider trait 对象。
fn build_adapter(
  entry: &ProviderEntry,
) -> Arc<dyn ra_provider::traits::Provider> {
  let adapter_model = model(
    &entry.provider_id,
    &entry.model,
    &entry.model,
    entry.base_url.clone(),
  );
  let api_key = Some(entry.api_key.clone());
  match entry.adapter.as_str() {
    "openai-chat" => Arc::new(OpenAiCompatibleAdapter::new(
      pid(&entry.provider_id),
      vec![adapter_model],
      reqwest::Client::new(),
      api_key,
    )) as Arc<dyn ra_provider::traits::Provider>,
    // openai-responses 适配器与 chat 同构，为控制示例体量不单独演示，
    // 配置里出现时按 openai-chat 兼容端点处理并提示。
    "openai-responses" | "anthropic" => Arc::new(AnthropicProvider::new(
      pid(&entry.provider_id),
      vec![adapter_model],
      reqwest::Client::new(),
      api_key,
    ))
      as Arc<dyn ra_provider::traits::Provider>,
    other => {
      unreachable!("未知 adapter 类型已在 load_config 阶段拦截：{other}")
    }
  }
}

// ============================================================================
// 会话容器组装（两模式共用）
// ============================================================================

/// 写入两个演示技能（目录型 + 扁平型，覆盖 SkillsLoader 的两种布局）。
fn write_skill_files(skills_dir: &Path) {
  let terminal_dir = skills_dir.join("terminal");
  std::fs::create_dir_all(&terminal_dir).expect("create terminal dir");
  std::fs::write(
    terminal_dir.join("SKILL.md"),
    "---\n\
     name: terminal\n\
     description: Execute shell commands safely\n\
     ---\n\
     Prefer simple, single-command invocations.",
  )
  .expect("write terminal skill");
  std::fs::write(
    skills_dir.join("note.md"),
    "---\n\
     name: note\n\
     description: Take structured notes during the session\n\
     ---\n\
     Always log key decisions in a structured format.",
  )
  .expect("write note skill");
}

/// 组装完整会话容器并执行一轮对话 + 模拟重启回放。
///
/// 与 [`harness_session`] 的差异：上下文装配用
/// [`HarnessContext`]（技能清单 + 系统提示 + 压缩接入）而非直通实现，
/// 展示 harness 在真实 provider 场景下的完整能力。
async fn run_session(
  gateway: Arc<Gateway>,
  model: Model,
  data_dir: &Path,
) -> RiboResult<()> {
  // ===== 阶段 1：技能加载 =====
  let skills_dir = data_dir
    .parent()
    .expect("data_dir has parent")
    .join("skills");
  write_skill_files(&skills_dir);
  let (skills, diagnostics) = SkillsLoader::new(vec![skills_dir]).load();
  tracing::info!(
    skills = skills.len(),
    diagnostics = diagnostics.len(),
    "skills loaded"
  );
  assert_eq!(skills.len(), 2, "terminal + note");

  // ===== 阶段 2：Harness 装配（HarnessContext + 持久化） =====
  let builder = SystemPromptBuilder::new()
    .with_template("You are a helpful assistant with fallback providers.");
  let context = HarnessContext::new(
    builder,
    Some(Arc::new(Compactor::new(gateway.clone(), model))),
    skills,
    ContextConfig {
      // v1 演示不触发压缩；需要演示时把 enabled 置 true 并配置
      // summarizer_model（走同一网关的降级链）。
      compaction: CompactionConfig::default(),
      include_environment: false,
    },
  );

  let store = Arc::new(JsonlSessionStore::new(data_dir.join("sessions")));
  let provider = Arc::new(PersistentSessionProvider::new(store.clone()));
  let deps = LoopDeps {
    gateway,
    model: mid("glm-4.5-flash"),
    tools: Arc::new(ToolManagerAdapter::new(Arc::new(ToolManager::new()))),
    context_provider: Arc::new(context),
  };
  let agent = BaseAgent::new(deps, provider.clone());
  let harness = Harness::new(agent, provider.clone(), store.clone());

  // ===== 阶段 3：一轮对话（离线模式经降级链到兜底桩；真实模式直达） =====
  let new_messages = harness
    .prompt("demo", "用一句话介绍 Rust 的所有权。")
    .await?;
  println!("回合完成：新增 {} 条消息；", new_messages.len());
  let transcript = harness.session_messages("demo").await?;
  assert_eq!(transcript.len(), 2, "user + assistant");
  // 真实端点不可达时 agent 会把建流错误收敛为 Error 终端消息（Ok 返回），
  // 这里把终态打印出来，便于区分「降级成功」与「端点失败」。
  if let Message::Assistant(assistant) = &transcript[1] {
    println!(
      "助手终态：stop_reason={:?}，文本片段={:?}",
      assistant.stop_reason,
      assistant
        .content
        .iter()
        .filter_map(|c| match c {
          ra_core::message::Content::Text(t) => {
            Some(t.content.chars().take(40).collect::<String>())
          }
          _ => None,
        })
        .collect::<Vec<_>>()
    );
  }

  // ===== 阶段 4：模拟重启，回放无重复 =====
  drop(harness);
  drop(provider);
  drop(store);
  tracing::info!("--- simulating process restart ---");
  let store2 = Arc::new(JsonlSessionStore::new(data_dir.join("sessions")));
  let provider2 = Arc::new(PersistentSessionProvider::new(store2.clone()));
  let agent2 = BaseAgent::new(
    LoopDeps {
      // 重启后只做回放读取，不发起 LLM 调用。
      gateway: Arc::new(Gateway::new(GatewayConfig::default())),
      model: mid("glm-4.5-flash"),
      tools: Arc::new(ToolManagerAdapter::new(Arc::new(ToolManager::new()))),
      context_provider: Arc::new(HarnessContext::new(
        SystemPromptBuilder::new(),
        None,
        Vec::new(),
        ContextConfig::default(),
      )),
    },
    provider2.clone(),
  );
  let harness2 = Harness::new(agent2, provider2, store2);
  let replay = harness2.session_messages("demo").await?;
  assert_eq!(replay.len(), 2, "replay matches, no duplicates");
  assert!(
    matches!(&replay[0], Message::User(_))
      && matches!(&replay[1], Message::Assistant(_)),
    "replay order: user first, assistant last"
  );
  println!("重启回放完成：{} 条消息，无重复。", replay.len());
  Ok(())
}

// ============================================================================
// 离线演示模式：主链返回可跨 provider 降级的错误，兜底桩返回成功流
// ============================================================================

/// 启动一次性本地 HTTP 服务：读取请求后返回 `response` 并关闭。
async fn serve_once(response: String) -> std::net::SocketAddr {
  let listener = TcpListener::bind("127.0.0.1:0")
    .await
    .expect("绑定本地端口");
  let addr = listener.local_addr().expect("读取本地地址");
  tracing::info!(addr = %addr, "offline stub listening");
  tokio::spawn(async move {
    let (mut socket, _) = listener.accept().await.expect("接受连接");
    let mut request = [0u8; 8192];
    let _ = socket.read(&mut request).await;
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.shutdown().await;
  });
  addr
}

/// 构造固定 Content-Length 的 HTTP 响应。
fn http_response(status_line: &str, body: &str) -> String {
  format!(
    "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
    body.len()
  )
}

/// 构造 OpenAI Chat Completions 兼容的 SSE 成功流（两段文本增量 + 末帧用量 + `[DONE]`）。
fn openai_chat_sse_body() -> String {
  let frames = [
    r#"{"id":"chatcmpl-1","choices":[{"index":0,"delta":{"role":"assistant","content":"来自 chat-backup 的降级响应：Rust 的所有权让内存安全无需垃圾回收。"}}]}"#,
    r#"{"id":"chatcmpl-1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":6,"completion_tokens":18,"total_tokens":24}}"#,
  ];
  let mut body = String::new();
  for frame in frames {
    body.push_str("data: ");
    body.push_str(frame);
    body.push_str("\n\n");
  }
  body.push_str("data: [DONE]\n\n");
  http_response("200 OK", &body)
}

/// 离线演示：主链（chat-primary）返回 429（RATE_LIMITED，可跨 provider 降级），
/// 兜底桩（chat-backup）返回 200 SSE 成功流；随后走同一会话容器流程。
async fn run_offline(data_dir: &Path) -> RiboResult<()> {
  println!("===== 离线演示模式（未找到有效配置文件）=====");
  let addr_429 = serve_once(http_response(
    "429 Too Many Requests",
    r#"{"error":{"type":"rate_limit_error","message":"demo primary throttled"}}"#,
  ))
  .await;
  let addr_ok = serve_once(openai_chat_sse_body()).await;

  let primary = model(
    "chat-primary",
    "glm-4.5-flash",
    "chat-primary",
    format!("http://{addr_429}"),
  );
  let backup = model(
    "chat-backup",
    "glm-4.5-flash",
    "chat-backup",
    format!("http://{addr_ok}"),
  );
  let compactor_model = model(
    "chat-primary",
    "glm-4.5-flash",
    "chat-primary",
    format!("http://{addr_ok}"),
  );

  let mut gateway = Gateway::new(base_config());
  gateway
    .register(Arc::new(OpenAiCompatibleAdapter::new(
      pid("chat-primary"),
      vec![primary],
      reqwest::Client::new(),
      Some("demo-key".to_string()),
    )))
    .expect("register primary");
  gateway
    .register(Arc::new(OpenAiCompatibleAdapter::new(
      pid("chat-backup"),
      vec![backup],
      reqwest::Client::new(),
      Some("demo-key".to_string()),
    )))
    .expect("register backup");
  let mut routes = RouteMap::default();
  routes.insert_cross_provider(pid("chat-primary"), vec![pid("chat-backup")]);
  let gateway = Arc::new(gateway.with_routes(routes));
  tracing::info!(
    route = "chat-primary -> chat-backup",
    "cross-provider fallback route registered"
  );

  run_session(gateway, compactor_model, data_dir).await?;
  Ok(())
}

/// 真实 API 模式：按配置注册适配器与路由后走同一会话容器流程。
async fn run_real(cfg: RealConfig, data_dir: &Path) -> RiboResult<()> {
  println!(
    "===== 真实 API 模式：已注册 {} 个 provider =====",
    cfg.providers.len()
  );
  let mut gateway = Gateway::new(base_config());
  for entry in &cfg.providers {
    let adapter = build_adapter(entry);
    gateway
      .register(adapter)
      .expect("配置文件里的 provider_id 应当通过注册校验");
    tracing::info!(
      provider = %entry.provider_id,
      adapter = %entry.adapter,
      model = %entry.model,
      "provider registered"
    );
  }
  let mut routes = RouteMap::default();
  for (from, chain) in &cfg.routing.cross_provider {
    routes
      .insert_cross_provider(pid(from), chain.iter().map(|c| pid(c)).collect());
    tracing::info!(from = %from, candidates = ?chain, "fallback route registered");
  }
  let gateway = Arc::new(gateway.with_routes(routes));

  // Compactor 的 context_window 估算基准：用主候选模型的合理默认值。
  let primary = cfg
    .providers
    .iter()
    .find(|p| p.model == cfg.query_model)
    .expect("已校验");
  let compactor_model = model(
    &primary.provider_id,
    &primary.model,
    &primary.model,
    primary.base_url.clone(),
  );
  run_session(gateway, compactor_model, data_dir).await?;
  Ok(())
}

/// 初始化 `tracing` 订阅者：示例默认输出 INFO 及以上级别的英文日志。
fn init_tracing() {
  let _ = tracing_subscriber::fmt().try_init();
}

#[tokio::main]
async fn main() -> RiboResult<()> {
  init_tracing();

  // 产物目录：workspace 的 tmp/harness_fallback/（与 test_util 的 tmp/ 约定一致，
  // 与运行 cargo run 时的 shell cwd 无关）。
  let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
    .ancestors()
    .nth(2)
    .expect("manifest has two ancestors")
    .join("tmp/harness_fallback");
  let _ = std::fs::remove_dir_all(&root);
  let data_dir = root.join("data");
  std::fs::create_dir_all(&data_dir).expect("create data dir");

  match load_config() {
    Some(cfg) => run_real(cfg, &data_dir).await?,
    None => run_offline(&data_dir).await?,
  }
  println!("EX-02 completed。产物目录：{}", root.display());
  Ok(())
}
