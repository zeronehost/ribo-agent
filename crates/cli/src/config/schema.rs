//! 分层配置 schema：每层独立、层间零交叉，聚合层负责拼接与校验
//!（14 详细设计 §4；`query_model` 为 TOML 根级字段）。

use std::{collections::HashMap, path::PathBuf};

use serde::Deserialize;

/// provider 层：`query_model` 为 TOML 根级字段（与 harness_fallback.toml 同构），
/// 反序列化时并入本层。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ProviderSchema {
  /// 网关查询入口模型名：必须是某 provider 暴露的 `model`（mock 模式默认 mock-model）。
  pub query_model: String,
  /// 提供方列表。
  pub providers: Vec<ProviderEntry>,
  /// 跨提供方降级链。
  pub routing: RoutingSchema,
}

impl Default for ProviderSchema {
  /// 默认 = mock 模式：无 provider、query_model 用 mock 模型名。
  fn default() -> Self {
    Self {
      query_model: "mock-model".to_string(),
      providers: Vec::new(),
      routing: RoutingSchema::default(),
    }
  }
}

/// `[[providers]]` 单条目。
#[derive(Debug, Clone, Deserialize)]
pub struct ProviderEntry {
  /// 提供方 ID（用于注册与路由引用）。
  pub provider_id: String,
  /// 适配器类型：`openai-chat` | `openai-responses` | `anthropic`。
  pub adapter: String,
  /// 基础端点（适配器自动拼接路径）。
  pub base_url: String,
  /// 支持 `$ENV_NAME` 占位（解析期读取环境变量）。
  pub api_key: String,
  /// 真实模型名：同时作为网关模型 ID 与请求线 `model` 字段。
  pub model: String,
}

/// 路由段：主提供方 → 候选列表。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RoutingSchema {
  /// 跨提供方降级链。
  #[serde(default)]
  pub cross_provider: HashMap<String, Vec<String>>,
}

/// retry 层：仅暴露三个常用字段，`RetryPolicy` 其余字段取 Default。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RetrySchema {
  /// 最大重试次数（不含首次请求）。
  pub max_retries: u32,
  /// 首次重试基础退避（毫秒）。
  pub initial_backoff_ms: u64,
  /// 抖动比例。
  pub jitter_ratio: f64,
}

impl Default for RetrySchema {
  fn default() -> Self {
    let p = ra_provider::retry::RetryPolicy::default();
    Self {
      max_retries: p.max_retries,
      initial_backoff_ms: p.initial_backoff.as_millis() as u64,
      jitter_ratio: p.jitter_ratio,
    }
  }
}

/// compression 层：字段与 `ra_harness::CompactionConfig` 一一对应。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CompressionSchema {
  /// 是否启用压缩。
  pub enabled: bool,
  /// 触发比例。
  pub trigger_ratio: f64,
  /// 保留最近多少个 user 轮。
  pub keep_recent_turns: usize,
  /// 摘要模型；None 时压缩退化为纯滑窗。
  pub summarizer_model: Option<String>,
  /// 摘要最大字符数。
  pub max_summary_chars: usize,
}

impl Default for CompressionSchema {
  fn default() -> Self {
    Self {
      enabled: false,
      trigger_ratio: 0.75,
      keep_recent_turns: 4,
      summarizer_model: None,
      max_summary_chars: 2048,
    }
  }
}

/// session 层：REPL 会话行为。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SessionSchema {
  /// 初始会话键（`/new` 轮换基准）。
  pub default_key: String,
  /// `/history` 默认条数。
  pub history_lines: usize,
}

impl Default for SessionSchema {
  fn default() -> Self {
    Self {
      default_key: "default".to_string(),
      history_lines: 10,
    }
  }
}

/// tools 层。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ToolsSchema {
  /// 是否注册内置工具（calculator/echo/time + fs 六件）。
  pub builtin_enabled: bool,
  /// 内置工具文件沙箱根（fs 六件的读写边界）：**相对 harness 层 `root`
  /// 解析**，保证沙箱始终位于 root 之下；未设置时回退 `root` 本身。
  /// 绝对路径或含 `..` 逃逸组件 → 加载期 warning + 回退 `root`。
  pub fs_root: Option<PathBuf>,
}

impl Default for ToolsSchema {
  fn default() -> Self {
    Self {
      builtin_enabled: true,
      fs_root: None,
    }
  }
}

