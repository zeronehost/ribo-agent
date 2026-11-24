//! 网关静态配置。

use std::time::Duration;

use crate::retry::RetryPolicy;

/// 网关配置。
///
/// 全部字段均有默认值（见 [`Default`] 实现），可按需覆盖。
#[derive(Debug, Clone)]
pub struct GatewayConfig {
  /// 单次建流的默认超时（请求未指定超时时使用）。
  pub default_timeout: Duration,
  /// 建流失败的重试策略基线（按请求选项覆盖后生效）。
  pub retry: RetryPolicy,
  /// 熔断打开的连续失败阈值。
  pub circuit_threshold: u64,
  /// 熔断打开后的冷却时长，期满进入半开放行探测。
  pub circuit_cooldown: Duration,
  /// 全局最大在途请求数（超限直接返回 `OVERLOADED`）。
  pub max_in_flight: usize,
  /// 流空闲超时；`None` 表示不限制（详见 `ObservedStream`）。
  pub stream_idle_timeout: Option<Duration>,
}

impl Default for GatewayConfig {
  fn default() -> Self {
    Self {
      default_timeout: Duration::from_secs(600),
      retry: RetryPolicy::default(),
      circuit_threshold: 5,
      circuit_cooldown: Duration::from_secs(30),
      max_in_flight: 100,
      stream_idle_timeout: None,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// 场景：读取网关配置默认值。
  ///
  /// 预期：建流超时 600s、熔断阈值 5、冷却 30s、并发上限 100、
  /// 流空闲超时默认不限制。
  #[test]
  fn default_config_has_documented_values() {
    let config = GatewayConfig::default();

    assert_eq!(config.default_timeout, Duration::from_secs(600));
    assert_eq!(config.retry, RetryPolicy::default());
    assert_eq!(config.circuit_threshold, 5);
    assert_eq!(config.circuit_cooldown, Duration::from_secs(30));
    assert_eq!(config.max_in_flight, 100);
    assert_eq!(config.stream_idle_timeout, None);
  }
}
