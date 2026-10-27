//! Provider trait 与错误谓词扩展 trait。
//!
//! [`Provider`] 是所有模型提供方的统一抽象，适配器通过实现该 trait
//! 把各家 LLM 提供方的 API 线格式屏蔽在 crate 内部。
//!
//! [`ProviderErrorExt`] 是对 [`ra_core::error::RiboError`] 的本地扩展，
//! 提供重试、降级、熔断所需的分类谓词。具体实现见 [`crate::error`]。

use std::sync::Arc;

use ra_core::error::RiboResult;

use crate::{
  error::ErrorStage,
  ids::{ModelId, ProviderId},
  model::Model,
  options::ProviderOptions,
  stream::ProviderEventStream,
};

// ============================================================================
// 错误分类谓词扩展 trait
// ============================================================================
//
// 统一按 `(domain, code)` 二元组做 match，domain 固定为 `"provider"`
// （由 error/constants.rs 的 DOMAIN 常量定义）。阶段门短路规则：
// InStream 阶段一律不可重试/降级（除 circuit_trigger 外），
// 因为重试会与已产出的内容重复。

/// 错误分类谓词扩展 trait。
///
/// 实现于 [`RiboError`](ra_core::error::RiboError)，内部用 `(code, stage)`
/// 二元组做 match + 阶段门短路，保证跨阶段误用被运行期纠正。
pub trait ProviderErrorExt {
  /// 读取 `RiboError.context["stage"]` 反解出错误生命周期阶段。
  ///
  /// 无 stage context 时保守按 PreStream 处理（旧数据兼容）。
  fn stage(&self) -> ErrorStage;

  /// 是否可重试。
  ///
  /// InStream 阶段一律不可重试（重试会与已产出的内容重复）。
  /// 可重试的 code：`RATE_LIMITED` / `TIMEOUT` / `SERVER_ERROR` /
  /// `TRANSPORT` / `OVERLOADED`。
  fn is_retryable(&self) -> bool;

  /// 是否可降级（换同一 provider 的其他候选）。
  ///
  /// InStream 阶段一律降级为 false。
  fn is_degradable(&self) -> bool;

  /// 是否可跨 provider 降级（换另一家提供方）。
  ///
  /// 跨 provider 能解决：限流（配额耗尽）、鉴权、配额。
  /// ServerError / Timeout / Transport 等跨 provider 意义不大
  /// （如果是网络问题，哪家都挂）。
  fn is_cross_provider_degradable(&self) -> bool;

  /// 是否应触发熔断器。
  ///
  /// 不受阶段门影响——流内错误同样可能触发熔断
  /// （例如 SSE 连接反复中断说明上游不稳定）。
  fn is_circuit_trigger(&self) -> bool;
}

// ============================================================================
// Provider trait
// ============================================================================

/// 模型提供方统一抽象。
///
/// # 契约（实现方必须遵守）
///
/// 1. 建流阶段失败（HTTP 连接、认证、限流）必须返回 `Err(RiboError)`，
///    且错误的 `stage` 应为 PreStream；
/// 2. 流内阶段失败必须通过 [`crate::stream::ProviderEventStreamProducer::error`]
///    终结，不得让 `stream` 返回 `Err`；
/// 3. 实现内部不得自行重试，重试归属 `ra-retry` 层或网关；
/// 4. 必须实现 `Send + Sync`，适配到并发任务中。
#[async_trait::async_trait]
pub trait Provider: Send + Sync {
  /// 提供方标识，在同一注册表内唯一。
  fn id(&self) -> &ProviderId;
  /// 该提供方当前可用的模型列表。
  fn models(&self) -> &[Model];
  /// 判断本提供方是否支持指定模型。
  ///
  /// 默认实现按 [`Provider::models`] 列表线性查找，实现方可按需覆盖。
  fn supports(&self, model: &ModelId) -> bool {
    self.models().iter().any(|m| &m.id == model)
  }
  /// 提供方内部是否已自带重试。
  ///
  /// 默认为 `false`；自带重试的包装器应返回 `true`，
  /// 供上层避免叠加多层重试。
  fn is_retrying_provider(&self) -> bool {
    false
  }
  /// 发起一次流式请求，返回统一的事件流。
  ///
  /// # Errors
  ///
  /// 仅在建流阶段失败（HTTP 连接、认证、限流等）时返回 `Err`，
  /// 且错误阶段应为 PreStream；流内失败必须经
  /// [`crate::stream::ProviderEventStreamProducer::error`] 终结。
  async fn stream(
    &self,
    options: ProviderOptions<'_>,
  ) -> RiboResult<ProviderEventStream>;
}

