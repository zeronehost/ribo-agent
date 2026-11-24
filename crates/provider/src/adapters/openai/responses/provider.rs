//! OpenAI Responses API（`POST /responses`）适配器。

use ra_core::error::RiboResult;

use super::{build_params, sse};
use crate::{
  adapters::common::ProviderCore,
  ids::ProviderId,
  model::Model,
  options::ProviderOptions,
  stream::{ProviderEventStream, ProviderEventStreamProducer},
  traits::Provider,
};

/// OpenAI Responses API 适配器。
///
/// 与 [`crate::adapters::openai::completion::OpenAiCompatibleAdapter`] 并列：
/// 共用同一个 Bearer 鉴权与模型目录形态，但端点、请求体与事件线格式
/// 完全不同（见 `build_params` / `sse` 模块文档）。
pub struct OpenAiResponsesAdapter {
  /// 适配器公共骨架：身份、模型目录、HTTP 客户端与鉴权密钥。
  core: ProviderCore,
}

impl OpenAiResponsesAdapter {
  /// 构造 Responses API 适配器。
  ///
  /// # 参数
  ///
  /// - `id`：提供方标识（小写 ASCII，如 `openai`）。
  /// - `models`：模型目录快照；`stream` 按 `Model.id` 做归属校验。
  /// - `http`：共享 HTTP 客户端；调用方只应设置 `connect_timeout`。
  /// - `api_key`：Bearer 鉴权密钥；缺失时 `stream` 以 `AUTH_FAILED` 拒绝。
  ///
  /// # 示例
  ///
  /// ```no_run
  /// // 此示例需要外部 HTTP 服务，因此只编译不运行
  /// use ra_provider::adapters::openai::responses::OpenAiResponsesAdapter;
  /// use ra_provider::ids::ProviderId;
  /// use ra_provider::traits::Provider;
  ///
  /// let adapter = OpenAiResponsesAdapter::new(
  ///   ProviderId::parse("openai").expect("valid provider id"),
  ///   Vec::new(), // 模型目录由部署配置注入
  ///   reqwest::Client::new(),
  ///   Some("sk-...".to_string()),
  /// );
  /// assert!(!adapter.is_retrying_provider());
  /// ```
  pub fn new(
    id: ProviderId,
    models: Vec<Model>,
    http: reqwest::Client,
    api_key: Option<String>,
  ) -> Self {
    Self {
      core: ProviderCore::new(id, models, http, api_key),
    }
  }
}

#[async_trait::async_trait]
impl Provider for OpenAiResponsesAdapter {
  fn id(&self) -> &ProviderId {
    self.core.id()
  }

  fn models(&self) -> &[Model] {
    self.core.models()
  }

  // is_retrying_provider：裸适配器不带重试，使用 trait 默认实现（false）。
  async fn stream(
    &self,
    options: ProviderOptions<'_>,
  ) -> RiboResult<ProviderEventStream> {
    let ProviderOptions {
      model,
      context,
      options,
    } = options;

    // ===== 阶段 1：本地校验（PreStream）=====
    // 模型归属 / 鉴权配置 / 采样参数（Responses 与 Chat 温度范围一致）。
    let api_key = self.core.validate(model, options, 2.0)?;

    // ===== 阶段 2：构造请求（PreStream）=====
    let endpoint =
      format!("{}/responses", model.base_url.trim_end_matches('/'));
    let body = build_params::build(model, context, options);

    // ===== 阶段 3：发送并等待响应头（PreStream，受 timeout 约束）=====
    let response = self
      .core
      .send_stream(&endpoint, model, options, |req| {
        req.bearer_auth(api_key).json(&body)
      })
      .await?;

    tracing::debug!(
      provider = self.core.id().as_str(),
      model = model.id.as_str(),
      status = response.status().as_u16(),
      "response headers received"
    );

    // ===== 阶段 4：建立事件通道（PreStream）=====
    let (producer, stream) = ProviderEventStreamProducer::build();

    // ===== 阶段 5：spawn 解析任务（InStream）=====
    sse::spawn_parse_task(
      response,
      producer,
      options.cancel.clone().unwrap_or_default(),
      self.core.id().clone(),
      model.id.clone(),
    );

    Ok(stream)
  }
}

#[cfg(test)]
mod tests {
  use tokio::io::{AsyncReadExt, AsyncWriteExt};
  use tokio::net::TcpListener;

  use super::*;
  use crate::{
    context::Context,
    error::{AUTH_FAILED, INVALID_REQUEST},
    ids::ModelId,
    options::{ProviderOptions, StreamOptions},
    stream::ProviderEvent,
  };

