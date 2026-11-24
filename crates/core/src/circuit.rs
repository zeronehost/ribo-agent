//! 通用熔断器：Closed → Open → HalfOpen 三态自动机。
//!
//! 连续失败达到阈值后打开熔断（拒绝全部请求），冷却期满后进入
//! 半开状态放行单个探测请求，探测成功即闭合、失败则重新打开。
//! 状态用原子变量实现，无锁，可在并发请求路径上直接调用。
//!
//! 典型调用方按"受保护资源各持一个实例"使用：网关为每个模型
//! 提供方持有一个（见 `ra-provider::gateway`），工具管理器为整个
//! 工具面持有一个（见 `ra-tools::manager`）。

use std::{
  sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
  time::Duration,
};

/// 状态码：闭合（正常放行）。
const STATE_CLOSED: u8 = 0;
/// 状态码：打开（拒绝全部请求）。
const STATE_OPEN: u8 = 1;
/// 状态码：半开（放行单个探测请求）。
const STATE_HALF_OPEN: u8 = 2;

/// 熔断器三态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
  /// 闭合：正常放行全部请求。
  Closed,
  /// 打开：拒绝全部请求，等待冷却期满。
  Open,
  /// 半开：冷却期满，仅放行单个探测请求。
  HalfOpen,
}

/// 无锁熔断器（每个受保护资源一个实例，如单个提供方或整个工具面）。
///
/// 用原子变量维护状态与失败计数；时间均以实例创建时刻（`base`）
/// 为基准的相对毫秒数表达，避免依赖系统墙钟。
///
/// # Examples
///
/// ```
/// use std::time::Duration;
///
/// use ra_core::circuit::{Admission, CircuitBreaker, CircuitState};
///
/// # fn main() {
/// // 连续失败 2 次即打开，冷却 30 秒
/// let breaker = CircuitBreaker::new(2, Duration::from_secs(30));
/// assert_eq!(breaker.admit(), Admission::Allow);
///
/// breaker.record_failure();
/// assert_eq!(breaker.admit(), Admission::Allow, "未达阈值继续放行");
/// breaker.record_failure();
/// assert_eq!(breaker.state(), CircuitState::Open);
/// assert_eq!(breaker.admit(), Admission::Reject, "冷却期内拒绝全部请求");
/// # }
/// ```
pub struct CircuitBreaker {
  /// 连续失败计数（观测到成功即清零）。
  failures: AtomicU64,
  /// 当前状态码（取值为 `STATE_*` 常量）。
  state: AtomicU8,
  /// 冷却截止时刻（相对 `base` 的毫秒数）；到达它即可进入半开。
  deadline_ms: AtomicU64,
  /// 是否已有探测请求在途（半开状态同时只放行一个）。
  probe_in_flight: AtomicBool,
  /// 打开熔断的连续失败阈值。
  threshold: u64,
  /// 打开后的冷却时长（毫秒）。
  cooldown_ms: u64,
  /// 计时基准：实例创建时刻。
  base: tokio::time::Instant,
}

impl CircuitBreaker {
  /// 创建闭合状态的熔断器。
  ///
  /// # 参数
  ///
  /// - `threshold`：连续失败达到该值即打开熔断；
  /// - `cooldown`：打开后的冷却时长，期满进入半开。
  pub fn new(threshold: u64, cooldown: Duration) -> Self {
    Self {
      failures: AtomicU64::new(0),
      state: AtomicU8::new(STATE_CLOSED),
      deadline_ms: AtomicU64::new(0),
      probe_in_flight: AtomicBool::new(false),
      threshold,
      cooldown_ms: cooldown.as_millis() as u64,
      base: tokio::time::Instant::now(),
    }
  }

  /// 取当前时刻（相对 `base` 的毫秒数）。
  fn now_ms(&self) -> u64 {
    self.base.elapsed().as_millis() as u64
  }

