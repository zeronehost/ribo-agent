//! 多提供方网关：统一入口，负责路由、熔断、重试与降级编排。
//!
//! 由以下子模块组成：
//!
//! - `router`：提供方注册目录（[`ProviderCatalog`] 的内存实现）；
//! - `fallback`：降级路由表（显式映射 / 同提供方链 / 跨提供方链）；
//! - `option`：网关静态配置；
//! - `utils`：注册前的提供方合法性校验。
//!
//! 每提供方熔断器复用 [`ra_core::circuit`] 的无锁三态实现
//! （Closed / Open / HalfOpen），与工具管理器（`ra-tools::manager`）
//! 共用同一实现。
//!
//! 请求路径：信号量准入 → 目录解析主候选 → 熔断准入 → 建流
//! （带超时与重试）→ 可降级时沿候选链切换 → 成功后以
//! [`ObservedStream`] 观察流的终结并回报熔断。

mod fallback;
mod option;
mod router;
mod utils;

// `GatewayConfig` / `RouteMap` 是 `Gateway` 公开方法（`new` / `with_routes`）
// 的参数类型，定义在私有子模块中；必须在此再导出，否则 crate 外部无法命名。
pub use self::{fallback::RouteMap, option::GatewayConfig};

use std::sync::{Arc, RwLock};

use dashmap::DashMap;
use ra_core::{
  circuit::{Admission, CircuitBreaker},
  error::{RiboError, RiboResult},
};
use tokio::sync::Semaphore;

use crate::{
  context::Context,
  error::{
    CANCELLED, CIRCUIT_OPEN, DUPLICATE_PROVIDER, MODEL_NOT_FOUND, OVERLOADED,
    RETRY_PROVIDER_REFUSED, TIMEOUT, pre_stream,
  },
  ids::{ModelId, ProviderId},
  options::{ProviderOptions, StreamOptions},
  retry::{RetryDecision, sleep_backoff},
  stream::ObservedStream,
  traits::{Provider, ProviderCatalog, ProviderErrorExt},
};

use self::{
  fallback::Candidate, router::CatalogIndex, utils::validate_provider,
};

/// 多提供方网关：统一入口，负责路由、熔断、重试与降级。
///
/// # 职责
///
/// - 注册期校验提供方合法性，拒绝重复注册与自带重试的提供方；
/// - 按模型解析主提供方，可降级失败时沿候选链逐个尝试；
/// - 每个提供方独立熔断，熔断打开的候选直接跳过；
/// - 全局并发上限（信号量）与建流超时保护。
///
/// 建流成功后返回 [`ObservedStream`]，由它在流终结时回报熔断器
/// 并释放在途配额。
pub struct Gateway {
  /// 网关静态配置。
  config: GatewayConfig,
  /// 提供方注册目录（读多写少，用读写锁保护）。
  catalog: RwLock<CatalogIndex>,
  /// 每提供方的熔断器，首次使用时按配置惰性创建。
  breakers: DashMap<ProviderId, Arc<CircuitBreaker>>,
  /// 降级路由表。
  route: RouteMap,
  /// 全局在途请求配额；permit 由 `ObservedStream` 持有至流终结。
  semaphore: Arc<Semaphore>,
}

impl Gateway {
  /// 按配置创建网关；降级路由表为空，可用 [`Gateway::with_routes`] 注入。
  pub fn new(config: GatewayConfig) -> Self {
    let semaphore = Arc::new(Semaphore::new(config.max_in_flight));
    Self {
      config,
      catalog: RwLock::new(CatalogIndex::default()),
      breakers: DashMap::new(),
      route: RouteMap::default(),
      semaphore,
    }
  }

  /// 注入降级路由表（builder 风格，消费并返回自身）。
  ///
  /// 路由表决定可降级错误发生时的候选顺序：显式模型映射优先，
  /// 其次同提供方备选链，跨提供方链仅在错误可跨提供方降级时启用。
  pub fn with_routes(mut self, route: RouteMap) -> Self {
    self.route = route;
    self
  }

  /// 取指定提供方的熔断器，首次访问时按配置惰性创建。
  fn breaker_for(&self, provider: &ProviderId) -> Arc<CircuitBreaker> {
    self
      .breakers
      .entry(provider.clone())
      .or_insert_with(|| {
        Arc::new(CircuitBreaker::new(
          self.config.circuit_threshold,
          self.config.circuit_cooldown,
        ))
      })
      .clone()
  }

  /// 注册一个提供方实例。
  ///
  /// 注册前先做合法性校验（见 `validate_provider`）；
  /// 自带重试的提供方与重复 ID 均被拒绝——网关自身已负责重试，
  /// 叠加重试会导致请求放大与内容重复。
  ///
  /// # Errors
  ///
  /// - 校验失败 → `INVALID_REQUEST`；
  /// - 提供方自带重试 → `RETRY_PROVIDER_REFUSED`；
  /// - ID 重复 → `DUPLICATE_PROVIDER`。
  pub fn register(&mut self, provider: Arc<dyn Provider>) -> RiboResult<()> {
    validate_provider(provider.as_ref())?;

    if provider.is_retrying_provider() {
      tracing::warn!(
        provider = %provider.id(),
        "retrying provider refused: gateway must not stack retries"
      );
      return Err(pre_stream(
        RETRY_PROVIDER_REFUSED,
        format!(
          "provider '{}' has built-in retries and cannot be registered behind the gateway",
          provider.id()
        ),
      ));
    }
    let mut catalog = self.catalog.write().expect("catalog lock poisoned");
    if catalog.contains(provider.id()) {
      tracing::warn!(provider = %provider.id(), "duplicate provider registration refused");
      return Err(pre_stream(
        DUPLICATE_PROVIDER,
        format!("provider '{}' is already registered", provider.id()),
      ));
    }
    tracing::info!(
      provider = %provider.id(),
      models = provider.models().len(),
      "provider registered"
    );
    catalog.insert(provider);
    Ok(())
  }