/// harness 层：存储与技能目录（概设双真值裁决：不含 compaction）。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct HarnessLayerSchema {
  /// 工作空间根目录（会话存储、日志、技能扫描与内置工具沙箱的统一
  /// 基准；默认 `.`（进程 cwd）；`--root` 参数可覆盖）。
  pub root: PathBuf,
  /// 技能扫描根（相对 `root` 解析）。
  pub skills_roots: Vec<PathBuf>,
  /// 环境段注入开关（默认 true，与 `ContextConfig::default()` 对齐）。
  pub include_environment: bool,
}

impl Default for HarnessLayerSchema {
  fn default() -> Self {
    Self {
      root: PathBuf::from("."),
      skills_roots: vec![PathBuf::from("skills")],
      include_environment: true,
    }
  }
}

/// 适配器类型白名单。
pub const ADAPTER_TYPES: [&str; 3] =
  ["openai-chat", "openai-responses", "anthropic"];

/// 单条 provider 校验：返回是否可注册，问题写入 warnings。
///
/// 1. 五字段非空；2. adapter 白名单；3. 占位值检测（整体回退由调用方处理）；
/// 4. `$ENV` 密钥展开（未设置 → 跳过该 provider）。
pub fn validate_provider_entry(
  entry: &ProviderEntry,
  warnings: &mut Vec<String>,
) -> bool {
  if entry.provider_id.trim().is_empty()
    || entry.adapter.trim().is_empty()
    || entry.base_url.trim().is_empty()
    || entry.api_key.trim().is_empty()
    || entry.model.trim().is_empty()
  {
    warnings.push(format!("provider entry incomplete: {}", entry.provider_id));
    return false;
  }
  if !ADAPTER_TYPES.contains(&entry.adapter.as_str()) {
    warnings.push(format!(
      "unsupported adapter type: {} ({})",
      entry.adapter, entry.provider_id
    ));
    return false;
  }
  // 占位值检测：整体回退 mock 的标记（调用方识别后统一处理）。
  if entry.api_key.starts_with("your-")
    || entry.base_url.contains("example.com")
  {
    warnings.push(format!(
      "placeholder config detected for {}",
      entry.provider_id
    ));
    return false;
  }
  true
}

/// 展开 `$ENV` 密钥占位；未设置 → false（跳过该 provider）。
pub fn expand_api_key(
  entry: &mut ProviderEntry,
  warnings: &mut Vec<String>,
) -> bool {
  let Some(env_name) = entry.api_key.strip_prefix('$') else {
    return true; // 非占位：直接使用
  };
  match std::env::var(env_name) {
    Ok(key) if !key.trim().is_empty() => {
      entry.api_key = key;
      true
    }
    _ => {
      warnings.push(format!(
        "api key env not set, provider skipped: {}",
        entry.provider_id
      ));
      false
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// 场景：占位值与 `$ENV` 展开识别。
  ///
  /// 预期：占位（your-*/example.com）命中检测；`$VAR` 设置成功展开、
  /// 未设置跳过 provider。
  #[test]
  fn placeholder_and_env_expansion_are_detected() {
    let mut warnings = Vec::new();
    let mut entry = ProviderEntry {
      provider_id: "p".into(),
      adapter: "openai-chat".into(),
      base_url: "https://api.example.com".into(),
      api_key: "k".into(),
      model: "m".into(),
    };
    // 占位 base_url
    assert!(!validate_provider_entry(&entry, &mut warnings));
    assert!(warnings[0].contains("placeholder config detected"));

    // 占位 api_key 前缀
    entry.base_url = "https://real-endpoint".into();
    entry.api_key = "your-key".into();
    assert!(!validate_provider_entry(&entry, &mut warnings));

    // $ENV 设置 → 展开（edition 2024 下 set_var 为 unsafe；测试进程
    // 单线程执行本测试且键名唯一，无并发读环境变量风险）
    #[allow(unsafe_code)]
    unsafe {
      std::env::set_var("RIBO_CLI_TEST_KEY", "sk-test");
    }
    entry.api_key = "$RIBO_CLI_TEST_KEY".into();
    assert!(expand_api_key(&mut entry, &mut warnings));
    assert_eq!(entry.api_key, "sk-test");

    // $ENV 未设置 → 跳过
    entry.api_key = "$RIBO_CLI_TEST_MISSING".into();
    assert!(!expand_api_key(&mut entry, &mut warnings));
    assert!(warnings.iter().any(|w| w.contains("api key env not set")));
    #[allow(unsafe_code)]
    unsafe {
      std::env::remove_var("RIBO_CLI_TEST_KEY");
    }
  }
}