  /// 读取当前状态；打开且冷却期满时原子迁移为半开。
  ///
  /// 用 `compare_exchange` 保证并发调用下只有一个调用者完成
  /// Open → HalfOpen 迁移，其余调用者看到迁移后的状态。
  pub fn state(&self) -> CircuitState {
    // ===== 惰性半开迁移 =====
    // 三个条件短路检查：状态为 Open、冷却已期满，再 CAS 抢占迁移权。
    if self.state.load(Ordering::Acquire) == STATE_OPEN
      && self.now_ms() >= self.deadline_ms.load(Ordering::Acquire)
      && self
        .state
        .compare_exchange(
          STATE_OPEN,
          STATE_HALF_OPEN,
          Ordering::AcqRel,
          Ordering::Acquire,
        )
        .is_ok()
    {
      tracing::debug!("circuit half-open after cooldown");
    }

    match self.state.load(Ordering::Acquire) {
      STATE_OPEN => CircuitState::Open,
      STATE_HALF_OPEN => CircuitState::HalfOpen,
      _ => CircuitState::Closed,
    }
  }

  /// 请求准入判定。
  ///
  /// 闭合放行；打开拒绝；半开状态下用 CAS 抢占 `probe_in_flight`，
  /// 抢到的调用者获得唯一探测资格，其余拒绝。
  pub fn admit(&self) -> Admission {
    match self.state() {
      CircuitState::Closed => Admission::Allow,
      CircuitState::Open => Admission::Reject,
      CircuitState::HalfOpen => {
        if self
          .probe_in_flight
          .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
          .is_ok()
        {
          tracing::debug!("circuit probe admitted");
          Admission::AllowProbe
        } else {
          Admission::Reject
        }
      }
    }
  }

  /// 观测到一次成功：清零失败计数并闭合熔断、复位探测标志。
  pub fn record_success(&self) {
    self.failures.store(0, Ordering::Release);
    self.state.store(STATE_CLOSED, Ordering::Release);
    self.probe_in_flight.store(false, Ordering::Release);
    tracing::debug!("circuit closed after success");
  }

  /// 观测到一次失败：累计计数；半开探测失败或达到阈值即打开熔断。
  ///
  /// 半开状态下失败立即重新打开（探测失败不允许继续累积），
  /// 并以当前时刻为熔断打开点重设冷却截止。
  pub fn record_failure(&self) {
    let failures = self.failures.fetch_add(1, Ordering::AcqRel) + 1;
    let prev = self.state.load(Ordering::Acquire);
    // 只有「闭合态达到阈值」或「半开探测失败」才迁移到 Open 并重设冷却；
    // Open 期间的迟到失败只累计计数，避免反复延后冷却起点（R9）。
    if prev == STATE_HALF_OPEN
      || (prev == STATE_CLOSED && failures >= self.threshold)
    {
      // 截止点 = 当前时刻 + 冷却时长；只存"打开时刻"会让冷却判断恒真，
      // 熔断会立即进入半开，冷却配置形同虚设。
      let deadline = self.now_ms().saturating_add(self.cooldown_ms);
      self.deadline_ms.store(deadline, Ordering::Release);
      self.state.store(STATE_OPEN, Ordering::Release);
      self.probe_in_flight.store(false, Ordering::Release);
      tracing::warn!(
        failures,
        cooldown_ms = self.cooldown_ms,
        "circuit opened"
      );
    }
  }

  /// 归还半开探测资格（请求在探测回报前被取消时调用）。
  ///
  /// 仅复位 `probe_in_flight`，不改变熔断状态与失败计数——取消既不是
  /// 上游成功也不是失败，熔断应保持原状态等待下一次探测。
  pub fn abort_probe(&self) {
    self.probe_in_flight.store(false, Ordering::Release);
    tracing::debug!("circuit probe released after cancellation");
  }
}

/// 准入结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
  /// 放行（普通请求）。
  Allow,
  /// 放行（半开探测，需在终结时回报结果）。
  AllowProbe,
  /// 拒绝（熔断打开）。
  Reject,
}

