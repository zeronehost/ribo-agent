//! 自带重试的 `Provider` 包装器。
//!
//! 将 `RetryPolicy` 与可选的首包超时守卫组合起来，在建流失败
//! （PreStream）或首包超时时自动重试；流内错误按契约不做重试。

use ra_core::error::RiboResult;

use crate::{
  error::{CANCELLED, TIMEOUT, TRANSPORT, pre_stream},
  ids::{ModelId, ProviderId},
  model::Model,
  options::ProviderOptions,
  retry::backoff::sleep_backoff,
  stream::ProviderEventStream,
  traits::Provider,
};

use super::{
  first_token::{FirstTokenPolicy, Peeked, peek_first_content},
  policy::{RetryDecision, RetryPolicy},
};

/// 自带重试的 `Provider` 包装器。
///
/// 包装任意 [`Provider`] 实现：建流失败（PreStream）或首包超时时，
/// 按 [`RetryPolicy`] 自动重试；同时通过 [`Provider::is_retrying_provider`]
/// 把自身标记为已带重试，供上层避免叠加多层重试。
pub struct RetryingProvider<P> {
  /// 被包装的真实提供方。
  inner: P,
  /// 重试策略。
  policy: RetryPolicy,
  /// 可选的首包超时守卫；`None` 表示不启用。
  first_token: Option<FirstTokenPolicy>,
}

impl<P: Provider> RetryingProvider<P> {
  /// 创建重试包装器。
  ///
  /// # 参数
  ///
  /// - `inner`：被包装的真实提供方；
  /// - `policy`：重试策略。
  pub fn new(inner: P, policy: RetryPolicy) -> Self {
    Self {
      inner,
      policy,
      first_token: None,
    }
  }
  /// 启用首包超时守卫（builder 风格，消费并返回自身）。
  ///
  /// 守卫在建流成功后生效：等待首个内容事件超过 `guard.timeout` 时，
  /// 按 `guard.retry_on_timeout` 决定重试还是直接报错。
  pub fn with_first_token_guard(mut self, guard: FirstTokenPolicy) -> Self {
    self.first_token = Some(guard);
    self
  }
}

