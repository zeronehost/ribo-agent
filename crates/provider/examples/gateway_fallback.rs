//! 真实提供商适配器的多候选降级示例（不使用 MockProvider）。
//!
//! 演示要点：
//!
//! 1. 组装三家真实适配器：
//!    - `chat-primary` → [`OpenAiCompatibleAdapter`](ra_provider::adapters::openai::completion::OpenAiCompatibleAdapter)
//!      （默认 OpenAI Chat Completions 端点）；
//!    - `responses-primary` → [`OpenAiResponsesAdapter`](ra_provider::adapters::openai::responses::OpenAiResponsesAdapter)
//!      （OpenAI Responses API）；
//!    - `anthropic-primary` → [`AnthropicProvider`](ra_provider::adapters::anthropic::AnthropicProvider)
//!      （Anthropic Messages API）。
//!
//!    每家适配器经 `models` 数组注册自己的**真实模型**（同时作为请求线
//!    `model` 字段）；网关查询入口由 `[routing].default` 二元组指定。
//!    降级规则写在 `[[routing.fallback]]` 列表（声明顺序即优先级）。
//! 2. 真实 API 模式：从 `gateway_fallback.toml` 读取 provider 列表与路由；
//!    任一 provider 的 `base_url` / `api_key` / `models` 缺失、`default`
//!    二元组悬空或路由引用无效，示例自动回退离线桩。
//! 3. 离线演示模式：三个本地 HTTP 桩依次返回 429（RATE_LIMITED）、402（QUOTA_EXCEEDED）、401（AUTH_FAILED）——
//!    这三种错误码都属于"可跨提供方降级"——网关沿跨链逐个尝试直到终极兜底桩返回 200 SSE 成功流。
//! 4. 运行路径不依赖 MockProvider，所有降级都由真实适配器的建流错误驱动。
//!
//! 日志：`main` 中初始化 `tracing` 订阅者（默认 INFO 级别），配置加载、
//! provider 注册、跨提供方路由、网关建流与错误分支均输出英文日志；
//! 流的逐事件内容仍用 `println!` 面向读者展示。
//!
//! # 运行
//!
//! ```text
//! cargo run -p ra-provider --example gateway_fallback
//! ```
//!
//! 真实 API 模式配置文件示例（`crates/provider/examples/gateway_fallback.toml`）：
//!
//! ```text,ignore
//! [routing]
//! default = { provider = "chat-primary", model = "glm-4.5-flash" }
//!
//! [[providers]]
//! provider_id  = "chat-primary"
//! adapter     = "openai-chat"
//! base_url    = "https://open.bigmodel.cn/api/paas/v4"
//! api_key     = "sk-..."
//!
//!   [[providers.models]]
//!   id = "glm-4.5-flash"
//!
//! [[routing.fallback]]
//! match = { provider = "chat-primary" }
//! targets = [{ provider = "responses-primary" }, { provider = "anthropic-primary" }]
//! ```

use std::{path::Path, sync::Arc, time::Duration};

use ra_core::message::{
  AssistantMessage, Content, Message, UserContent, UserMessage,
};
use ra_provider::{
  adapters::{
    anthropic::AnthropicProvider,
    openai::{
      completion::OpenAiCompatibleAdapter, responses::OpenAiResponsesAdapter,
    },
  },
  context::Context,
  gateway::{
    FallbackMatch, FallbackRule, FallbackTarget, Gateway, GatewayConfig,
    RouteMap,
  },
  ids::{ModelId, ProviderId},
  model::Model,
  options::StreamOptions,
  retry::RetryPolicy,
  stream::{ObservedStream, ProviderEvent},
};
use serde::Deserialize;
use tokio::{
  io::{AsyncReadExt, AsyncWriteExt},
  net::TcpListener,
};

// ============================================================================
// 示例辅助
// ============================================================================

/// 解析示例用的提供方 ID。
fn pid(raw: &str) -> ProviderId {
  ProviderId::parse(raw).expect("示例提供方 ID 合法")
}

/// 解析示例用的模型 ID。
fn mid(raw: &str) -> ModelId {
  ModelId::parse(raw).expect("示例模型 ID 合法")
}