  /// 按模型发起流式请求，失败时按策略重试并沿降级链切换候选。
  ///
  /// 处理流程：
  ///
  /// 1. 信号量准入（超限即 `OVERLOADED`），并按配置合并默认超时；
  /// 2. 在目录中解析主提供方与模型；
  /// 3. 逐候选尝试：熔断打开的候选直接跳过；候选内按
  ///    `effective_retry` 对建流失败（含建流超时）做重试；
  /// 4. 可降级错误惰性解析 fallback 候选后继续；致命错误短路返回；
  /// 5. 建流成功后包成 [`ObservedStream`] 返回，permit 由流持有至终结。
  ///
  /// # Errors
  ///
  /// - 候选链耗尽 → 返回最后一次错误（附 fallback 链摘要）；
  /// - 全部候选被熔断拒绝 → `CIRCUIT_OPEN`；
  /// - 等待期被取消 → `CANCELLED`；
  /// - 信号量超限 → `OVERLOADED`；模型未注册 → `MODEL_NOT_FOUND`。
  pub async fn stream(
    &self,
    model: &ModelId,
    context: &Context,
    options: StreamOptions,
  ) -> RiboResult<ObservedStream> {
    // ===== 并发准入 =====
    // 超限直接拒绝；permit 持有至流终结，由 ObservedStream 释放。
    let permit = self.semaphore.clone().try_acquire_owned().map_err(|e| {
      tracing::warn!(model = %model, "gateway overloaded: max in-flight reached");
      tracing::debug!(model = %model, "gateway overloaded: max in-flight reached ({e})");
      pre_stream(OVERLOADED, "gateway max in-flight reached")
    })?;

    // ===== 主候选解析 =====
    // 读锁内一次性取出 (提供方 ID, 模型, 提供方实例)。
    let (primary_id, primary_model) = {
      let catalog = self.catalog.read().expect("catalog lock poisoned");
      let (pid, m) = catalog.find_model(model).ok_or_else(|| {
        tracing::warn!(model = %model, "model not found in gateway catalog");
        pre_stream(
          MODEL_NOT_FOUND,
          format!("model '{model}' is not registered"),
        )
      })?;
      (pid, m)
    };
    // ===== 合并默认超时与生效重试策略 =====
    let effective_options = {
      let mut o = options.clone();
      if o.timeout.is_none() {
        o.timeout = Some(self.config.default_timeout);
      }
      o
    };
    // 上方合并块保证 timeout 恒为 Some（None 时已填入 default_timeout），
    // 直接取出即可，无需再次回退到配置默认值。
    let build_timeout = effective_options.timeout.unwrap();
    let effective_retry = self.config.retry.effective(&effective_options);
    let cancel = effective_options.cancel.as_ref();

    let mut chain: Vec<Candidate> = vec![Candidate {
      provider: primary_id.clone(),
      model: primary_model.clone(),
    }];
    let mut fallbacks_resolved = false;
    let mut last_err: Option<RiboError> = None;
    let mut chain_summary: Vec<String> = Vec::new();
    let mut idx = 0usize;

    // ===== 候选链循环 =====
    // 外层遍历候选（主候选 + 惰性解析的 fallback），内层对每个
    // 候选按策略做建流重试；`last_err` 保留最后一次可降级错误。
    while let Some(candidate) = chain.get(idx) {
      idx += 1;
      // 候选的提供方必定已在目录中（候选由目录解析得出），不存在时视为内部错误。
      let cand_provider = {
        let catalog = self.catalog.read().expect("catalog lock poisoned");
        catalog.provider(&candidate.provider)
      }
      .expect("fallback candidate provider must be registered");
      // ===== 熔断准入 =====
      // 打开的候选直接跳过；半开放行的探测请求终结时需回报结果。
      let breaker = self.breaker_for(&candidate.provider);
      let admission = breaker.admit();
      if admission == Admission::Reject {
        tracing::warn!(provider = %candidate.provider, "circuit open, skipping candidate");
        chain_summary.push(format!("{} circuit-open", candidate.provider));
        continue;
      }
      let is_probe = admission == Admission::AllowProbe;
      let mut accounted = false;
      let mut attempt: u32 = 0;

      // ===== 候选内建流重试循环 =====
      // 每轮先检查取消，再以 build_timeout 包裹建流调用。
      loop {
        if let Some(c) = cancel
          && c.is_cancelled()
        {
          // 探测请求在回报前被取消：归还探测资格，避免熔断永久停留半开（R2）。
          if is_probe {
            breaker.abort_probe();
          }
          tracing::info!(
            "gateway request aborted by cancellation before attempt"
          );
          return Err(pre_stream(
            CANCELLED,
            "cancelled before gateway attempt",
          ));
        }
        let outcome = tokio::time::timeout(
          build_timeout,
          cand_provider.stream(ProviderOptions {
            model: &candidate.model,
            context,
            options: &effective_options,
          }),
        )
        .await;
        match outcome {
          // ===== 建流成功：打包观察流 =====
          // permit 与熔断器移交 ObservedStream，终结时回报并释放。
          Ok(Ok(stream)) => {
            return Ok(ObservedStream::new(
              stream,
              permit,
              breaker,
              admission,
              self.config.stream_idle_timeout,
            ));
          }
          // ===== 建流失败：回报熔断并按策略处置 =====
          Ok(Err(e)) => {
            if e.is_circuit_trigger() {
              breaker.record_failure();
              accounted = true;
            }
            chain_summary.push(format!("{} {}", candidate.provider, e.code()));

            match effective_retry.decide(&e, attempt) {
              RetryDecision::Retry { delay, .. } => {
                tracing::warn!(
                  provider = %candidate.provider,
                  model = %candidate.model.id,
                  attempt,
                  code = e.code(),
                  "pre-stream failure, retrying after {:?}",
                  delay
                );
                // 等待期被取消立即传播（permit 自动释放），并归还探测资格
                if let Err(cancel_err) = sleep_backoff(delay, cancel).await {
                  if is_probe {
                    breaker.abort_probe();
                  }
                  return Err(cancel_err);
                }
                attempt += 1;
              }
              RetryDecision::Exhausted | RetryDecision::NotRetryable => {
                // ===== 探测失败回报 =====
                if is_probe && !accounted {
                  breaker.record_failure();
                }
                // 可降级 → 惰性解析 fallback 后尝试下一候选
                if e.is_degradable() {
                  if !fallbacks_resolved {
                    let fallbacks = self.route.candidates(
                      &primary_model,
                      &e,
                      &*self.catalog.read().expect("catalog lock poisoned"),
                    );
                    chain.extend(fallbacks);
                    fallbacks_resolved = true;
                  }
                  last_err = Some(e);
                  break; // 下一候选
                }
                // 致命错误短路：换 provider 也解决不了（§10.2 错误处理策略）
                return Err(e);
              }
            }
          }
          // ===== 建流超时：与失败同径处置 =====
          // 超时合成 TIMEOUT 错误后，走同一套熔断回报与重试/降级判定。
          Err(_elapsed) => {
            let e = pre_stream(
              TIMEOUT,
              format!(
                "gateway stream build timeout ({:?}) for provider '{}'",
                build_timeout, candidate.provider
              ),
            );
            // 合成的 TIMEOUT 恒为熔断触发项，直接记一次失败。
            breaker.record_failure();
            chain_summary.push(format!("{} timeout", candidate.provider));

            match effective_retry.decide(&e, attempt) {
              RetryDecision::Retry { delay, .. } => {
                tracing::warn!(
                  provider = %candidate.provider,
                  model = %candidate.model.id,
                  attempt,
                  "stream build timed out, retrying after {:?}",
                  delay
                );
                if let Err(cancel_err) = sleep_backoff(delay, cancel).await {
                  if is_probe {
                    breaker.abort_probe();
                  }
                  return Err(cancel_err);
                }
                attempt += 1;
              }
              RetryDecision::Exhausted | RetryDecision::NotRetryable => {
                // 超时合成的 TIMEOUT 恒为可降级错误，且已在循环前计入熔断，
                // 此处直接解析候选并切换到下一候选。
                if !fallbacks_resolved {
                  let fallbacks = self.route.candidates(
                    &primary_model,
                    &e,
                    &*self.catalog.read().expect("catalog lock poisoned"),
                  );
                  chain.extend(fallbacks);
                  fallbacks_resolved = true;
                }
                last_err = Some(e);
                break;
              }
            }
          }
        }
      }
    }

    let err = last_err.unwrap_or_else(|| {
      pre_stream(CIRCUIT_OPEN, "all candidates rejected by circuit breakers")
    });
    Err(with_chain_summary(err, &chain_summary))
  }
}

