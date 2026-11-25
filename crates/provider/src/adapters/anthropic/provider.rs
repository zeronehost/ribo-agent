//! AnthropicProvider：Provider trait 实现 + PreStream 鉴权 + HTTP 请求构建。
//!
//! 骨架：能正确处理鉴权、建流、SSE 解码启动。
//!
//! # HTTP 协议
//!
//! - 端点：`POST /v1/messages`，base_url 由 [`Model`](crate::model::Model) 提供。
//! - 鉴权：`x-api-key: {api_key}` + `anthropic-version: 2023-06-01`。
//! - 流式：`Accept: text/event-stream` + body 里 `stream: true`。

use ra_core::error::RiboResult;

use crate::{
  adapters::common::ProviderCore,
  ids::ProviderId,
  model::Model,
  options::ProviderOptions,
  stream::{ProviderEventStream, ProviderEventStreamProducer},
  traits::Provider,
};

/// Anthropic 提供商适配器。
///
/// 鉴权、模型目录与 HTTP 客户端由共享骨架
/// [`ProviderCore`](crate::adapters::common::ProviderCore) 持有；
/// 本适配器只额外记录 API 版本头。
pub struct AnthropicProvider {
  /// 适配器公共骨架：身份、模型目录、HTTP 客户端与鉴权密钥。
  core: ProviderCore,
  /// Anthropic API 版本（默认 `2023-06-01`）。
  api_version: &'static str,
}

impl AnthropicProvider {
  /// 构造 AnthropicProvider。
  ///
  /// http 客户端由调用方注入（允许共享连接池 / connect_timeout 预配置）。
  /// 缺失 api_key 时 PreStream 阶段拒绝 AUTH_FAILED。
  pub fn new(
    id: ProviderId,
    models: Vec<Model>,
    http: reqwest::Client,
    api_key: Option<String>,
  ) -> Self {
    Self {
      core: ProviderCore::new(id, models, http, api_key),
      api_version: "2023-06-01",
    }
  }
}

#[async_trait::async_trait]
impl Provider for AnthropicProvider {
  fn id(&self) -> &ProviderId {
    self.core.id()
  }

  fn models(&self) -> &[Model] {
    self.core.models()
  }

  async fn stream(
    &self,
    options: ProviderOptions<'_>,
  ) -> RiboResult<ProviderEventStream> {
    // 解构借用入参：引用生命周期绑定调用方栈帧，不跨任务保存。
    let ProviderOptions {
      model,
      context,
      options,
    } = options;

    // 本地校验（PreStream）
    // 模型归属 / 鉴权配置 / 采样参数由共享骨架统一校验。
    let api_key = self.core.validate(model, options, 2.0)?;

    // 构造请求（PreStream）
    // 端点取自模型快照；鉴权外附加头（模型 header + extra_headers）
    // 由 send_stream 统一追加。
    let body =
      crate::adapters::anthropic::build_params::build(model, context, options);
    let endpoint =
      format!("{}/v1/messages", model.base_url.trim_end_matches('/'));

    // 发送并等待响应头（受 cancel / timeout 约束）
    let response = self
      .core
      .send_stream(&endpoint, model, options, |req| {
        req
          .header("x-api-key", api_key)
          .header("anthropic-version", self.api_version)
          .header("content-type", "application/json")
          .header("accept", "text/event-stream")
          .json(&body)
      })
      .await?;

    // 建立事件通道（PreStream）
    let (producer, stream) = ProviderEventStreamProducer::build();

    // spawn 解析任务（InStream）
    crate::adapters::anthropic::sse::spawn_parse_task(
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
  use super::*;
  use crate::context::Context;
  use crate::error::INVALID_REQUEST;
  use crate::ids::{ModelId, ProviderId};
  use crate::model::Model;
  use crate::options::StreamOptions;
  use crate::test_support::init;

  fn make_model(id: &str) -> Model {
    Model::new(
      ProviderId::parse("anthropic").expect("valid"),
      ModelId::parse(id).expect("valid"),
      id,
      "https://api.anthropic.com",
    )
  }

  /// 场景：暴露 id 与 models，Provider trait 方法正确代理。
  #[test]
  fn exposes_identity_and_models() {
    init();
    let provider = AnthropicProvider::new(
      ProviderId::parse("anthropic").expect("valid"),
      vec![make_model("anthropic/claude-3-opus-20240229")],
      reqwest::Client::new(),
      Some("key".to_string()),
    );
    assert_eq!(provider.id().as_str(), "anthropic");
    assert_eq!(provider.models().len(), 1);
    assert!(provider.models()[0].id.as_str().contains("opus"));
  }

  /// 场景：缺失 api_key 时鉴权校验返回 AUTH_FAILED。
  #[test]
  fn rejects_missing_api_key() {
    init();
    let provider = AnthropicProvider::new(
      ProviderId::parse("anthropic").expect("valid"),
      vec![make_model("anthropic/claude-3-haiku-20240307")],
      reqwest::Client::new(),
      None,
    );
    let err = provider.core.api_key().expect_err("should fail");
    assert_eq!(err.code(), crate::error::AUTH_FAILED);
  }

  /// 场景：请求模型目录之外的模型。
  ///
  /// 预期：PreStream 阶段以 INVALID_REQUEST 拒绝，且不发出 HTTP 请求
  /// （回归防护：历史实现缺少 supports 校验）。
  #[tokio::test]
  async fn rejects_unknown_model() {
    init();
    let provider = AnthropicProvider::new(
      ProviderId::parse("anthropic").expect("valid"),
      vec![make_model("anthropic/claude-3-haiku-20240307")],
      reqwest::Client::new(),
      Some("key".to_string()),
    );
    let other = make_model("anthropic/claude-3-sonnet-20240229");
    let context = Context::default();
    let options = StreamOptions::default();

    let result = provider
      .stream(ProviderOptions {
        model: &other,
        context: &context,
        options: &options,
      })
      .await;
    let Err(err) = result else {
      panic!("expected pre-stream rejection");
    };
    assert_eq!(err.code(), INVALID_REQUEST);
  }

  /// 场景：温度超出合法区间 [0, 2]。
  ///
  /// 预期：PreStream 阶段以 INVALID_REQUEST 拒绝。
  #[tokio::test]
  async fn rejects_out_of_range_temperature() {
    init();
    let model = make_model("anthropic/claude-3-haiku-20240307");
    let provider = AnthropicProvider::new(
      ProviderId::parse("anthropic").expect("valid"),
      vec![make_model("anthropic/claude-3-haiku-20240307")],
      reqwest::Client::new(),
      Some("key".to_string()),
    );
    let context = Context::default();
    let mut options = StreamOptions::default();
    options.sampling.temperature = Some(3.0);

    let result = provider
      .stream(ProviderOptions {
        model: &model,
        context: &context,
        options: &options,
      })
      .await;
    let Err(err) = result else {
      panic!("expected pre-stream rejection");
    };
    assert_eq!(err.code(), INVALID_REQUEST);
  }
}