  fn model_id(raw: &str) -> ModelId {
    ModelId::parse(raw).expect("测试模型 ID 合法")
  }

  fn test_model(base_url: String) -> Model {
    Model::new(
      ProviderId::parse("openai").expect("测试提供方 ID 合法"),
      model_id("gpt-4o"),
      "GPT-4o",
      base_url,
    )
    .with_max_tokens(1024)
  }

  /// 场景：本地一次性服务按 Responses 线格式返回文本流。
  ///
  /// 预期：解析出 TextStart/TextDelta，并以 Done 携带完整文本终结。
  #[tokio::test]
  async fn streams_text_from_responses_sse() {
    crate::test_support::init();
    let body = concat!(
      "event: response.created\n",
      "data: {\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-4o\"}}\n\n",
      "event: response.output_text.delta\n",
      "data: {\"delta\":\"hi\"}\n\n",
      "event: response.completed\n",
      "data: {\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-4o\",",
      "\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\n",
    )
    .to_string();

    let listener = TcpListener::bind("127.0.0.1:0")
      .await
      .expect("绑定本地端口");
    let addr = listener.local_addr().expect("读取本地地址");
    let server = tokio::spawn(async move {
      let (mut socket, _) = listener.accept().await.expect("接受连接");
      let mut request = [0u8; 8192];
      let _ = socket.read(&mut request).await;
      let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
      );
      let _ = socket.write_all(response.as_bytes()).await;
      let _ = socket.shutdown().await;
    });

    let model = test_model(format!("http://{addr}"));
    let adapter = OpenAiResponsesAdapter::new(
      ProviderId::parse("openai").expect("测试提供方 ID 合法"),
      vec![model.clone()],
      reqwest::Client::new(),
      Some("test-key".to_string()),
    );

    let context = Context::default();
    let options = StreamOptions::default();
    let mut stream = adapter
      .stream(ProviderOptions {
        model: &model,
        context: &context,
        options: &options,
      })
      .await
      .expect("建流成功");

    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
      events.push(event);
    }
    server.await.expect("服务任务结束");

    assert!(events.iter().any(|e| matches!(
      e,
      ProviderEvent::TextDelta { delta, .. } if delta == "hi"
    )));
    match events.last().expect("终端事件") {
      ProviderEvent::Done { reason, message } => {
        assert_eq!(*reason, ra_core::message::StopReason::Stop);
        assert_eq!(message.response_id.as_deref(), Some("resp_1"));
        assert_eq!(message.usage.input, 3);
        assert_eq!(message.usage.output, 1);
        assert!(matches!(
          message.content.first(),
          Some(ra_core::message::Content::Text(text)) if text.content == "hi"
        ));
      }
      other => panic!("expected Done, got {other:?}"),
    }
  }

  /// 场景：未配置密钥即发起调用。
  ///
  /// 预期：本地校验阶段以 AUTH_FAILED 拒绝，不发出网络请求。
  #[tokio::test]
  async fn rejects_missing_api_key() {
    crate::test_support::init();
    let adapter = OpenAiResponsesAdapter::new(
      ProviderId::parse("openai").expect("测试提供方 ID 合法"),
      vec![test_model("http://127.0.0.1:1".to_string())],
      reqwest::Client::new(),
      None,
    );
    let model = &adapter.models()[0];
    let context = Context::default();
    let options = StreamOptions::default();

    // ProviderEventStream 未实现 Debug，不能用 expect_err
    let Err(err) = adapter
      .stream(ProviderOptions {
        model,
        context: &context,
        options: &options,
      })
      .await
    else {
      panic!("缺失密钥应失败");
    };
    assert_eq!(err.code(), AUTH_FAILED);
  }

  /// 场景：调用目录外的模型。
  ///
  /// 预期：本地校验阶段以 INVALID_REQUEST 拒绝。
  #[tokio::test]
  async fn rejects_unknown_model() {
    crate::test_support::init();
    let adapter = OpenAiResponsesAdapter::new(
      ProviderId::parse("openai").expect("测试提供方 ID 合法"),
      vec![test_model("http://127.0.0.1:1".to_string())],
      reqwest::Client::new(),
      Some("test-key".to_string()),
    );
    let other = test_model("http://127.0.0.1:1".to_string());
    let other = Model {
      id: model_id("other"),
      ..other
    };
    let context = Context::default();
    let options = StreamOptions::default();

    let Err(err) = adapter
      .stream(ProviderOptions {
        model: &other,
        context: &context,
        options: &options,
      })
      .await
    else {
      panic!("未知模型应失败");
    };
    assert_eq!(err.code(), INVALID_REQUEST);
  }
}
