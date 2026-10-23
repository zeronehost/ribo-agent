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
