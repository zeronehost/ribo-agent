//! 网关 + OpenAI 兼容适配器的流式请求示例。
//!
//! 支持两种模式：
//!
//! 1. **真实 API 模式**：在同目录的 `gateway_openai.toml` 中填写
//!    `base_url` / `api_key` / `model` 后运行，经网关请求任意
//!    OpenAI 兼容端点（OpenAI / DeepSeek / 各类聚合网关）：
//!
//!    ```text
//!    cargo run -p ra-provider --example gateway_openai
//!    ```
//!
//!    `base_url` 为基础地址，适配器在其后拼接
//!    `/chat/completions`（尾 `/` 自动剥离）。
//!
//! 2. **离线演示模式**（配置文件缺失或字段为空时回退）：本地起一个
//!    OpenAI 兼容 HTTP 服务桩，完全离线可运行。
//!
//! 日志：`main` 中初始化 `tracing` 订阅者（默认 INFO 级别），配置加载、
//! 提供方注册、网关建流与错误分支均输出英文日志；流的逐事件内容仍用
//! `println!` 面向读者展示。
//!
//! 演示要点：
//!
//! 1. 组装 [`OpenAiCompatibleAdapter`](ra_provider::adapters::openai::completion::OpenAiCompatibleAdapter)
//!    （真实端点或本地服务桩）；
//! 2. 注册进 [`Gateway`](ra_provider::gateway::Gateway)（注册期校验
//!    模型归属、上下文窗口与输出限额）；
//! 3. 网关建流成功后逐事件消费（`Start` → `TextDelta*` → `Done`），
//!    `result()` 取最终消息（文本 + 停止原因 + 用量）；
//! 4. 离线模式额外演示建流错误：服务桩返回 HTTP 429——适配器映射
//!    为 `RATE_LIMITED`，由网关上抛（消息附降级链摘要）。

use std::{net::SocketAddr, path::Path, sync::Arc, time::Duration};

