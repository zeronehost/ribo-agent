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
    let base_ns = (self.initial_backoff.as_nanos() as f64) * exp;
    let cap_ns = self.max_backoff.as_nanos() as f64;
    let capped = base_ns.min(cap_ns);

    // 抖动范围为空（jitter_ratio = 0 或封顶值为 0）时跳过随机采样——
    // rand 对空区间的 `random_range` 会 panic。
    let ratio = self.jitter_ratio.clamp(0.0, 0.99);
    let jitter_range = capped * ratio;
    let jittered = if jitter_range > 0. {
      (capped + rand::random_range(-jitter_range..jitter_range))
        .clamp(0.0, cap_ns)
    } else {
      capped
    };
    Duration::from_nanos(jittered as u64)
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
  Retry {
    /// 本次决策时已失败的尝试次数。
    attempt: u32,
    /// 重试前的等待时长（已含抖动与 retry_after 协商）。
    delay: Duration,
  },
  /// 重试次数已耗尽。
  Exhausted,
  /// 该错误不可重试。
  NotRetryable,
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::error::{
    AUTH_FAILED, RATE_LIMITED, SERVER_ERROR, TIMEOUT, in_stream, pre_stream,
  };

  /// 构造无抖动策略，便于对退避曲线做精确断言。
  fn no_jitter_policy() -> RetryPolicy {
    RetryPolicy {
      jitter_ratio: 0.,
      ..RetryPolicy::default()
    }
  }

  /// 场景：无抖动时退避曲线呈 500ms → 1s → 2s → 4s 指数增长，
  /// 且超过 `max_backoff`（默认 30s）后被封顶。
  ///
  /// 预期：attempt 0..3 依次翻倍；attempt=10 理论值 512s，应精确等于 30s。
  #[test]
  fn delay_grows_exponentially_and_is_capped() {
    let policy = no_jitter_policy();

    assert_eq!(policy.delay(0), Duration::from_millis(500));
    assert_eq!(policy.delay(1), Duration::from_secs(1));
    assert_eq!(policy.delay(2), Duration::from_secs(2));
    assert_eq!(policy.delay(3), Duration::from_secs(4));
    assert_eq!(policy.delay(10), policy.max_backoff);
  }

  /// 场景：抖动边界——连续采样 100 次，结果均落在
  /// `±jitter_ratio` 区间内且恒为非负。
  ///
  /// 预期：base 恒为 1s、抖动比例 25% 时，全部结果落在
  /// [750ms, 1250ms] 内；下界 0.75s > 0 同时验证了 `.max(0.)` 不被触发。
  #[test]
  fn delay_jitter_stays_within_ratio_bounds() {
    let policy = RetryPolicy {
      // multiplier=1 使 base 恒为 1s，且远小于 max_backoff，不触发封顶
      initial_backoff: Duration::from_secs(1),
      multiplier: 1.,
      jitter_ratio: 0.25,
      ..RetryPolicy::default()
    };

    for _ in 0..100 {
      let d = policy.delay(0);
      let ns = d.as_nanos();
      assert!(ns >= 750_000_000, "低于抖动下界: {d:?}");
      assert!(ns <= 1_250_000_000, "超出抖动上界: {d:?}");
    }
  }

  /// 场景：不可重试码（AUTH_FAILED）直接判定为 NotRetryable。
  #[test]
  fn decide_not_retryable_for_auth_failed() {
    let policy = RetryPolicy::default();
    let err = pre_stream(AUTH_FAILED, "bad key");

    assert_eq!(policy.decide(&err, 0), RetryDecision::NotRetryable);
  }

  /// 契约回归：InStream 阶段错误被阶段门短路，绝不触发重试。
  ///
  /// TIMEOUT 本身属于可重试码，但流内阶段重试会与已产出内容重复，
  /// 因此应判定为 NotRetryable。
  #[test]
  fn decide_in_stream_error_is_never_retryable() {
    let policy = RetryPolicy::default();
    let err = in_stream(TIMEOUT, "mid-stream failure");

    assert_eq!(policy.decide(&err, 0), RetryDecision::NotRetryable);
  }

  /// 场景：attempt 达到 max_retries 后判定为 Exhausted。
  #[test]
  fn decide_exhausted_after_max_retries() {
    let policy = RetryPolicy::default();
    let err = pre_stream(SERVER_ERROR, "boom");

    assert_eq!(
      policy.decide(&err, 3),
      RetryDecision::Exhausted,
      "attempt == max_retries 时应耗尽"
    );
  }

  /// 场景：服务端建议的等待时长（retry_after_ms=60s）超过
  /// max_backoff（默认 30s）时，放弃重试。
  #[test]
  fn decide_exhausted_when_retry_after_exceeds_max_backoff() {
    let policy = RetryPolicy::default();
    let err = pre_stream(RATE_LIMITED, "slow down")
      .with_context("retry_after_ms", "60000");

    assert_eq!(policy.decide(&err, 0), RetryDecision::Exhausted);
  }

  /// 场景：retry_after 未超限时按 max(策略退避, retry_after) 等待。
  ///
  /// 预期：retry_after=100ms 小于退避 500ms → 用退避；
  /// retry_after=2s 大于退避 500ms → 用服务端建议。
  #[test]
  fn decide_retry_delay_takes_max_of_backoff_and_retry_after() {
    let policy = no_jitter_policy();

    let small =
      pre_stream(RATE_LIMITED, "x").with_context("retry_after_ms", "100");
    assert_eq!(
      policy.decide(&small, 0),
      RetryDecision::Retry {
        attempt: 0,
        delay: Duration::from_millis(500)
      }
    );

    let large =
      pre_stream(RATE_LIMITED, "x").with_context("retry_after_ms", "2000");
    assert_eq!(
      policy.decide(&large, 0),
      RetryDecision::Retry {
        attempt: 0,
        delay: Duration::from_secs(2)
      }
    );
  }

  /// 场景：effective() 按请求级选项覆盖策略参数。
  ///
  /// 预期：请求未指定的字段保留策略原值；指定了 max_retries /
  /// max_backoff 时覆盖生效。
  #[test]
  fn effective_overrides_only_present_fields() {
    let policy = RetryPolicy::default();

    // 请求未携带覆盖项 → 完整保留策略原值
    let kept = policy.effective(&StreamOptions::default());
    assert_eq!(kept, policy);

    // 请求显式覆盖 max_retries / max_backoff
    let options = StreamOptions {
      retry: crate::options::RetryOptions {
        max_retries: Some(7),
        max_backoff: Some(Duration::from_secs(5)),
        ..Default::default()
      },
      ..Default::default()
    };
    let applied = policy.effective(&options);
    assert_eq!(applied.max_retries, 7);
    assert_eq!(applied.max_backoff, Duration::from_secs(5));
    assert_eq!(applied.initial_backoff, policy.initial_backoff);
  }
}