/// 构造满足网关注册校验的模型（上下文窗口与输出限额非零且合理）。
fn model(
  provider_raw: &str,
  id: &str,
  display: &str,
  base_url: String,
) -> Model {
  Model::new(pid(provider_raw), mid(id), display, base_url)
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

/// 取消息中的第一段文本（示例辅助）。
fn first_text(message: &AssistantMessage) -> String {
  message
    .content
    .iter()
    .find_map(|c| match c {
      Content::Text(t) => Some(t.content.clone()),
      _ => None,
    })
    .unwrap_or_default()
}

/// 消费观察流：逐事件打印后取终态消息。
async fn consume(mut stream: ObservedStream, tag: &str) {
  while let Some(event) = stream.next().await {
    match event {
      ProviderEvent::Start { partial } => {
        tracing::info!(
          tag = %tag,
          provider = %partial.provider,
          model = %partial.model,
          "stream started"
        );
        println!(
          "[{tag}] start: provider={} model={}",
          partial.provider, partial.model
        );
      }
      ProviderEvent::TextDelta { delta, .. } => {
        println!("[{tag}] text-delta: {delta}");
      }
      ProviderEvent::Done { reason, .. } => {
        tracing::info!(tag = %tag, reason = ?reason, "stream done");
        println!("[{tag}] done: reason={reason:?}");
      }
      ProviderEvent::Error { code, message, .. } => {
        tracing::warn!(tag = %tag, code = ?code, "stream error event");
        println!(
          "[{tag}] error: code={code:?} msg={}",
          message.error_message.as_deref().unwrap_or("(no message)")
        );
      }
      _ => {}
    }
  }
  let message = stream.result().await.expect("终态消息应可用");
  tracing::info!(
    tag = %tag,
    stop_reason = ?message.stop_reason,
    "stream finalized"
  );
  println!(
    "[{tag}] final: stop_reason={:?} text={:?}",
    message.stop_reason,
    first_text(&message)
  );
}

/// 示例查询上下文（真实 API 与离线桩共用）。
fn query_context() -> Context {
  Context {
    message: vec![Message::User(UserMessage {
      content: UserContent::Text(
        "用一句话介绍 Rust 的所有权机制。".to_string(),
      ),
      timestamp: 0,
    })],
    ..Context::default()
  }
}

// ============================================================================
// 真实 API 模式（配置文件驱动）
// ============================================================================

/// `[[providers]]` 数组的单个条目：定义一个提供商适配器。
#[derive(Debug, Deserialize)]
struct ProviderEntry {
  /// 提供方 ID（用于注册与路由引用，例如 `"chat-primary"`）。
  provider_id: String,
  /// 适配器类型：`"openai-chat"` / `"openai-responses"` / `"anthropic"`。
  adapter: String,
  /// 基础端点（适配器自动拼接 `/chat/completions` / `/responses` / `/v1/messages`）。
  base_url: String,
  /// 鉴权密钥。
  api_key: String,
  /// 该通道可用的模型清单（声明顺序即备选序）。
  models: Vec<ModelEntry>,
}

/// `[[providers.models]]` 单条目。
#[derive(Debug, Deserialize)]
struct ModelEntry {
  /// 真实模型名：同时作为网关模型 ID 与请求线格式中的 `model` 字段。
  id: String,
}

/// `[[routing.fallback]]` 单条规则。
#[derive(Debug, Deserialize)]
struct FallbackRuleCfg {
  /// 匹配条件（`model` 缺省 = provider 级通配）。
  #[serde(rename = "match")]
  match_rule: MatchCfg,
  /// 有序目标列表。
  targets: Vec<TargetCfg>,
}

#[derive(Debug, Deserialize)]
struct MatchCfg {
  provider: String,
  #[serde(default)]
  model: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TargetCfg {
  provider: String,
  #[serde(default)]
  model: Option<String>,
}

/// routing 段：默认二元组 + 降级规则列表。
#[derive(Debug, Deserialize, Default)]
struct Routing {
  /// 默认模型二元组（缺省时回退 mock-model）。
  #[serde(default)]
  default: Option<ModelRefCfg>,
  /// 降级规则（声明顺序 = 优先级）。
  #[serde(default)]
  fallback: Vec<FallbackRuleCfg>,
}

#[derive(Debug, Deserialize)]
struct ModelRefCfg {
  provider: String,
  model: String,
}

/// 根配置。
#[derive(Debug, Deserialize)]
struct RealConfig {
  /// 提供方列表（至少一个）。
  providers: Vec<ProviderEntry>,
  /// routing 段（默认二元组 + 降级规则）。
  #[serde(default)]
  routing: Routing,
}

/// 尝试从候选路径读取并解析 `gateway_fallback.toml`。
///
/// 候选顺序同 `gateway_openai`；文件不存在 / 解析失败 / 任一 provider
/// 字段为空 / 路由引用了未注册的 provider_id → 返回 `None`（回退离线桩）。
fn load_config() -> Option<RealConfig> {
  let candidates = [
    "crates/provider/examples/gateway_fallback.toml",
    "examples/gateway_fallback.toml",
    "gateway_fallback.toml",
    // 模板文件（占位配置）：命中后字段不完整，自动回退离线桩
    "crates/provider/examples/gateway_fallback_example.toml",
    "examples/gateway_fallback_example.toml",
  ];
  let loaded = candidates.iter().find_map(|path| {
    std::fs::read_to_string(Path::new(path))
      .ok()
      .map(|text| (*path, text))
  });
  let Some((path, text)) = loaded else {
    tracing::warn!("no gateway_fallback.toml found, using offline stubs");
    return None;
  };
  tracing::info!(path = %path, "config file loaded");
  let cfg: RealConfig = toml::from_str(&text)
    .inspect_err(|e| {
      tracing::warn!(
        error = %e,
        "config parse failed, falling back to offline stubs"
      );
    })
    .ok()?;
  // 字段完整性校验：任一 provider 不完整 → 整组未配置（避免带病运行）。
  if cfg.providers.is_empty() {
    tracing::warn!("config has no providers, using offline stubs");
    return None;
  }
  for p in &cfg.providers {
    if p.provider_id.trim().is_empty()
      || p.adapter.trim().is_empty()
      || p.base_url.trim().is_empty()
      || p.api_key.trim().is_empty()
      || p.models.is_empty()
      || p.models.iter().any(|m| m.id.trim().is_empty())
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
  }
  // 默认二元组必须指向某个 provider 暴露的模型。
  let Some(default) = &cfg.routing.default else {
    tracing::warn!("[routing].default missing, using offline stubs");
    return None;
  };
  if !cfg.providers.iter().any(|p| {
    p.provider_id == default.provider
      && p.models.iter().any(|m| m.id == default.model)
  }) {
    tracing::warn!(
      provider = %default.provider,
      model = %default.model,
      "default model not provided by any provider, using offline stubs"
    );
    return None;
  }
  // 路由引用的 from / 候选 provider_id 必须都存在于 providers 列表。
  let ids: std::collections::HashSet<&str> = cfg
    .providers
    .iter()
    .map(|p| p.provider_id.as_str())
    .collect();
  for rule in &cfg.routing.fallback {
    if !ids.contains(rule.match_rule.provider.as_str()) {
      tracing::warn!(
        "fallback rule match provider not registered, using offline stubs"
      );
      return None;
    }
    if rule.match_rule.model.as_ref().is_some_and(|m| {
      !cfg.providers.iter().any(|p| {
        p.provider_id == rule.match_rule.provider
          && p.models.iter().any(|x| x.id == *m)
      })
    }) {
      tracing::warn!(
        "fallback rule match model not registered, using offline stubs"
      );
      return None;
    }
    for target in &rule.targets {
      if !ids.contains(target.provider.as_str()) {
        tracing::warn!(
          "fallback target provider not registered, using offline stubs"
        );
        return None;
      }
      if target.model.as_ref().is_some_and(|m| {
        !cfg.providers.iter().any(|p| {
          p.provider_id == target.provider
            && p.models.iter().any(|x| x.id == *m)
        })
      }) {
        tracing::warn!(
          "fallback target model not registered, using offline stubs"
        );
        return None;
      }
    }
  }
  tracing::info!(
    providers = cfg.providers.len(),
    rules = cfg.routing.fallback.len(),
    "config validated"
  );
  Some(cfg)
}

/// 按适配器类型动态构造 provider trait 对象（Arc<dyn Provider>）。
fn build_adapter(
  entry: &ProviderEntry,
) -> Arc<dyn ra_provider::traits::Provider> {
  let provider_id = pid(&entry.provider_id);
  let base_url = entry.base_url.clone();
  let api_key = entry.api_key.clone();
  // 真实模型名注册：既是网关目录模型 ID，也是请求线 `model` 字段。
  let models: Vec<Model> = entry
    .models
    .iter()
    .map(|m| {
      Model::new(
        provider_id.clone(),
        mid(&m.id),
        m.id.clone(),
        base_url.clone(),
      )
      .with_context_window(8192)
      .with_max_tokens(1024)
    })
    .collect();

  match entry.adapter.as_str() {
    "openai-chat" => Arc::new(OpenAiCompatibleAdapter::new(
      provider_id,
      models,
      reqwest::Client::new(),
      Some(api_key),
    )) as Arc<dyn ra_provider::traits::Provider>,
    "openai-responses" => Arc::new(OpenAiResponsesAdapter::new(
      provider_id,
      models,
      reqwest::Client::new(),
      Some(api_key),
    )) as Arc<dyn ra_provider::traits::Provider>,
    "anthropic" => Arc::new(AnthropicProvider::new(
      provider_id,
      models,
      reqwest::Client::new(),
      Some(api_key),
    )) as Arc<dyn ra_provider::traits::Provider>,
    other => {
      unreachable!("未知 adapter 类型已在 load_config 阶段拦截：{other}")
    }
  }
}

/// 从 TOML 配置组装网关并发起一次请求。
async fn run_real(cfg: RealConfig) {
  println!(
    "===== 真实 API 模式：已注册 {} 个 provider =====",
    cfg.providers.len()
  );
  for p in &cfg.providers {
    println!(
      "  - {} ({}) → {}",
      p.provider_id,
      p.adapter,
      p.models
        .iter()
        .map(|m| m.id.as_str())
        .collect::<Vec<_>>()
        .join(", ")
    );
  }
  tracing::info!(providers = cfg.providers.len(), "real api mode");

  // ===== 注册适配器 =====
  let mut gateway = Gateway::new(base_config());
  for entry in &cfg.providers {
    let adapter = build_adapter(entry);
    gateway
      .register(adapter)
      .expect("配置文件里的 provider_id 应当通过注册校验");
    tracing::info!(
      provider = %entry.provider_id,
      adapter = %entry.adapter,
      "provider registered"
    );
  }

  // ===== 跨 provider 降级路由（统一 fallback 规则）=====
  let mut routes = RouteMap::default();
  let rules: Vec<FallbackRule> = cfg
    .routing
    .fallback
    .iter()
    .map(|r| FallbackRule {
      match_: FallbackMatch {
        provider: pid(&r.match_rule.provider),
        model: r.match_rule.model.as_ref().map(|m| mid(m)),
      },
      targets: r
        .targets
        .iter()
        .map(|t| match &t.model {
          Some(m) => FallbackTarget::Model {
            provider: pid(&t.provider),
            model: mid(m),
          },
          None => FallbackTarget::Provider(pid(&t.provider)),
        })
        .collect(),
    })
    .collect();
  routes.insert_fallback_rules(rules);
  tracing::info!("fallback rules registered");
  gateway = gateway.with_routes(routes);

  // ===== 构造上下文并发起请求 =====
  let ctx = Context {
    message: vec![Message::User(UserMessage {
      content: UserContent::Text(
        "用一句话介绍 Rust 的所有权机制。".to_string(),
      ),
      timestamp: 0,
    })],
    ..Context::default()
  };
  // 网关查询入口取配置里的 default 二元组（load_config 已校验）。
  let default = cfg.routing.default.as_ref().expect("default 已校验");
  let target = mid(&default.model);
  tracing::info!(
    model = %target.as_str(),
    provider = %default.provider,
    "gateway stream requested"
  );
  match gateway
    .stream(&target, &ctx, StreamOptions::default())
    .await
  {
    Ok(stream) => consume(stream, "real").await,
    Err(e) => {
      tracing::warn!(
        code = %e.code(),
        error = %e.message(),
        "gateway stream failed"
      );
      println!("[建流失败] code={} message={}", e.code(), e.message());
    }
  }
}

// ============================================================================
// 离线演示模式：三个本地桩依次返回可跨 provider 降级的错误
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

/// 构造一个"返回 HTTP 错误"的桩（错误码 + 错误体 → OpenAI / Anthropic 兼容错误体）。
///
/// 错误码映射必须是**可跨 provider 降级**的（RATE_LIMITED / QUOTA_EXCEEDED / AUTH_FAILED），
/// 否则 Gateway 不会继续沿跨链走。
async fn serve_error(status_line: &str, body: &str) -> std::net::SocketAddr {
  serve_once(http_response(status_line, body)).await
}

/// 构造 OpenAI Chat Completions 兼容的 SSE 成功流（两段文本增量 + 末帧用量 + `[DONE]`）。
fn openai_chat_sse_body() -> String {
  let frames = [
    r#"{"id":"chatcmpl-1","choices":[{"index":0,"delta":{"role":"assistant","content":"来自 chat-backup-success 的降级响应："}}]}"#,
    r#"{"id":"chatcmpl-1","choices":[{"index":0,"delta":{"content":"三家真实适配器全挂了才轮到我兜底。"}}]}"#,
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

/// 构造指向本地桩的适配器（provider_id / 模型展示名按示例命名）。
fn chat_adapter_for(addr: std::net::SocketAddr) -> OpenAiCompatibleAdapter {
  OpenAiCompatibleAdapter::new(
    pid("chat-primary"),
    vec![model(
      "chat-primary",
      "glm-4.5-flash",
      "chat-primary",
      format!("http://{addr}"),
    )],
    reqwest::Client::new(),
    Some("demo-key".to_string()),
  )
}
fn responses_adapter_for(addr: std::net::SocketAddr) -> OpenAiResponsesAdapter {
  OpenAiResponsesAdapter::new(
    pid("responses-primary"),
    vec![model(
      "responses-primary",
      "glm-4.5-flash",
      "responses-primary",
      format!("http://{addr}"),
    )],
    reqwest::Client::new(),
    Some("demo-key".to_string()),
  )
}
fn anthropic_adapter_for(addr: std::net::SocketAddr) -> AnthropicProvider {
  AnthropicProvider::new(
    pid("anthropic-primary"),
    vec![model(
      "anthropic-primary",
      "glm-4.5-flash",
      "anthropic-primary",
      format!("http://{addr}"),
    )],
    reqwest::Client::new(),
    Some("demo-key".to_string()),
  )
}
fn chat_backup_adapter_for(
  addr: std::net::SocketAddr,
) -> OpenAiCompatibleAdapter {
  OpenAiCompatibleAdapter::new(
    pid("chat-backup-success"),
    vec![model(
      "chat-backup-success",
      "glm-4.5-flash",
      "chat-backup-success",
      format!("http://{addr}"),
    )],
    reqwest::Client::new(),
    Some("demo-key".to_string()),
  )
}

/// 离线演示：三个真实适配器依次返回可跨 provider 降级的错误，
/// 终极兜底桩返回成功的 SSE 流。
///
/// 错误体统一采用 OpenAI 风格的顶层 `type` / `message`，
/// OpenAI Chat / Responses 与 Anthropic 的建流阶段 HTTP 错误解析都会命中
/// from_http 通用映射（code=RATE_LIMITED / QUOTA_EXCEEDED / AUTH_FAILED）。
async fn run_offline() {
  println!("gateway_fallback.toml 缺失或字段不完整，回退到离线服务桩演示。");
  tracing::info!("offline demo mode: using local stubs");

  // ===== 1. 第一桩：429 限流 → 映射 RATE_LIMITED（可跨链）=====
  let addr_1 = serve_error(
    "429 Too Many Requests",
    r#"{"message":"insufficient_quota","type":"insufficient_quota"}"#,
  )
  .await;
  tracing::info!(
    provider = "chat-primary",
    status = 429,
    code = "RATE_LIMITED",
    addr = %addr_1,
    "offline stub ready"
  );
  // ===== 2. 第二桩：402 配额用尽 → 映射 QUOTA_EXCEEDED（可跨链）=====
  let addr_2 = serve_error(
    "402 Payment Required",
    r#"{"message":"billing exhausted","type":"insufficient_quota"}"#,
  )
  .await;
  tracing::info!(
    provider = "responses-primary",
    status = 402,
    code = "QUOTA_EXCEEDED",
    addr = %addr_2,
    "offline stub ready"
  );
  // ===== 3. 第三桩：401 鉴权失败 → 映射 AUTH_FAILED（可跨链）=====
  let addr_3 = serve_error(
    "401 Unauthorized",
    r#"{"message":"invalid api key","type":"authentication_error"}"#,
  )
  .await;
  tracing::info!(
    provider = "anthropic-primary",
    status = 401,
    code = "AUTH_FAILED",
    addr = %addr_3,
    "offline stub ready"
  );
  // ===== 4. 终极兜底桩：200 SSE 成功流 =====
  let addr_4 = serve_once(openai_chat_sse_body()).await;
  tracing::info!(
    provider = "chat-backup-success",
    status = 200,
    addr = %addr_4,
    "offline stub ready"
  );

  // ===== 组装三家真实适配器 + 一个兜底 =====
  let chat = Arc::new(chat_adapter_for(addr_1));
  let responses = Arc::new(responses_adapter_for(addr_2));
  let anthropic = Arc::new(anthropic_adapter_for(addr_3));
  let backup = Arc::new(chat_backup_adapter_for(addr_4));

  // ===== 降级路由：统一 fallback 规则列表=====
  // 每条规则按声明顺序匹配；targets 按序尝试（{ provider } 形态 =
  // 同名 → 目标默认两级解析）
  let mut routes = RouteMap::default();
  routes.insert_fallback_rules(vec![
    FallbackRule {
      match_: FallbackMatch {
        provider: pid("chat-primary"),
        model: Some(mid("chat-model")),
      },
      targets: vec![
        FallbackTarget::Provider(pid("responses-primary")),
        FallbackTarget::Provider(pid("anthropic-primary")),
        FallbackTarget::Provider(pid("chat-backup-success")),
      ],
    },
    FallbackRule {
      match_: FallbackMatch {
        provider: pid("responses-primary"),
        model: Some(mid("responses-model")),
      },
      targets: vec![
        FallbackTarget::Provider(pid("anthropic-primary")),
        FallbackTarget::Provider(pid("chat-backup-success")),
      ],
    },
    FallbackRule {
      match_: FallbackMatch {
        provider: pid("anthropic-primary"),
        model: Some(mid("anthropic-model")),
      },
      targets: vec![FallbackTarget::Provider(pid("chat-backup-success"))],
    },
  ]);

  let mut gateway = Gateway::new(base_config()).with_routes(routes);
  gateway.register(chat).expect("chat-primary 应注册成功");
  gateway
    .register(responses)
    .expect("responses-primary 应注册成功");
  gateway
    .register(anthropic)
    .expect("anthropic-primary 应注册成功");
  gateway
    .register(backup)
    .expect("chat-backup-success 应注册成功");

  // ===== 发起一次请求，沿链触发 4 次尝试 =====
  let ctx = query_context();
  tracing::info!(query_model = "glm-4.5-flash", "gateway stream requested");
  let stream = gateway
    .stream(&mid("glm-4.5-flash"), &ctx, StreamOptions::default())
    .await
    .expect("三家真实适配器全挂后应降级到 chat-backup-success 成功");
  consume(stream, "offline-fallback").await;
}

/// 初始化 `tracing` 订阅者：示例默认输出 INFO 及以上级别的英文日志。
fn init_tracing() {
  tracing_subscriber::fmt()
    .with_timer(
      tracing_subscriber::fmt::time::OffsetTime::local_rfc_3339()
        .unwrap_or_else(|_| {
          tracing_subscriber::fmt::time::OffsetTime::new(
            time::UtcOffset::UTC,
            time::format_description::well_known::Rfc3339,
          )
        }),
    )
    .with_max_level(tracing::Level::INFO)
    .init();
}

#[tokio::main]
async fn main() {
  init_tracing();
  tracing::info!("gateway_fallback example starting");
  match load_config() {
    Some(cfg) => run_real(cfg).await,
    None => run_offline().await,
  }
  tracing::info!("gateway_fallback example finished");
}
