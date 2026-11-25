//! 提供方注册前的合法性校验。

use std::collections::HashSet;

use ra_core::error::RiboResult;

use crate::{
  error::{INVALID_REQUEST, pre_stream},
  ids::ModelId,
  model::Model,
  traits::Provider,
};

/// 校验提供方的模型目录是否满足注册要求。
///
/// 校验规则（任一失败即拒绝注册）：
///
/// 1. 至少暴露一个模型；
/// 2. 每个模型的 `provider` 字段与注册 ID 一致；
/// 3. `context_window` 与 `max_tokens` 均非零，且
///    `max_tokens` 不得超过 `context_window`；
/// 4. 模型 ID 不得重复。
///
/// # Errors
///
/// 违反任一规则时返回 `INVALID_REQUEST`，context 携带违规的 `model` 键。
pub fn validate_provider(provider: &dyn Provider) -> RiboResult<()> {
  let models: &[Model] = provider.models();

  // 规则 1：模型列表非空
  if models.is_empty() {
    tracing::warn!(
      provider = %provider.id(),
      "provider validation failed: no models"
    );
    return Err(pre_stream(
      INVALID_REQUEST,
      format!("provider {} exposes no models", provider.id()),
    ));
  }
  let mut seen: HashSet<&ModelId> = HashSet::with_capacity(models.len());
  for m in models {
    // 规则 2：模型归属与注册 ID 一致
    if &m.provider != provider.id() {
      tracing::warn!(
        provider = %provider.id(),
        model = %m.id,
        "provider validation failed: model provider mismatch"
      );
      return Err(
        pre_stream(
          INVALID_REQUEST,
          format!(
            "model {} declares provider {}, but registered under {}",
            m.id,
            m.provider,
            provider.id()
          ),
        )
        .with_context("model", m.id.to_string()),
      );
    }
    // 规则 3：上下文窗口与输出限额合理
    if m.context_window == 0 || m.max_tokens == 0 {
      tracing::warn!(
        provider = %provider.id(),
        model = %m.id,
        "provider validation failed: zero window/output limit"
      );
      return Err(
        pre_stream(
          INVALID_REQUEST,
          format!("model {} has zero context_window or max_tokens", m.id),
        )
        .with_context("model", m.id.to_string()),
      );
    }
    if m.max_tokens > m.context_window {
      tracing::warn!(
        provider = %provider.id(),
        model = %m.id,
        "provider validation failed: output limit exceeds window"
      );
      return Err(
        pre_stream(
          INVALID_REQUEST,
          format!(
            "model {} max_tokens ({}) exceeds context_window ({})",
            m.id, m.max_tokens, m.context_window
          ),
        )
        .with_context("model", m.id.to_string()),
      );
    }
    // 规则 4：模型 ID 不重复
    if !seen.insert(&m.id) {
      tracing::warn!(
        provider = %provider.id(),
        model = %m.id,
        "provider validation failed: duplicate model id"
      );
      return Err(
        pre_stream(
          INVALID_REQUEST,
          format!(
            "provider {} declares duplicate model id {}",
            provider.id(),
            m.id
          ),
        )
        .with_context("model", m.id.to_string()),
      );
    }
  }
  tracing::debug!(
    provider = %provider.id(),
    models = models.len(),
    "provider validation passed"
  );
  Ok(())
}

#[cfg(test)]
mod tests {
  use ra_core::error::RiboResult;

  use super::*;
  use crate::{
    context::Context,
    error::INVALID_REQUEST,
    ids::ProviderId,
    options::{ProviderOptions, StreamOptions},
    stream::ProviderEventStream,
  };

  /// 仅用于校验用例的测试提供方。
  struct ValidationStub {
    id: ProviderId,
    models: Vec<Model>,
  }

  #[async_trait::async_trait]
  impl Provider for ValidationStub {
    fn id(&self) -> &ProviderId {
      &self.id
    }
    fn models(&self) -> &[Model] {
      &self.models
    }
    async fn stream(
      &self,
      _options: ProviderOptions<'_>,
    ) -> RiboResult<ProviderEventStream> {
      Err(pre_stream(INVALID_REQUEST, "validation stub 不应发起建流"))
    }
  }