/// 把降级链摘要附加到最终错误上。
///
/// 不改错误码，仅在 message 末尾追加 `fallback chain: ...`，
/// 并保留原错误的全部 context，供上层继续分类与排障。
fn with_chain_summary(err: RiboError, chain: &[String]) -> RiboError {
  let mut rebuilt = pre_stream(
    err.code(),
    format!("{}; fallback chain: {}", err.message(), chain.join("; ")),
  );
  if let Some(ctx) = err.context() {
    for (k, v) in ctx.iter() {
      rebuilt = rebuilt.with_context(k, v.clone());
    }
  }
  rebuilt
}
#[cfg(test)]
mod tests {
  use std::{
    collections::VecDeque,
    sync::{
      Mutex,
      atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
  };

  use ra_core::message::{AssistantMessage, StopReason};

  use super::*;
  use crate::{
    error::{
      AUTH_FAILED, CANCELLED, CIRCUIT_OPEN, DUPLICATE_PROVIDER,
      INVALID_REQUEST, MODEL_NOT_FOUND, OVERLOADED, RATE_LIMITED,
      RETRY_PROVIDER_REFUSED, SERVER_ERROR, TIMEOUT,
    },
    model::Model,
    options::{CancelToken, RetryOptions},
    retry::RetryPolicy,
    stream::{ProviderEventStream, ProviderEventStreamProducer},
  };

  /// 可编排建流结果的测试桩：按队列依次返回结果并统计调用次数。
  struct StubProvider {
    id: ProviderId,
    models: Vec<Model>,
    outcomes: Mutex<VecDeque<RiboResult<ProviderEventStream>>>,
    calls: AtomicUsize,
    retrying: bool,
    /// 建流前的模拟耗时；用于触发网关建流超时。
    delay: Option<Duration>,
  }

  impl StubProvider {
    fn new(
      id: &str,
      models: Vec<Model>,
      outcomes: Vec<RiboResult<ProviderEventStream>>,
    ) -> Self {
      Self {
        id: provider(id),
        models,
        outcomes: Mutex::new(outcomes.into()),
        calls: AtomicUsize::new(0),
        retrying: false,
        delay: None,
      }
    }

    fn retrying(mut self) -> Self {
      self.retrying = true;
      self
    }

    /// 让建流调用先等待指定时长，用于构造超时场景。
    fn slow(mut self, delay: Duration) -> Self {
      self.delay = Some(delay);
      self
    }

    fn calls(&self) -> usize {
      self.calls.load(Ordering::SeqCst)
    }
  }

  #[async_trait::async_trait]
  impl Provider for StubProvider {
    fn id(&self) -> &ProviderId {
      &self.id
    }
    fn models(&self) -> &[Model] {
      &self.models
    }
    fn is_retrying_provider(&self) -> bool {
      self.retrying
    }
    async fn stream(
      &self,
      _options: ProviderOptions<'_>,
    ) -> RiboResult<ProviderEventStream> {
      self.calls.fetch_add(1, Ordering::SeqCst);
      if let Some(delay) = self.delay {
        tokio::time::sleep(delay).await;
      }
      self
        .outcomes
        .lock()
        .expect("测试桩锁不应中毒")
        .pop_front()
        .expect("编排结果已耗尽——出现计划外的额外调用")
    }
  }

  fn provider(raw: &str) -> ProviderId {
    ProviderId::parse(raw).expect("测试提供方 ID 合法")
  }

  fn model(provider_raw: &str, id: &str) -> Model {
    Model::new(
      provider(provider_raw),
      ModelId::parse(id).expect("测试模型 ID 合法"),
      id,
      "http://stub.local",
    )
    .with_context_window(8192)
    .with_max_tokens(1024)
  }

  fn ok_stream() -> ProviderEventStream {
    let (producer, stream) = ProviderEventStreamProducer::build();
    producer.done(
      StopReason::Stop,
      AssistantMessage::terminal("p1", "m1", StopReason::Stop, "", 0),
    );
    stream
  }

  /// 零退避策略，避免测试依赖真实等待。
  fn instant_policy(max_retries: u32) -> RetryPolicy {
    RetryPolicy {
      initial_backoff: Duration::ZERO,
      max_retries,
      jitter_ratio: 0.,
      ..RetryPolicy::default()
    }
  }

  fn config(max_retries: u32) -> GatewayConfig {
    crate::test_support::init();
    GatewayConfig {
      retry: instant_policy(max_retries),
      ..GatewayConfig::default()
    }
  }

  fn model_id(raw: &str) -> ModelId {
    ModelId::parse(raw).expect("测试模型 ID 合法")
  }

  /// 场景：注册重复的提供方 ID。
  ///
  /// 预期：第二次注册返回 DUPLICATE_PROVIDER。
  #[test]
  fn rejects_duplicate_provider_id() {
    let mut gateway = Gateway::new(config(0));
    let models = vec![model("p1", "m1")];
    gateway
      .register(Arc::new(StubProvider::new("p1", models.clone(), vec![])))
      .expect("首次注册应成功");

    let err = gateway
      .register(Arc::new(StubProvider::new("p1", models, vec![])))
      .expect_err("重复 ID 应被拒绝");
    assert_eq!(err.code(), DUPLICATE_PROVIDER);
  }

  /// 场景：注册自带重试的提供方。
  ///
  /// 预期：返回 RETRY_PROVIDER_REFUSED，避免重试叠加。
  #[test]
  fn rejects_retrying_provider() {
    let mut gateway = Gateway::new(config(0));
    let stub =
      StubProvider::new("p1", vec![model("p1", "m1")], vec![]).retrying();

    let err = gateway
      .register(Arc::new(stub))
      .expect_err("自带重试的提供方应被拒绝");
    assert_eq!(err.code(), RETRY_PROVIDER_REFUSED);
  }

  /// 场景：注册未暴露模型的提供方。
  ///
  /// 预期：返回 INVALID_REQUEST。
  #[test]
  fn rejects_invalid_provider() {
    let mut gateway = Gateway::new(config(0));
    let stub = StubProvider::new("p1", vec![], vec![]);

    let err = gateway
      .register(Arc::new(stub))
      .expect_err("空模型列表应被拒绝");
    assert_eq!(err.code(), INVALID_REQUEST);
  }

  /// 场景：目录中没有请求的模型。
  ///
  /// 预期：返回 MODEL_NOT_FOUND，且不发起任何建流。
  #[tokio::test]
  async fn returns_model_not_found_when_catalog_empty() {
    let gateway = Gateway::new(config(0));
    let context = Context::default();

    let err = gateway
      .stream(&model_id("missing"), &context, StreamOptions::default())
      .await
      .err()
      .expect("未注册模型应报错");
    assert_eq!(err.code(), MODEL_NOT_FOUND);
  }

  /// 场景：全局在途配额为 0。
  ///
  /// 预期：请求在准入阶段即返回 OVERLOADED。
  #[tokio::test]
  async fn returns_overloaded_when_no_in_flight_quota() {
    let gateway = Gateway::new(GatewayConfig {
      max_in_flight: 0,
      ..config(0)
    });
    let context = Context::default();

    let err = gateway
      .stream(&model_id("m1"), &context, StreamOptions::default())
      .await
      .err()
      .expect("无配额应被拒绝");
    assert_eq!(err.code(), OVERLOADED);
  }

  /// 场景：首次建流失败、重试后成功。
  ///
  /// 预期：返回可观察流，实际调用提供方两次。
  #[tokio::test]
  async fn retries_transient_failure_then_succeeds() {
    let stub = Arc::new(StubProvider::new(
      "p1",
      vec![model("p1", "m1")],
      vec![
        Err(pre_stream(RATE_LIMITED, "rate limited")),
        Ok(ok_stream()),
      ],
    ));
    let mut gateway = Gateway::new(config(1));
    gateway.register(stub.clone()).expect("注册应成功");
    let context = Context::default();

    gateway
      .stream(&model_id("m1"), &context, StreamOptions::default())
      .await
      .expect("重试后应成功");
    assert_eq!(stub.calls(), 2);
  }

  /// 场景：重试耗尽且路由表没有备选。
  ///
  /// 预期：上抛最后一次错误，并在 message 中附加降级链摘要。
  #[tokio::test]
  async fn exhausted_without_fallback_returns_last_error() {
    let stub = Arc::new(StubProvider::new(
      "p1",
      vec![model("p1", "m1")],
      vec![Err(pre_stream(SERVER_ERROR, "boom"))],
    ));
    let mut gateway = Gateway::new(config(0));
    gateway.register(stub.clone()).expect("注册应成功");
    let context = Context::default();

    let err = gateway
      .stream(&model_id("m1"), &context, StreamOptions::default())
      .await
      .err()
      .expect("无备选时应上抛错误");
    assert_eq!(err.code(), SERVER_ERROR);
    assert!(err.message().contains("fallback chain"));
    assert_eq!(stub.calls(), 1);
  }

  /// 场景：主候选失败后沿同提供方备选链切换。
  ///
  /// 预期：第二个候选建流成功，实际调用提供方两次。
  #[tokio::test]
  async fn falls_back_to_same_provider_alternate() {
    let stub = Arc::new(StubProvider::new(
      "p1",
      vec![model("p1", "m1"), model("p1", "m2")],
      vec![Err(pre_stream(SERVER_ERROR, "boom")), Ok(ok_stream())],
    ));
    let mut route = RouteMap::default();
    route.insert_same_provider(model_id("m1"), vec![model_id("m2")]);
    let mut gateway = Gateway::new(config(0)).with_routes(route);
    gateway.register(stub.clone()).expect("注册应成功");
    let context = Context::default();

    gateway
      .stream(&model_id("m1"), &context, StreamOptions::default())
      .await
      .expect("备选候选应成功");
    assert_eq!(stub.calls(), 2);
  }

  /// 场景：不可降级的致命错误。
  ///
  /// 预期：立即短路返回原错误，不尝试任何备选。
  #[tokio::test]
  async fn fatal_error_short_circuits() {
    let stub = Arc::new(StubProvider::new(
      "p1",
      vec![model("p1", "m1")],
      vec![Err(pre_stream(INVALID_REQUEST, "bad request"))],
    ));
    let mut gateway = Gateway::new(config(3));
    gateway.register(stub.clone()).expect("注册应成功");
    let context = Context::default();

    let err = gateway
      .stream(&model_id("m1"), &context, StreamOptions::default())
      .await
      .err()
      .expect("致命错误应直接返回");
    assert_eq!(err.code(), INVALID_REQUEST);
    assert_eq!(stub.calls(), 1);
  }

  /// 场景：某候选连续失败触发熔断后，再次请求同一模型。
  ///
  /// 预期：第二次请求被熔断拒绝，返回 CIRCUIT_OPEN 且不再调用提供方。
  #[tokio::test]
  async fn open_circuit_rejects_subsequent_request() {
    let stub = Arc::new(StubProvider::new(
      "p1",
      vec![model("p1", "m1")],
      vec![Err(pre_stream(SERVER_ERROR, "boom"))],
    ));
    let mut gateway = Gateway::new(GatewayConfig {
      circuit_threshold: 1,
      ..config(0)
    });
    gateway.register(stub.clone()).expect("注册应成功");
    let context = Context::default();

    let first = gateway
      .stream(&model_id("m1"), &context, StreamOptions::default())
      .await
      .err()
      .expect("首次请求应失败");
    assert_eq!(first.code(), SERVER_ERROR);
    assert_eq!(stub.calls(), 1);

    let second = gateway
      .stream(&model_id("m1"), &context, StreamOptions::default())
      .await
      .err()
      .expect("熔断打开后应拒绝");
    assert_eq!(second.code(), CIRCUIT_OPEN);
    assert_eq!(stub.calls(), 1, "熔断打开后不应再调用提供方");
  }

  /// 场景：请求在首次尝试前被取消。
  ///
  /// 预期：返回 CANCELLED，且不调用提供方。
  #[tokio::test]
  async fn cancellation_before_attempt_returns_cancelled() {
    let stub = Arc::new(StubProvider::new(
      "p1",
      vec![model("p1", "m1")],
      vec![Ok(ok_stream())],
    ));
    let mut gateway = Gateway::new(config(0));
    gateway.register(stub.clone()).expect("注册应成功");
    let context = Context::default();

    let cancel = CancelToken::new();
    cancel.cancel();
    let options = StreamOptions {
      cancel: Some(cancel),
      ..StreamOptions::default()
    };

    let err = gateway
      .stream(&model_id("m1"), &context, options)
      .await
      .err()
      .expect("取消后应返回错误");
    assert_eq!(err.code(), CANCELLED);
    assert_eq!(stub.calls(), 0);
  }

  /// 场景：半开探测在首次尝试前被取消（对应审查报告 R2）。
  ///
  /// 预期：探测资格被归还，后续请求仍能重新准入并成功。
  #[tokio::test(start_paused = true)]
  async fn cancelled_probe_releases_admission() {
    let stub = Arc::new(StubProvider::new(
      "p1",
      vec![model("p1", "m1")],
      vec![Err(pre_stream(SERVER_ERROR, "boom")), Ok(ok_stream())],
    ));
    let mut gateway = Gateway::new(GatewayConfig {
      circuit_threshold: 1,
      circuit_cooldown: Duration::from_secs(30),
      ..config(0)
    });
    gateway.register(stub.clone()).expect("注册应成功");
    let context = Context::default();

    // 首次失败打开熔断。
    gateway
      .stream(&model_id("m1"), &context, StreamOptions::default())
      .await
      .err()
      .expect("首次请求应失败");

    // 冷却期满进入半开。
    tokio::time::advance(Duration::from_secs(30)).await;

    // 探测请求在首次尝试前被取消。
    let cancel = CancelToken::new();
    cancel.cancel();
    let cancelled_options = StreamOptions {
      cancel: Some(cancel),
      ..StreamOptions::default()
    };
    let err = gateway
      .stream(&model_id("m1"), &context, cancelled_options)
      .await
      .err()
      .expect("探测请求应被取消");
    assert_eq!(err.code(), CANCELLED);

    // 探测资格应已归还，第三次请求可正常建流。
    gateway
      .stream(&model_id("m1"), &context, StreamOptions::default())
      .await
      .expect("探测资格归还后应可再次准入");
    assert_eq!(stub.calls(), 2);
  }

  /// 场景：请求级重试选项覆盖网关基线。
  ///
  /// 预期：`max_retries = 0` 时首次失败即结束，不再重试。
  #[tokio::test]
  async fn request_options_can_disable_retries() {
    let stub = Arc::new(StubProvider::new(
      "p1",
      vec![model("p1", "m1")],
      vec![Err(pre_stream(SERVER_ERROR, "boom"))],
    ));
    let mut gateway = Gateway::new(config(3));
    gateway.register(stub.clone()).expect("注册应成功");
    let context = Context::default();
    let options = StreamOptions {
      retry: RetryOptions {
        max_retries: Some(0),
        ..RetryOptions::default()
      },
      ..StreamOptions::default()
    };

    let err = gateway
      .stream(&model_id("m1"), &context, options)
      .await
      .err()
      .expect("禁用重试后应直接失败");
    assert_eq!(err.code(), SERVER_ERROR);
    assert_eq!(stub.calls(), 1);
  }

  /// 场景：提供方返回可跨提供方降级的错误，但路由表未配置目标。
  ///
  /// 预期：错误码保持不变（鉴权失败），不产生额外调用。
  #[tokio::test]
  async fn cross_provider_error_without_route_returns_original() {
    let stub = Arc::new(StubProvider::new(
      "p1",
      vec![model("p1", "m1")],
      vec![Err(pre_stream(AUTH_FAILED, "unauthorized"))],
    ));
    let mut gateway = Gateway::new(config(0));
    gateway.register(stub.clone()).expect("注册应成功");
    let context = Context::default();

    let err = gateway
      .stream(&model_id("m1"), &context, StreamOptions::default())
      .await
      .err()
      .expect("无跨提供方路由时应返回原错误");
    assert_eq!(err.code(), AUTH_FAILED);
    assert_eq!(stub.calls(), 1);
  }

  /// 场景：半开探测返回不可降级的致命错误。
  ///
  /// 预期：探测失败被计入熔断，错误原样上抛。
  #[tokio::test(start_paused = true)]
  async fn fatal_error_on_probe_records_failure() {
    let stub = Arc::new(StubProvider::new(
      "p1",
      vec![model("p1", "m1")],
      vec![Err(pre_stream(INVALID_REQUEST, "bad request"))],
    ));
    let mut gateway = Gateway::new(GatewayConfig {
      circuit_threshold: 1,
      circuit_cooldown: Duration::from_secs(30),
      ..config(0)
    });
    gateway.register(stub.clone()).expect("注册应成功");
    // 直接打开熔断，再推进到半开。
    let breaker = gateway.breaker_for(&provider("p1"));
    breaker.record_failure();
    tokio::time::advance(Duration::from_secs(30)).await;
    let context = Context::default();

    let err = gateway
      .stream(&model_id("m1"), &context, StreamOptions::default())
      .await
      .err()
      .expect("探测请求应失败");

    assert_eq!(err.code(), INVALID_REQUEST);
    assert_eq!(breaker.state(), ra_core::circuit::CircuitState::Open);
    assert_eq!(stub.calls(), 1);
  }

  /// 场景：半开探测在重试退避期间被取消。
  ///
  /// 预期：返回 CANCELLED、探测资格归还，后续请求仍可重新准入。
  #[tokio::test(start_paused = true)]
  async fn cancelled_probe_during_backoff_releases_admission() {
    let stub = Arc::new(StubProvider::new(
      "p1",
      vec![model("p1", "m1")],
      vec![Err(pre_stream(SERVER_ERROR, "boom")), Ok(ok_stream())],
    ));
    let mut gateway = Gateway::new(GatewayConfig {
      circuit_threshold: 1,
      circuit_cooldown: Duration::from_secs(30),
      retry: RetryPolicy {
        initial_backoff: Duration::from_secs(3600),
        max_retries: 3,
        jitter_ratio: 0.,
        ..RetryPolicy::default()
      },
      ..config(0)
    });
    gateway.register(stub.clone()).expect("注册应成功");
    gateway.breaker_for(&provider("p1")).record_failure();
    tokio::time::advance(Duration::from_secs(30)).await;
    let context = Context::default();

    let cancel = CancelToken::new();
    let canceller = {
      let cancel = cancel.clone();
      tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        cancel.cancel();
      })
    };
    let options = StreamOptions {
      cancel: Some(cancel),
      ..StreamOptions::default()
    };

