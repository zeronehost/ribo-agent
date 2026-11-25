//! 真实 API 集成测试（全部 `#[ignore]`，手动运行）。
//!
//! 需要 `#[ignore]` 的原因：打真实外部端点（AGENTS.md：需要外部服务的
//! 测试应标记 `#[ignore]`）。运行方式：
//!
//! ```text
//! ANTHROPIC_API_KEY=sk-... cargo test -p ra-provider --test real_api -- --ignored
//! OPENAI_API_KEY=sk-...    cargo test -p ra-provider --test real_api -- --ignored
//! ```
//!
//! 端点与模型可用环境变量覆盖（代理/网关场景）：
//! `ANTHROPIC_BASE_URL` / `OPENAI_BASE_URL` / `ANTHROPIC_MODEL` / `OPENAI_MODEL`。

use ra_core::{
  error::RiboResult,
  message::{Message, StopReason, ToolChoice, UserContent, UserMessage},
};
use ra_provider::{
  adapters::{
    anthropic::AnthropicProvider,
    openai::{
      completion::OpenAiCompatibleAdapter, responses::OpenAiResponsesAdapter,
    },
  },
  context::Context,
  ids::{ModelId, ProviderId},
  model::Model,
  options::{ProviderOptions, StreamOptions},
  stream::ProviderEvent,
  traits::Provider,
};

/// 读取环境变量，缺失时回退默认值。
fn env_or(key: &str, default: &str) -> String {
  std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// 构造真实调用用模型（窗口/限额非零，max_tokens 压小以省钱）。
fn real_model(provider: &str, id: &str, base_url: String) -> Model {
  Model::new(
    ProviderId::parse(provider).expect("提供方 ID 合法"),
    ModelId::parse(id).expect("模型 ID 合法"),
    id.to_string(),
    base_url,
  )
  .with_context_window(8192)
  .with_max_tokens(64)
}

/// 构造最小用户消息。
fn user_prompt(text: &str) -> Message {
  Message::User(UserMessage {
    content: UserContent::Text(text.to_string()),
    timestamp: 0,
  })
}

/// 驱动一次最小对话（"回复 ok"）并断言流以 Done(Stop) 收敛且文本非空。
///
/// # Errors
///
/// 建流失败（网络 / 鉴权 / 请求被拒）时透传错误。
async fn assert_real_stream_turn(
  adapter: &dyn Provider,
  model: &Model,
  label: &str,
) -> RiboResult<()> {
  let context = Context {
    system_prompt: Vec::new(),
    message: vec![user_prompt("Reply with exactly: ok")],
    tools: Vec::new(),
    tool_choice: ToolChoice::Auto,
  };
  let options = StreamOptions::default();

  let mut stream = adapter
    .stream(ProviderOptions {
      model,
      context: &context,
      options: &options,
    })
    .await?;

  let mut final_message = None;
  while let Some(event) = stream.next().await {
    if let ProviderEvent::Done { message, .. } = event {
      final_message = Some(message);
    }
  }
  let message =
    final_message.unwrap_or_else(|| panic!("{label} 流应以 Done 终结"));
  assert_eq!(message.stop_reason, StopReason::Stop, "{label} 应正常收敛");
  let text = message
    .content
    .iter()
    .find_map(|b| match b {
      ra_core::message::Content::Text(t) => Some(t.content.clone()),
      _ => None,
    })
    .unwrap_or_default();
  assert!(!text.trim().is_empty(), "{label} 应返回非空文本");
  Ok(())
}

// Anthropic 真实流

/// 场景：真实 Anthropic 端点最小对话。
/// 预期：Done(Stop) 且文本非空。缺密钥时跳过（打印提示，不失败）。
#[ignore]
#[tokio::test]
async fn int_01_anthropic_real_stream() {
  let Some(api_key) = std::env::var("ANTHROPIC_API_KEY").ok() else {
    eprintln!("跳过：未设置 ANTHROPIC_API_KEY");
    return;
  };
  let base_url = env_or("ANTHROPIC_BASE_URL", "https://api.anthropic.com");
  let model_name = env_or("ANTHROPIC_MODEL", "claude-3-5-haiku-latest");
  let model = real_model("anthropic", &model_name, base_url);

  let adapter = AnthropicProvider::new(
    ProviderId::parse("anthropic").expect("提供方 ID 合法"),
    vec![model.clone()],
    reqwest::Client::new(),
    Some(api_key),
  );
  assert_real_stream_turn(&adapter, &model, "INT-01 anthropic")
    .await
    .expect("INT-01 应通过");
}

// OpenAI Chat Completions 真实流

/// 场景：真实 OpenAI Chat Completions 端点最小对话。
/// 预期：Done(Stop) 且文本非空。缺密钥时跳过。
#[ignore]
#[tokio::test]
async fn int_02_openai_chat_real_stream() {
  let Some(api_key) = std::env::var("OPENAI_API_KEY").ok() else {
    eprintln!("跳过：未设置 OPENAI_API_KEY");
    return;
  };
  let base_url = env_or("OPENAI_BASE_URL", "https://api.openai.com/v1");
  let model_name = env_or("OPENAI_MODEL", "gpt-4o-mini");
  let model = real_model("openai", &model_name, base_url);

  let adapter = OpenAiCompatibleAdapter::new(
    ProviderId::parse("openai").expect("提供方 ID 合法"),
    vec![model.clone()],
    reqwest::Client::new(),
    Some(api_key),
  );
  assert_real_stream_turn(&adapter, &model, "INT-02 openai-chat")
    .await
    .expect("INT-02 应通过");
}

// OpenAI Responses 真实流

/// 场景：真实 OpenAI Responses 端点最小对话。
/// 预期：Done(Stop) 且文本非空。缺密钥时跳过。
#[ignore]
#[tokio::test]
async fn int_03_openai_responses_real_stream() {
  let Some(api_key) = std::env::var("OPENAI_API_KEY").ok() else {
    eprintln!("跳过：未设置 OPENAI_API_KEY");
    return;
  };
  let base_url = env_or("OPENAI_BASE_URL", "https://api.openai.com/v1");
  let model_name = env_or("OPENAI_MODEL", "gpt-4o-mini");
  let model = real_model("openai", &model_name, base_url);

  let adapter = OpenAiResponsesAdapter::new(
    ProviderId::parse("openai").expect("提供方 ID 合法"),
    vec![model.clone()],
    reqwest::Client::new(),
    Some(api_key),
  );
  assert_real_stream_turn(&adapter, &model, "INT-03 openai-responses")
    .await
    .expect("INT-03 应通过");
}