  fn provider(raw: &str, models: Vec<Model>) -> ValidationStub {
    crate::test_support::init();
    ValidationStub {
      id: ProviderId::parse(raw).expect("测试提供方 ID 合法"),
      models,
    }
  }

  fn model(provider_raw: &str, id: &str) -> Model {
    Model::new(
      ProviderId::parse(provider_raw).expect("测试提供方 ID 合法"),
      ModelId::parse(id).expect("测试模型 ID 合法"),
      id,
      "http://stub.local",
    )
    .with_context_window(8192)
    .with_max_tokens(1024)
  }

  /// 场景：目录完全合法的提供方。
  ///
  /// 预期：校验通过。
  #[test]
  fn accepts_valid_provider() {
    let stub = provider("p1", vec![model("p1", "m1")]);
    assert!(validate_provider(&stub).is_ok());
  }

  /// 场景：提供方未暴露任何模型。
  ///
  /// 预期：拒绝注册（INVALID_REQUEST）。
  #[test]
  fn rejects_provider_without_models() {
    let stub = provider("p1", vec![]);
    let err = validate_provider(&stub).expect_err("空模型列表应被拒绝");
    assert_eq!(err.code(), INVALID_REQUEST);
  }

  /// 场景：模型声明的 provider 与注册 ID 不一致。
  ///
  /// 预期：拒绝注册（INVALID_REQUEST）。
  #[test]
  fn rejects_model_provider_mismatch() {
    let stub = provider("p1", vec![model("p2", "m1")]);
    let err = validate_provider(&stub).expect_err("归属不一致应被拒绝");
    assert_eq!(err.code(), INVALID_REQUEST);
  }

  /// 场景：上下文窗口或输出限额为零。
  ///
  /// 预期：拒绝注册（INVALID_REQUEST）。
  #[test]
  fn rejects_zero_window_or_output_limit() {
    let zero_window = Model::new(
      ProviderId::parse("p1").unwrap(),
      ModelId::parse("m1").unwrap(),
      "m1",
      "http://stub.local",
    )
    .with_max_tokens(1024);
    let err = validate_provider(&provider("p1", vec![zero_window]))
      .expect_err("零窗口应被拒绝");
    assert_eq!(err.code(), INVALID_REQUEST);

    let zero_output = Model::new(
      ProviderId::parse("p1").unwrap(),
      ModelId::parse("m1").unwrap(),
      "m1",
      "http://stub.local",
    )
    .with_context_window(8192);
    let err = validate_provider(&provider("p1", vec![zero_output]))
      .expect_err("零输出限额应被拒绝");
    assert_eq!(err.code(), INVALID_REQUEST);
  }

  /// 场景：输出限额超过上下文窗口。
  ///
  /// 预期：拒绝注册（INVALID_REQUEST）。
  #[test]
  fn rejects_output_limit_exceeding_window() {
    let stub = provider(
      "p1",
      vec![
        Model::new(
          ProviderId::parse("p1").unwrap(),
          ModelId::parse("m1").unwrap(),
          "m1",
          "http://stub.local",
        )
        .with_context_window(100)
        .with_max_tokens(101),
      ],
    );
    let err = validate_provider(&stub).expect_err("输出超过窗口应被拒绝");
    assert_eq!(err.code(), INVALID_REQUEST);
  }

  /// 场景：模型 ID 在同一提供方内重复。
  ///
  /// 预期：拒绝注册（INVALID_REQUEST）。
  #[test]
  fn rejects_duplicate_model_ids() {
    let stub = provider("p1", vec![model("p1", "m1"), model("p1", "m1")]);
    let err = validate_provider(&stub).expect_err("重复模型 ID 应被拒绝");
    assert_eq!(err.code(), INVALID_REQUEST);
  }

  /// 场景：测试桩的建流方法。
  ///
  /// 预期：返回 INVALID_REQUEST，便于覆盖测试桩自身的函数。
  #[tokio::test]
  async fn validation_stub_stream_returns_error() {
    let stub = provider("p1", vec![model("p1", "m1")]);
    let model = model("p1", "m1");
    let context = Context::default();
    let options = StreamOptions::default();

    let err = stub
      .stream(ProviderOptions {
        model: &model,
        context: &context,
        options: &options,
      })
      .await
      .err()
      .expect("测试桩不应发起建流");

    assert_eq!(err.code(), INVALID_REQUEST);
  }
}