    let err = gateway
      .stream(&model_id("m1"), &context, options)
      .await
      .err()
      .expect("退避期取消应返回错误");
    canceller.await.expect("取消任务不应 panic");

    assert_eq!(err.code(), CANCELLED);
    assert_eq!(stub.calls(), 1, "取消后不得再发起尝试");

    // 探测失败已重新打开熔断，冷却期满后再次进入半开。
    tokio::time::advance(Duration::from_secs(30)).await;
    gateway
      .stream(&model_id("m1"), &context, StreamOptions::default())
      .await
      .expect("探测资格归还后应可重新准入");
    assert_eq!(stub.calls(), 2);
  }

  /// 场景：建流耗时超过默认超时。
  ///
  /// 预期：合成 TIMEOUT 错误；超时可重试，共调用两次；最终错误附降级链摘要。
  #[tokio::test(start_paused = true)]
  async fn build_timeout_records_failure_and_returns_timeout() {
    let stub = Arc::new(
      StubProvider::new("p1", vec![model("p1", "m1")], vec![])
        .slow(Duration::from_secs(3600)),
    );
    let mut gateway = Gateway::new(GatewayConfig {
      default_timeout: Duration::from_millis(10),
      ..config(1)
    });
    gateway.register(stub.clone()).expect("注册应成功");
    let context = Context::default();

    let err = gateway
      .stream(&model_id("m1"), &context, StreamOptions::default())
      .await
      .err()
      .expect("建流超时应失败");

    assert_eq!(err.code(), TIMEOUT);
    assert!(err.message().contains("fallback chain"));
    assert_eq!(stub.calls(), 2, "超时可重试，应调用两次");
  }

  /// 场景：半开探测的建流超时在退避期被取消。
  ///
  /// 预期：返回 CANCELLED，探测资格被归还。
  #[tokio::test(start_paused = true)]
  async fn cancelled_probe_during_timeout_backoff_releases_admission() {
    let stub = Arc::new(
      StubProvider::new("p1", vec![model("p1", "m1")], vec![])
        .slow(Duration::from_secs(3600)),
    );
    let mut gateway = Gateway::new(GatewayConfig {
      default_timeout: Duration::from_millis(10),
      circuit_threshold: 1,
      circuit_cooldown: Duration::from_secs(30),
      retry: RetryPolicy {
        initial_backoff: Duration::from_secs(3600),
        max_retries: 3,
        jitter_ratio: 0.,
        ..RetryPolicy::default()
      },
      ..config(0)
    });
    gateway.register(stub.clone()).expect("注册应成功");
    gateway.breaker_for(&provider("p1")).record_failure();
    tokio::time::advance(Duration::from_secs(30)).await;
    let context = Context::default();

    let cancel = CancelToken::new();
    let canceller = {
      let cancel = cancel.clone();
      tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        cancel.cancel();
      })
    };
    let options = StreamOptions {
      cancel: Some(cancel),
      ..StreamOptions::default()
    };

    let err = gateway
      .stream(&model_id("m1"), &context, options)
      .await
      .err()
      .expect("退避期取消应返回错误");
    canceller.await.expect("取消任务不应 panic");

    assert_eq!(err.code(), CANCELLED);
    assert_eq!(stub.calls(), 1);
  }

  /// 场景：最终错误没有 context。
  ///
  /// 预期：仍追加降级链摘要，并由构造器补齐 context。
  #[test]
  fn with_chain_summary_handles_error_without_context() {
    let err = ra_core::error::RiboError::new("provider", SERVER_ERROR, "boom");
    assert!(err.context().is_none());

    let out = with_chain_summary(err, &["p1 timeout".to_string()]);

    assert_eq!(out.code(), SERVER_ERROR);
    assert!(out.message().contains("fallback chain: p1 timeout"));
    assert!(out.context().is_some());
  }

  /// 场景：请求显式提供 timeout。
  ///
  /// 预期：跳过默认超时合并，建流成功。
  #[tokio::test]
  async fn explicit_request_timeout_is_used() {
    let stub = Arc::new(StubProvider::new(
      "p1",
      vec![model("p1", "m1")],
      vec![Ok(ok_stream())],
    ));
    let mut gateway = Gateway::new(config(0));
    gateway.register(stub).expect("注册应成功");
    let context = Context::default();
    let options = StreamOptions {
      timeout: Some(Duration::from_secs(5)),
      ..StreamOptions::default()
    };

    gateway
      .stream(&model_id("m1"), &context, options)
      .await
      .expect("显式超时下应成功");
  }

  /// 场景：普通请求（非探测）在重试退避期被取消。
  ///
  /// 预期：返回 CANCELLED，且探测资格分支不被触发。
  #[tokio::test(start_paused = true)]
  async fn cancellation_during_backoff_without_probe() {
    let stub = Arc::new(StubProvider::new(
      "p1",
      vec![model("p1", "m1")],
      vec![Err(pre_stream(SERVER_ERROR, "boom"))],
    ));
    let mut gateway = Gateway::new(GatewayConfig {
      retry: RetryPolicy {
        initial_backoff: Duration::from_secs(3600),
        max_retries: 3,
        jitter_ratio: 0.,
        ..RetryPolicy::default()
      },
      ..config(0)
    });
    gateway.register(stub.clone()).expect("注册应成功");
    let context = Context::default();

    let cancel = CancelToken::new();
    let canceller = {
      let cancel = cancel.clone();
      tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        cancel.cancel();
      })
    };
    let options = StreamOptions {
      cancel: Some(cancel),
      ..StreamOptions::default()
    };

    let err = gateway
      .stream(&model_id("m1"), &context, options)
      .await
      .err()
      .expect("退避期取消应返回错误");
    canceller.await.expect("取消任务不应 panic");

    assert_eq!(err.code(), CANCELLED);
    assert_eq!(stub.calls(), 1);
  }

  /// 场景：普通请求的建流超时在退避期被取消。
  ///
  /// 预期：返回 CANCELLED，且探测资格分支不被触发。
  #[tokio::test(start_paused = true)]
  async fn cancellation_during_timeout_backoff_without_probe() {
    let stub = Arc::new(
      StubProvider::new("p1", vec![model("p1", "m1")], vec![])
        .slow(Duration::from_secs(3600)),
    );
    let mut gateway = Gateway::new(GatewayConfig {
      default_timeout: Duration::from_millis(10),
      retry: RetryPolicy {
        initial_backoff: Duration::from_secs(3600),
        max_retries: 3,
        jitter_ratio: 0.,
        ..RetryPolicy::default()
      },
      ..config(0)
    });
    gateway.register(stub.clone()).expect("注册应成功");
    let context = Context::default();

    let cancel = CancelToken::new();
    let canceller = {
      let cancel = cancel.clone();
      tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        cancel.cancel();
      })
    };
    let options = StreamOptions {
      cancel: Some(cancel),
      ..StreamOptions::default()
    };

    let err = gateway
      .stream(&model_id("m1"), &context, options)
      .await
      .err()
      .expect("超时退避期取消应返回错误");
    canceller.await.expect("取消任务不应 panic");

    assert_eq!(err.code(), CANCELLED);
    assert_eq!(stub.calls(), 1);
  }

  /// 场景：同提供方备选链上的候选全部失败。
  ///
  /// 预期：返回最后一次错误，调用两次；第二次不再重复解析降级链。
  #[tokio::test]
  async fn fallback_candidates_all_fail() {
    let stub = Arc::new(StubProvider::new(
      "p1",
      vec![model("p1", "m1"), model("p1", "m2")],
      vec![
        Err(pre_stream(SERVER_ERROR, "boom")),
        Err(pre_stream(SERVER_ERROR, "boom again")),
      ],
    ));
    let mut route = RouteMap::default();
    route.insert_same_provider(model_id("m1"), vec![model_id("m2")]);
    let mut gateway = Gateway::new(config(0)).with_routes(route);
    gateway.register(stub.clone()).expect("注册应成功");
    let context = Context::default();

    let err = gateway
      .stream(&model_id("m1"), &context, StreamOptions::default())
      .await
      .err()
      .expect("备选也应失败");

    assert_eq!(err.code(), SERVER_ERROR);
    assert_eq!(stub.calls(), 2);
  }

  /// 场景：主候选与备选候选均建流超时。
  ///
  /// 预期：返回 TIMEOUT；第二次超时不再重复解析降级链。
  #[tokio::test(start_paused = true)]
  async fn all_candidates_timeout() {
    let stub = Arc::new(
      StubProvider::new(
        "p1",
        vec![model("p1", "m1"), model("p1", "m2")],
        vec![],
      )
      .slow(Duration::from_secs(3600)),
    );
    let mut route = RouteMap::default();
    route.insert_same_provider(model_id("m1"), vec![model_id("m2")]);
    let mut gateway = Gateway::new(GatewayConfig {
      default_timeout: Duration::from_millis(10),
      ..config(0)
    })
    .with_routes(route);
    gateway.register(stub.clone()).expect("注册应成功");
    let context = Context::default();

    let err = gateway
      .stream(&model_id("m1"), &context, StreamOptions::default())
      .await
      .err()
      .expect("两个候选均应超时");

    assert_eq!(err.code(), TIMEOUT);
    assert_eq!(stub.calls(), 2);
  }
}