use ra_core::message::{
  AssistantMessage, Content, Message, UserContent, UserMessage,
};
use ra_provider::{
  adapters::openai::completion::OpenAiCompatibleAdapter,
  context::Context,
  error::RATE_LIMITED,
  gateway::{Gateway, GatewayConfig},
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

/// 消费观察流：逐事件打印，最后用 `result()` 取终态。
///
/// 逐事件循环与 `result()` 共享同一终态（oneshot 通道独立兑现）。
async fn consume(mut stream: ObservedStream) {
  while let Some(event) = stream.next().await {
    match event {
      ProviderEvent::Start { partial } => {
        tracing::info!(
          provider = %partial.provider,
          model = %partial.model,
          "stream started"
        );
        println!(
          "[start] provider={} model={}",
          partial.provider, partial.model
        );
      }
      ProviderEvent::TextDelta { delta, .. } => {
        println!("[text-delta] {delta}");
      }
      ProviderEvent::Done { reason, message } => {
        tracing::info!(
          reason = ?reason,
          input = message.usage.input,
          output = message.usage.output,
          "stream done"
        );
        println!("[done] reason={reason:?}");
        println!("[done] 文本：{}", first_text(&message));
        println!(
          "[done] usage: input={} output={} total={}",
          message.usage.input, message.usage.output, message.usage.total_tokens
        );
      }
      ProviderEvent::Error { code, message, .. } => {
        tracing::warn!(code = ?code, "stream error event");
        println!(
          "[error] code={code:?} msg={}",
          message.error_message.as_deref().unwrap_or("(no message)")
        );
      }
      _ => {}
    }
  }
  let message = stream.result().await.expect("终态消息应可用");
  tracing::info!(stop_reason = ?message.stop_reason, "stream finalized");
  println!(
    "[result] stop_reason={:?} text={:?}",
    message.stop_reason,
    first_text(&message)
  );
}

// ============================================================================
// 真实 API 模式（配置文件驱动）
// ============================================================================

/// 真实 API 配置：从 `gateway_openai.toml` 读取。
///
/// 任一字段为空或文件不存在 → 示例自动回退离线服务桩演示。
#[derive(Debug, Deserialize)]
struct RealConfig {
  /// API 基础地址（适配器在其后拼接 `/chat/completions`）。
  base_url: String,
  /// Bearer 鉴权密钥。
  api_key: String,
  /// 模型名（请求线格式中的 `model` 字段）。
  model: String,
}

/// 尝试从候选路径读取并解析配置文件。
///
/// 候选顺序：
/// 1. `crates/provider/examples/gateway_openai.toml`（工作目录相对）；
/// 2. `examples/gateway_openai.toml`（Cargo 在 workspace 根运行时生效）。
///
/// 文件不存在 / 解析失败 / 字段为空串 → 返回 `None`（回退离线桩）。
fn load_config() -> Option<RealConfig> {
  let candidates = [
    "crates/provider/examples/gateway_openai.toml",
    "examples/gateway_openai.toml",
    "gateway_openai.toml",
    // 模板文件（占位配置）：命中后字段不完整，自动回退离线服务桩
    "crates/provider/examples/gateway_openai_example.toml",
    "examples/gateway_openai_example.toml",
  ];
  let loaded = candidates.iter().find_map(|path| {
    std::fs::read_to_string(Path::new(path))
      .ok()
      .map(|text| (*path, text))
  });
  let Some((path, text)) = loaded else {
    tracing::warn!("no gateway_openai.toml found, using offline stubs");
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
  // 空串与纯空白视同未配置。
  if cfg.base_url.trim().is_empty()
    || cfg.api_key.trim().is_empty()
    || cfg.model.trim().is_empty()
  {
    tracing::warn!("config fields incomplete, using offline stubs");
    return None;
  }
  tracing::info!(base_url = %cfg.base_url, model = %cfg.model, "config validated");
  Some(cfg)
}

/// 真实 API 模式：经网关向 OpenAI 兼容端点发起一次流式请求。
async fn run_real(cfg: RealConfig) {
  println!(
    "===== 真实 API 模式：base={} model={} =====",
    cfg.base_url, cfg.model
  );
  tracing::info!(
    base_url = %cfg.base_url,
    model = %cfg.model,
    "real api mode"
  );

  // ===== 组装模型与适配器 =====
  let model_id = ModelId::parse(&cfg.model).expect("模型名非空即合法");
  let model = Model::new(
    ProviderId::parse("openai").expect("提供方 ID 合法"),
    model_id.clone(),
    cfg.model.clone(),
    cfg.base_url.clone(),
  )
  .with_context_window(8192)
  .with_max_tokens(1024);
  let adapter = OpenAiCompatibleAdapter::new(
    ProviderId::parse("openai").expect("提供方 ID 合法"),
    vec![model],
    reqwest::Client::new(),
    Some(cfg.api_key),
  );

  // ===== 注册进网关 =====
  tracing::info!(provider = "openai", model = %cfg.model, "registering provider");
  let mut gateway = Gateway::new(GatewayConfig::default());
  gateway
    .register(Arc::new(adapter))
    .expect("适配器应通过注册校验");
  tracing::info!(provider = "openai", "provider registered");

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
  tracing::info!(model = %model_id.as_str(), "gateway stream requested");
  match gateway
    .stream(&model_id, &ctx, StreamOptions::default())
    .await
  {
    Ok(stream) => consume(stream).await,
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
// 离线演示模式（本地服务桩）
// ============================================================================

/// 构造网关注册要求的模型（上下文窗口与输出限额非零且合理）。
fn local_model(base_url: String) -> Model {
  Model::new(
    ProviderId::parse("openai").expect("提供方 ID 合法"),
    ModelId::parse("gpt-local").expect("模型 ID 合法"),
    "Local GPT",
    base_url,
  )
  .with_context_window(8192)
  .with_max_tokens(1024)
}

/// 构造指向 `addr` 的 OpenAI 兼容适配器（Bearer 密钥仅进入请求头）。
fn adapter(addr: SocketAddr) -> OpenAiCompatibleAdapter {
  OpenAiCompatibleAdapter::new(
    ProviderId::parse("openai").expect("提供方 ID 合法"),
    vec![local_model(format!("http://{addr}"))],
    reqwest::Client::new(),
    Some("demo-key".to_string()),
  )
}

/// 启动一次性本地 HTTP 服务：读取请求后返回 `response` 并关闭。
/// 先读取请求（头 + 小 JSON 体）再写响应，避免未读数据导致
/// Windows 上的连接重置。
async fn serve_once(response: String) -> SocketAddr {
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

/// 构造 OpenAI 兼容的 SSE 成功响应：两段文本增量 + 末帧用量 + `[DONE]`。
fn sse_success_body() -> String {
  let frames = [
    r#"{"id":"chatcmpl-1","choices":[{"index":0,"delta":{"role":"assistant","content":"你好，"}}]}"#,
    r#"{"id":"chatcmpl-1","choices":[{"index":0,"delta":{"content":"Rust！"}}]}"#,
    r#"{"id":"chatcmpl-1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":3,"total_tokens":8}}"#,
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

/// 零退避、零重试的网关配置：示例路径不引入真实等待。
fn zero_retry_config() -> GatewayConfig {
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

/// 离线演示模式：本地服务桩驱动「SSE 成功流」与「HTTP 429 建流错误」。
async fn run_offline() {
  println!("gateway_openai.toml 缺失或字段不完整，回退到离线服务桩演示。");
  tracing::info!("offline demo mode: using local stubs");
  let ctx = Context::default();

  // ===== 场景 1：正常 SSE 流 =====
  // 服务桩返回两段文本增量 + [DONE]；网关建流成功后由消费端逐事件读取。
  println!("\n===== 场景 1：网关建流成功（SSE 增量）=====");
  let addr = serve_once(sse_success_body()).await;
  tracing::info!(
    addr = %addr,
    scenario = "sse-success",
    "offline stub ready"
  );
  let mut gateway = Gateway::new(zero_retry_config());
  gateway
    .register(Arc::new(adapter(addr)))
    .expect("适配器应通过注册校验");
  tracing::info!(model = "gpt-local", "gateway stream requested");
  let stream = gateway
    .stream(
      &ModelId::parse("gpt-local").expect("模型 ID 合法"),
      &ctx,
      StreamOptions::default(),
    )
    .await
    .expect("网关应建流成功");
  consume(stream).await;

  // ===== 场景 2：建流错误（HTTP 429 → RATE_LIMITED）=====
  // 服务桩返回 429；适配器映射为 RATE_LIMITED（可降级），网关无降级
  // 路由可用，重试耗尽后上抛最后一次错误并附降级链摘要。
  println!("\n===== 场景 2：建流错误（HTTP 429）=====");
  let err_body =
    r#"{"error":{"message":"insufficient quota","type":"insufficient_quota"}}"#;
  let addr = serve_once(http_response("429 Too Many Requests", err_body)).await;
  tracing::info!(
    addr = %addr,
    scenario = "http-429",
    "offline stub ready"
  );
  let mut failing_gateway = Gateway::new(zero_retry_config());
  failing_gateway
    .register(Arc::new(adapter(addr)))
    .expect("适配器应通过注册校验");
  tracing::warn!(model = "gpt-local", "gateway stream expected to fail");

  // ObservedStream 未实现 Debug，先映射为 () 再用 expect_err。
  let err = failing_gateway
    .stream(
      &ModelId::parse("gpt-local").expect("模型 ID 合法"),
      &ctx,
      StreamOptions::default(),
    )
    .await
    .map(|_| ())
    .expect_err("429 建流应失败");
  assert_eq!(err.code(), RATE_LIMITED);
  tracing::warn!(
    code = %err.code(),
    error = %err.message(),
    "build stream failed as expected"
  );
  println!("[场景2] code={}", err.code());
  println!("[场景2] message={}", err.message());
}

/// 初始化 `tracing` 订阅者：示例默认输出 INFO 及以上级别的英文日志。
fn init_tracing() {
  tracing_subscriber::fmt()
    .with_max_level(tracing::Level::INFO)
    .init();
}

#[tokio::main]
async fn main() {
  init_tracing();
  tracing::info!("gateway_openai example starting");
  match load_config() {
    Some(cfg) => run_real(cfg).await,
    None => run_offline().await,
  }
  tracing::info!("gateway_openai example finished");
}