/// 提供方注册表抽象。
///
/// 支持按 ID 精确查找提供方与模型，并支持跨全部已注册提供方检索模型，
/// 供网关层做路由与降级选型。
pub trait ProviderCatalog: Send + Sync {
  /// 按提供方 ID 查找提供方实例。
  fn provider(&self, id: &ProviderId) -> Option<Arc<dyn Provider>>;
  /// 在指定提供方下查找模型元信息。
  fn model(&self, id: &ProviderId, model: &ModelId) -> Option<Model>;
  /// 在已注册的全部提供方中查找指定模型，返回 `(提供方 ID, 模型)`。
  fn find_model(&self, model: &ModelId) -> Option<(ProviderId, Model)>;
  /// 列出当前已注册的全部提供方 ID。
  fn providers(&self) -> Vec<ProviderId>;
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{
    context::Context,
    error::{INVALID_REQUEST, pre_stream},
    options::StreamOptions,
  };

  /// 最小实现：只提供必填方法，其余依赖 trait 默认实现。
  struct DefaultsProvider {
    id: ProviderId,
    models: Vec<Model>,
  }

  #[async_trait::async_trait]
  impl Provider for DefaultsProvider {
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
      Err(pre_stream(INVALID_REQUEST, "not implemented"))
    }
  }

  /// 场景：默认 `supports` 按 `models` 列表线性查找。
  ///
  /// 预期：已注册模型返回 `true`，未注册模型返回 `false`。
  #[test]
  fn default_supports_matches_registered_models() {
    let provider_id = ProviderId::parse("p1").expect("测试提供方 ID 合法");
    let supported = ModelId::parse("m1").expect("测试模型 ID 合法");
    let missing = ModelId::parse("m2").expect("测试模型 ID 合法");
    let provider = DefaultsProvider {
      id: provider_id.clone(),
      models: vec![Model::new(
        provider_id,
        supported.clone(),
        "m1",
        "http://stub.local",
      )],
    };

    assert!(provider.supports(&supported));
    assert!(!provider.supports(&missing));
  }

  /// 场景：默认 `is_retrying_provider` 返回 `false`。
  #[test]
  fn default_is_retrying_provider_is_false() {
    let provider = DefaultsProvider {
      id: ProviderId::parse("p1").expect("测试提供方 ID 合法"),
      models: Vec::new(),
    };

    assert!(!provider.is_retrying_provider());
  }

  /// 场景：最小实现的必填方法（`id` 与 `stream`）。
  ///
  /// 预期：`id` 返回注册标识；`stream` 未实现时返回 INVALID_REQUEST。
  #[tokio::test]
  async fn required_provider_methods_are_exercised() {
    let provider = DefaultsProvider {
      id: ProviderId::parse("p1").expect("测试提供方 ID 合法"),
      models: Vec::new(),
    };
    let model = Model::new(
      ProviderId::parse("p1").expect("测试提供方 ID 合法"),
      ModelId::parse("m1").expect("测试模型 ID 合法"),
      "m1",
      "http://stub.local",
    );
    let context = Context::default();
    let options = StreamOptions::default();

    assert_eq!(provider.id().as_str(), "p1");
    let err = provider
      .stream(ProviderOptions {
        model: &model,
        context: &context,
        options: &options,
      })
      .await
      .err()
      .expect("未实现的建流应返回错误");
    assert_eq!(err.code(), INVALID_REQUEST);
  }
}