#[async_trait::async_trait]
impl<P: Provider> Provider for RetryingProvider<P> {
  fn id(&self) -> &ProviderId {
    self.inner.id()
  }
  fn models(&self) -> &[Model] {
    self.inner.models()
  }
  fn supports(&self, model: &ModelId) -> bool {
    self.inner.supports(model)
  }
  fn is_retrying_provider(&self) -> bool {
    true
  }
  async fn stream(
    &self,
    options: ProviderOptions<'_>,
  ) -> RiboResult<ProviderEventStream> {
    let effective = self.policy.effective(options.options);
    let cancel = options.options.cancel.as_ref();

    let mut attempt: u32 = 0;

    loop {
      // ===== 取消检查 =====
      // 每次尝试前先看取消令牌，避免在已取消的任务上继续发请求。
      if let Some(c) = cancel
        && c.is_cancelled()
      {
        tracing::info!(
          "retrying provider aborted by cancellation before attempt"
        );
        return Err(pre_stream(CANCELLED, "cancelled before retry attempt"));
      }

      let _attempt_span = tracing::info_span!(
        "retrying_provider.stream",
        provider = %self.inner.id(),
        model = %options.model.id,
        attempt
      );
      let _g = _attempt_span.enter();

      // ===== 建流尝试 =====
      // Ok 分支先过首包守卫再交付；Err 分支进入重试判定。
      match self.inner.stream(options).await {
        Ok(stream) => {
          // ===== 首包守卫 =====
          // 窥视首个内容事件：超时/异常关闭按策略决定重试或直接报错。
          if let Some(guard) = self.first_token {
            match peek_first_content(stream, &guard, cancel).await {
              Peeked::Ready(new_stream) => return Ok(new_stream),
              Peeked::Timeout => {
                // ===== 首包超时处置 =====
                if !guard.retry_on_timeout {
                  tracing::warn!(
                    "first-token guard timed out, retry disabled — returning error"
                  );
                  return Err(pre_stream(
                    TIMEOUT,
                    "first content event timed out",
                  ));
                }
                let err = pre_stream(TIMEOUT, "first content event timed out");
                match effective.decide(&err, attempt) {
                  RetryDecision::Retry { delay, .. } => {
                    tracing::warn!(
                      "first-token timed out, retrying after {:?}",
                      delay
                    );
                    sleep_backoff(delay, cancel).await?;
                    attempt += 1;
                    continue;
                  }
                  RetryDecision::Exhausted | RetryDecision::NotRetryable => {
                    return Err(err);
                  }
                }
              }
              Peeked::Aborted => {
                return Err(pre_stream(CANCELLED, "first-token guard aborted"));
              }
              Peeked::Closed => {
                tracing::warn!(
                  "stream closed before first content, treating as transport error"
                );
                let err =
                  pre_stream(TRANSPORT, "stream closed before first content");
                match effective.decide(&err, attempt) {
                  RetryDecision::Retry { delay, .. } => {
                    tracing::warn!(
                      "transport error before content, retrying after {:?}",
                      delay
                    );
                    sleep_backoff(delay, cancel).await?;
                    attempt += 1;
                    continue;
                  }
                  RetryDecision::Exhausted | RetryDecision::NotRetryable => {
                    return Err(err);
                  }
                }
              }
            }
          } else {
            return Ok(stream);
          }
        }
        Err(err) => {
          // ===== 建流失败：按策略判定重试 =====
          // 仅 PreStream 错误会走到这里（流内失败由 Ok 分支的终结事件承载）。
          match effective.decide(&err, attempt) {
            RetryDecision::Retry { delay, .. } => {
              tracing::warn!(
                provider = %self.inner.id(),
                model = %options.model.id,
                attempt,
                code = err.code(),
                "pre-stream failure, retrying after {:?}",
                delay
              );
              // 等待期被取消立即传播——不计入上游故障
              sleep_backoff(delay, cancel).await?;
              attempt += 1;
            }
            RetryDecision::Exhausted => {
              tracing::error!(
                provider = %self.inner.id(),
                model = %options.model.id,
                code = err.code(),
                "pre-stream retry exhausted, bubbling error"
              );
              return Err(err);
            }
            RetryDecision::NotRetryable => {
              tracing::debug!(
                provider = %self.inner.id(),
                model = %options.model.id,
                code = err.code(),
                "pre-stream failure is not retryable, bubbling"
              );
              return Err(err);
            }
          }
        }
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use std::collections::VecDeque;
  use std::sync::Mutex;
  use std::sync::atomic::{AtomicUsize, Ordering};
  use std::time::Duration;

  use super::*;
  use ra_core::message::{AssistantMessage, StopReason};

  use crate::{
    context::Context,
    error::{SERVER_ERROR, STREAM_INTERRUPTED, in_stream},
    options::{CancelToken, StreamOptions},
    stream::ProviderEventStreamProducer,
  };

  /// 可编排建流结果的测试桩：按队列依次返回结果并统计调用次数；
  /// 编排耗尽后再被调用会 panic，用于暴露"计划外的额外尝试"。
  struct StubProvider {
    id: ProviderId,
    outcomes: Mutex<VecDeque<RiboResult<ProviderEventStream>>>,
    calls: AtomicUsize,
  }

  impl StubProvider {
    /// 单结果测试桩。
    fn new(outcome: RiboResult<ProviderEventStream>) -> Self {
      Self::new_many(vec![outcome])
    }

    /// 多结果测试桩（按顺序逐次返回）。
    fn new_many(outcomes: Vec<RiboResult<ProviderEventStream>>) -> Self {
      Self {
        id: ProviderId::parse("stub").expect("stub id 合法"),
        outcomes: Mutex::new(outcomes.into()),
        calls: AtomicUsize::new(0),
      }
    }

    /// 已被调用的次数。
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
      &[]
    }
    async fn stream(
      &self,
      _options: ProviderOptions<'_>,
    ) -> RiboResult<ProviderEventStream> {
      self.calls.fetch_add(1, Ordering::SeqCst);
      self
        .outcomes
        .lock()
        .expect("测试桩锁不应中毒")
        .pop_front()
        .expect("编排结果已耗尽——出现计划外的额外调用")
    }
  }

  /// 零退避策略：重试瞬间完成，测试无需推进虚拟时钟。
  fn instant_policy() -> RetryPolicy {
    RetryPolicy {
      initial_backoff: Duration::ZERO,
      jitter_ratio: 0.,
      ..RetryPolicy::default()
    }
  }

  /// 构造一条已正常终结的空流，作为建流成功结果。
  fn ok_stream() -> ProviderEventStream {
    let (producer, stream) = ProviderEventStreamProducer::build();
    producer.done(
      StopReason::Stop,
      AssistantMessage::terminal("", "", StopReason::Stop, "", 0),
    );
    stream
  }

  /// 场景：内层前 2 次返回 `Err(SERVER_ERROR)`、第 3 次成功
  /// → 最终返回 Ok，且实际调用次数为 3。
  #[tokio::test]
  async fn retries_until_success_within_budget() {
    let stub = StubProvider::new_many(vec![
      Err(pre_stream(SERVER_ERROR, "boom 1")),
      Err(pre_stream(SERVER_ERROR, "boom 2")),
      Ok(ok_stream()),
    ]);
    let retrying = RetryingProvider::new(stub, instant_policy());

    let model = Model::new(
      ProviderId::parse("stub").expect("stub id 合法"),
      ModelId::parse("stub-model").expect("stub model 合法"),
      "Stub Model",
      "http://stub.local",
    );
    let context = Context::default();
    let stream_options = StreamOptions::default();
    let res = retrying
      .stream(ProviderOptions {
        model: &model,
        context: &context,
        options: &stream_options,
      })
      .await;

    assert!(res.is_ok(), "第 3 次尝试应成功");
    assert_eq!(retrying.inner.calls(), 3);
  }

  /// 场景：内层始终失败 → 重试耗尽后原样上抛原始错误（不吞错、不改码）。
  ///
  /// 预期：错误码与消息与首次失败完全一致；调用次数为 1 次首发 + 2 次重试。
  #[tokio::test]
  async fn bubbles_original_error_when_retries_exhausted() {
    let stub = StubProvider::new_many(vec![
      Err(pre_stream(SERVER_ERROR, "boom")),
      Err(pre_stream(SERVER_ERROR, "boom")),
      Err(pre_stream(SERVER_ERROR, "boom")),
    ]);
    let policy = RetryPolicy {
      max_retries: 2,
      ..instant_policy()
    };
    let retrying = RetryingProvider::new(stub, policy);

    let model = Model::new(
      ProviderId::parse("stub").expect("stub id 合法"),
      ModelId::parse("stub-model").expect("stub model 合法"),
      "Stub Model",
      "http://stub.local",
    );
    let context = Context::default();
    let stream_options = StreamOptions::default();
    let res = retrying
      .stream(ProviderOptions {
        model: &model,
        context: &context,
        options: &stream_options,
      })
      .await;

    match res {
      Err(err) => {
        assert_eq!(err.code(), SERVER_ERROR);
        assert_eq!(err.message(), "boom");
      }
      Ok(_) => panic!("重试耗尽后应上抛错误"),
    }
    assert_eq!(retrying.inner.calls(), 3);
  }

  /// 场景：包装器自报 `is_retrying_provider() == true`，
  /// 供上层避免叠加多层重试。
  #[test]
  fn reports_itself_as_retrying_provider() {
    let stub = StubProvider::new(Ok(ok_stream()));
    let retrying = RetryingProvider::new(stub, instant_policy());

    assert!(retrying.is_retrying_provider());
  }

  /// 场景：退避期间触发取消 → 返回 CANCELLED，且重试次数不再增加。
  ///
  /// 预期：首次尝试失败进入 1h 退避，10ms 虚拟时间被取消打断；
  /// 最终错误码为 CANCELLED，调用次数保持 1。
  #[tokio::test(start_paused = true)]
  async fn cancellation_during_backoff_stops_retrying() {
    let stub = StubProvider::new(Err(pre_stream(SERVER_ERROR, "boom")));
    let policy = RetryPolicy {
      initial_backoff: Duration::from_secs(3600),
      jitter_ratio: 0.,
      ..RetryPolicy::default()
    };
    let retrying = RetryingProvider::new(stub, policy);

    let token = CancelToken::new();
    // 模拟上层在退避进行到 10ms 时触发取消
    let canceller = {
      let token = token.clone();
      tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        token.cancel();
      })
    };

    let model = Model::new(
      ProviderId::parse("stub").expect("stub id 合法"),
      ModelId::parse("stub-model").expect("stub model 合法"),
      "Stub Model",
      "http://stub.local",
    );
    let context = Context::default();
    let stream_options = StreamOptions {
      cancel: Some(token),
      ..Default::default()
    };
    let res = retrying
      .stream(ProviderOptions {
        model: &model,
        context: &context,
        options: &stream_options,
      })
      .await;

    canceller.await.expect("取消任务不应 panic");
    match res {
      Err(err) => assert_eq!(err.code(), CANCELLED),
      Ok(_) => panic!("退避期取消应返回 CANCELLED"),
    }
    assert_eq!(retrying.inner.calls(), 1, "取消后不得再发起新尝试");
  }

  /// 契约回归：InStream 阶段错误绝不触发重试（阶段门短路生效）。
  ///
  /// 预期：第一次失败即原样上抛，不发生任何重试调用。
  #[tokio::test]
  async fn in_stream_error_is_never_retried() {
    let stub = StubProvider::new(Err(in_stream(
      STREAM_INTERRUPTED,
      "mid-stream failure",
    )));
    let retrying = RetryingProvider::new(stub, instant_policy());

    let model = Model::new(
      ProviderId::parse("stub").expect("stub id 合法"),
      ModelId::parse("stub-model").expect("stub model 合法"),
      "Stub Model",
      "http://stub.local",
    );
    let context = Context::default();
    let stream_options = StreamOptions::default();
    let res = retrying
      .stream(ProviderOptions {
        model: &model,
        context: &context,
        options: &stream_options,
      })
      .await;

    match res {
      Err(err) => {
        assert_eq!(err.code(), STREAM_INTERRUPTED);
        assert_eq!(err.message(), "mid-stream failure");
      }
      Ok(_) => panic!("InStream 错误应原样上抛"),
    }
    assert_eq!(retrying.inner.calls(), 1);
  }
}
