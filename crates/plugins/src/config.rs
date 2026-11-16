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
  /// 单工具执行超时的宿主侧上限（ai_0018 §4.4 / B2.5）：工具自述
  /// `timeout_hint` 与插件 ACL ceiling 合并后再与本值取 min（只收紧）。
  pub tool_timeout_ceiling: std::time::Duration,
  /// 是否启用 fuel（指令数中断）。Step B 恒 `false`；Step C 启用需改
  /// 配置并**重启进程**——`Engine` 配置在 `new()` 时固化，`PluginRuntime`
  /// 为 `OnceLock` 单例不可重置（18_ §5.2 #5）。
  pub enable_fuel: bool,
  /// 每次 tool 执行注入的 fuel 预算（指令数权重）。仅 `enable_fuel`
  /// 为真时生效；耗尽触发 `Trap::OutOfFuel`，由结构化分类器归 Timeout
  /// （与 epoch 中断同语义）。
  pub fuel_limit: u64,
  /// ModuleCache 容量上限（按 wasm 内容哈希计）。超限按 LRU 淘汰最久
  /// 未使用的 Module，防止恶意插件反复换字节导致无界内存增长。
  pub module_cache_capacity: usize,
}

impl Default for PluginRuntimeConfig {
  /// 进程级默认值（18_ §0.3 v0.8.1 P1-3：实际实现，取代注释占位）。
  fn default() -> Self {
    Self {
      memory_limit_bytes: 32 << 20, // 32MB 线性内存
      storage_quota_bytes: 1 << 20, // 1MB 单插件 storage
      log_burst: 200,               // 窗口内最多 200 条
      log_rate_per_sec: 50,         // 50 条/秒
      tool_timeout_ceiling: std::time::Duration::from_secs(60),
      enable_fuel: false, // Step B 不启用 fuel
      fuel_limit: 1_000_000_000,
      module_cache_capacity: 64,
    }
  }
}

impl PluginRuntimeConfig {
  /// 进程启动配置：默认值 + 环境变量覆写（仅在 [`PluginRuntime::start`]
  /// 读取一次，`OnceLock` 后不再变化）。
  ///
  /// 支持的环境变量（仅进程启动期生效，运行期修改无效）：
  ///
  /// - `RIBO_PLUGINS_ENABLE_FUEL`：`1` / `true` 启用 fuel；
  /// - `RIBO_PLUGINS_FUEL_LIMIT`：正整数 fuel 预算；
  /// - `RIBO_PLUGINS_MODULE_CACHE_CAPACITY`：正整数缓存容量。
  ///
  /// 非法数值一律忽略并保留默认值——进程级基础设施不因环境变量拼写
  /// 错误而启动失败（启动致命仅保留给插件 wasm / ACL / 权威清单错误）。
  ///
  /// [`PluginRuntime::start`]: crate::wasm::runtime::PluginRuntime::start
  pub fn from_env() -> Self {
    let mut cfg = Self::default();
    if let Ok(v) = std::env::var("RIBO_PLUGINS_ENABLE_FUEL") {
      cfg.enable_fuel = matches!(v.as_str(), "1" | "true" | "TRUE" | "True");
    }
    if let Ok(v) = std::env::var("RIBO_PLUGINS_FUEL_LIMIT")
      && let Ok(n) = v.parse::<u64>()
      && n > 0
    {
      cfg.fuel_limit = n;
    }
    if let Ok(v) = std::env::var("RIBO_PLUGINS_MODULE_CACHE_CAPACITY")
      && let Ok(n) = v.parse::<usize>()
      && n > 0
    {
      cfg.module_cache_capacity = n;
    }
    cfg
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
    assert_eq!(cfg.tool_timeout_ceiling, std::time::Duration::from_secs(60));
    assert!(!cfg.enable_fuel);
    assert_eq!(cfg.fuel_limit, 1_000_000_000);
    assert_eq!(cfg.module_cache_capacity, 64);
  }
}