#[cfg(test)]
mod tests {
  use std::time::Duration;

  use super::*;

  /// 场景：连续失败达到阈值后打开熔断，冷却期内一律拒绝。
  ///
  /// 预期：两次失败后状态为 Open；`admit()` 返回 Reject；
  /// 冷却期满后 `state()` 惰性迁移为 HalfOpen。
  #[tokio::test(start_paused = true)]
  async fn opens_after_threshold_and_rejects_during_cooldown() {
    let breaker = CircuitBreaker::new(2, Duration::from_secs(30));

    assert_eq!(breaker.admit(), Admission::Allow);
    breaker.record_failure();
    assert_eq!(breaker.admit(), Admission::Allow, "未达阈值应继续放行");

    breaker.record_failure();
    assert_eq!(breaker.state(), CircuitState::Open);
    assert_eq!(breaker.admit(), Admission::Reject);

    tokio::time::advance(Duration::from_secs(30)).await;
    assert_eq!(breaker.state(), CircuitState::HalfOpen);
  }

  /// 场景：半开状态同时只允许一个探测请求。
  ///
  /// 预期：首个 `admit()` 拿到 AllowProbe，第二个在探测回报前被拒绝。
  #[tokio::test(start_paused = true)]
  async fn half_open_allows_only_one_probe() {
    let breaker = CircuitBreaker::new(1, Duration::from_secs(30));
    breaker.record_failure();
    tokio::time::advance(Duration::from_secs(30)).await;

    assert_eq!(breaker.admit(), Admission::AllowProbe);
    assert_eq!(breaker.admit(), Admission::Reject, "探测资格已被抢占");
  }

  /// 场景：半开探测成功 → 熔断闭合且失败计数清零。
  ///
  /// 预期：`record_success()` 后恢复 Closed，随后重新按阈值累计失败。
  #[tokio::test(start_paused = true)]
  async fn probe_success_closes_and_resets_failures() {
    let breaker = CircuitBreaker::new(2, Duration::from_secs(30));
    breaker.record_failure();
    breaker.record_failure();
    tokio::time::advance(Duration::from_secs(30)).await;
    assert_eq!(breaker.admit(), Admission::AllowProbe);

    breaker.record_success();
    assert_eq!(breaker.state(), CircuitState::Closed);

    // 计数已清零：一次失败不足以重新打开（阈值为 2）。
    breaker.record_failure();
    assert_eq!(breaker.state(), CircuitState::Closed);
  }

  /// 场景：半开探测失败 → 立即重新打开并重设冷却。
  ///
  /// 预期：探测失败后状态为 Open，冷却期内 `admit()` 返回 Reject。
  #[tokio::test(start_paused = true)]
  async fn probe_failure_reopens_breaker() {
    let breaker = CircuitBreaker::new(5, Duration::from_secs(30));
    breaker.record_failure();
    breaker.record_failure();
    breaker.record_failure();
    breaker.record_failure();
    breaker.record_failure();
    tokio::time::advance(Duration::from_secs(30)).await;
    assert_eq!(breaker.admit(), Admission::AllowProbe);

    breaker.record_failure();
    assert_eq!(breaker.state(), CircuitState::Open);
    assert_eq!(breaker.admit(), Admission::Reject);
  }

  /// 场景：熔断打开后迟到的失败不应延长冷却（对应审查报告 R9）。
  ///
  /// 预期：t0 打开（冷却 30s），t10 的迟到失败不改变截止点，
  /// t31 时应已进入 HalfOpen。
  #[tokio::test(start_paused = true)]
  async fn late_failure_does_not_extend_cooldown() {
    let breaker = CircuitBreaker::new(1, Duration::from_secs(30));
    breaker.record_failure();

    tokio::time::advance(Duration::from_secs(10)).await;
    breaker.record_failure(); // 迟到的失败

    tokio::time::advance(Duration::from_secs(21)).await;
    assert_eq!(breaker.state(), CircuitState::HalfOpen);
  }
}
