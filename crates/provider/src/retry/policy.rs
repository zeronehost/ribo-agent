//! 重试策略：指数退避曲线计算与按错误判定是否重试。

use std::time::Duration;

use ra_core::error::RiboError;

use super::backoff::retry_after_ms;
use crate::{options::StreamOptions, traits::ProviderErrorExt};

/// 指数退避重试策略。
///
/// 等待时长按 `initial_backoff * multiplier^attempt` 增长、以
/// `max_backoff` 封顶，并叠加 ±`jitter_ratio` 比例的随机抖动，
/// 避免故障恢复后的重试风暴。
#[derive(Debug, Clone, PartialEq)]
pub struct RetryPolicy {
  /// 首次重试的基础退避时长。
  pub initial_backoff: Duration,
  /// 单次退避等待的上限。
  pub max_backoff: Duration,
  /// 退避乘数（指数底数）。
  pub multiplier: f64,
  /// 最大重试次数（不含首次请求）。
  pub max_retries: u32,
  /// 抖动比例，最终延迟在计算值附近按该比例随机偏移。
  pub jitter_ratio: f64,
}

impl Default for RetryPolicy {
  fn default() -> Self {
    Self {
      initial_backoff: Duration::from_millis(500),
      max_backoff: Duration::from_secs(30),
      multiplier: 2.,
      max_retries: 3,
      jitter_ratio: 0.25,
    }
  }
}
impl RetryPolicy {
  /// 计算第 `attempt` 次重试前的等待时长。
  ///
  /// 算法步骤：
  ///
  /// 1. 指数增长：`initial_backoff * multiplier^attempt`；
  /// 2. 以 `max_backoff` 封顶；
  /// 3. 叠加 ±`jitter_ratio` 比例的随机抖动后返回。
  ///
  /// # 参数
  ///
  /// - `attempt`：当前已失败的尝试次数（从 0 开始）。
  ///
  /// # 返回
  ///
  /// 本次重试前的等待时长。
  pub fn delay(&self, attempt: u32) -> Duration {
    let exp = self.multiplier.powi(attempt as i32);
    let base = (self.initial_backoff.as_nanos() as f64) * exp;
    let capped = base.min(self.max_backoff.as_nanos() as f64);

    let jitter_range = capped * self.jitter_ratio;
    let jitter = rand::random_range(-jitter_range..jitter_range);
    let final_ns = (capped + jitter).max(0.) as u64;
    Duration::from_nanos(final_ns)
  }

  /// 按错误与已尝试次数判定重试决策。
  ///
  /// 判定顺序：
  ///
  /// 1. 错误不可重试（含流内阶段）→ [`RetryDecision::NotRetryable`]；
  /// 2. 已达最大重试次数 → [`RetryDecision::Exhausted`]；
  /// 3. 服务端建议的 `retry_after` 超过 `max_backoff` → 放弃重试；
  /// 4. 否则返回 [`RetryDecision::Retry`]，等待时长取「策略退避」与
  ///    「服务端建议」的较大值，再以 `max_backoff` 封顶。
  ///
  /// # 参数
  ///
  /// - `err`：触发本次判定的错误。
  /// - `attempt`：当前已失败的尝试次数。
  pub fn decide(&self, err: &RiboError, attempt: u32) -> RetryDecision {
    if !err.is_retryable() {
      return RetryDecision::NotRetryable;
    }
    if attempt >= self.max_retries {
      return RetryDecision::Exhausted;
    }

    let retry_after = retry_after_ms(err);

    if let Some(ra) = retry_after
      && ra > self.max_backoff
    {
      return RetryDecision::Exhausted;
    }

    let base = self.delay(attempt);
    let retry_after = retry_after.unwrap_or(Duration::ZERO);
    let delay = base.max(retry_after).min(self.max_backoff);
    RetryDecision::Retry { attempt, delay }
  }

  /// 按请求级选项覆盖策略参数，得到本次调用生效的策略副本。
  ///
  /// 仅 `options.retry.max_retries` / `max_backoff` 有值时才覆盖
  /// 对应字段，其余字段沿用当前策略的取值。
  pub fn effective(&self, options: &StreamOptions) -> RetryPolicy {
    let mut res = self.clone();

    if let Some(v) = options.retry.max_retries {
      res.max_retries = v;
    }
    if let Some(v) = options.retry.max_backoff {
      res.max_backoff = v;
    }
    res
  }
}

/// 重试决策。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryDecision {
  /// 继续重试，等待 `delay` 后发起第 `attempt + 1` 次尝试。
  Retry { attempt: u32, delay: Duration },
  /// 重试次数已耗尽。
  Exhausted,
  /// 该错误不可重试。
  NotRetryable,
}
