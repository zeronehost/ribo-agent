//! 进程级运行参数（设计文档 18_ §0.3，v0.8 #15）。
//!
//! 单一配置结构消除硬编码：进程级默认由 [`PluginRuntimeConfig::default`]
//! 给出，插件级覆写（[`PluginAcl`](crate::acl::PluginAcl) /
//! `ToolManifest.memory_limit_bytes`）**只能收紧不能放大**（宿主侧一律
//! 取 `min(覆写, 进程默认)`）。全链路只有一份进程默认配置——由
//! [`PluginRuntime`](crate::wasm::runtime::PluginRuntime) 单例持有。

/// 进程级运行参数：内存 / storage / 日志限流 / fuel 开关。
#[derive(Debug, Clone)]
pub struct PluginRuntimeConfig {
  /// 线性内存上限；默认 32MB。`ToolManifest.memory_limit_bytes` 与
  /// `PluginAcl.memory_limit_bytes` 可覆写（需 <= 此上限）。
  pub memory_limit_bytes: usize,
  /// 单插件 storage 容量；默认 1MB。`PluginAcl.storage_quota_bytes`
  /// 可覆写（需 <= 此上限）。
  pub storage_quota_bytes: usize,
  /// log 限流：窗口内最大条数（burst）。
  pub log_burst: u64,
  /// log 限流：每秒采样放行速率。
  pub log_rate_per_sec: u64,
  /// 是否启用 fuel（指令数中断）。Step B 恒 `false`；Step C 启用需改
  /// 配置并**重启进程**——`Engine` 配置在 `new()` 时固化，`PluginRuntime`
  /// 为 `OnceLock` 单例不可重置（18_ §5.2 #5）。
  pub enable_fuel: bool,
}

impl Default for PluginRuntimeConfig {
  /// 进程级默认值（18_ §0.3 v0.8.1 P1-3：实际实现，取代注释占位）。
  fn default() -> Self {
    Self {
      memory_limit_bytes: 32 << 20, // 32MB 线性内存
      storage_quota_bytes: 1 << 20, // 1MB 单插件 storage
      log_burst: 200,               // 窗口内最多 200 条
      log_rate_per_sec: 50,         // 50 条/秒
      enable_fuel: false,           // Step B 不启用 fuel
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// 场景：进程默认配置与设计文档 §0.3 的约定值一致。
  #[test]
  fn default_values_match_design_contract() {
    let cfg = PluginRuntimeConfig::default();
    assert_eq!(cfg.memory_limit_bytes, 32 << 20);
    assert_eq!(cfg.storage_quota_bytes, 1 << 20);
    assert_eq!(cfg.log_burst, 200);
    assert_eq!(cfg.log_rate_per_sec, 50);
    assert!(!cfg.enable_fuel);
  }
}
