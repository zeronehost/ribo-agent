//! Harness 配置：会话存储目录、压缩与上下文装配参数。

use std::path::PathBuf;

use ra_provider::ids::ModelId;
use serde::{Deserialize, Serialize};

/// 会话容器总配置。
#[derive(Debug, Clone, PartialEq)]
pub struct HarnessConfig {
  /// 数据根目录；会话文件位于 `data_dir/sessions/`。
  pub data_dir: PathBuf,
  /// 压缩参数。
  pub compaction: CompactionConfig,
  /// 上下文装配参数。
  pub context: ContextConfig,
  /// 技能扫描根目录（可多个，按顺序加载）。
  pub skills_roots: Vec<PathBuf>,
}

impl HarnessConfig {
  /// 以数据根目录创建默认配置。
  pub fn new(data_dir: impl Into<PathBuf>) -> Self {
    Self {
      data_dir: data_dir.into(),
      compaction: CompactionConfig::default(),
      context: ContextConfig::default(),
      skills_roots: Vec::new(),
    }
  }

  /// 会话文件目录（`data_dir/sessions`）。
  ///
  /// 该值即 [`JsonlSessionStore::new`](crate::session_store::JsonlSessionStore::new)
  /// 的入参；`JsonlSessionStore` 不再自行拼接 `sessions` 后缀。
  pub fn sessions_dir(&self) -> PathBuf {
    self.data_dir.join("sessions")
  }
}

/// 上下文装配配置。
#[derive(Debug, Clone, PartialEq)]
pub struct ContextConfig {
  /// 压缩配置（装配时以本字段为运行期真值）。
  pub compaction: CompactionConfig,
  /// 是否在系统提示中注入环境段（工作目录 / 平台）。
  pub include_environment: bool,
}

impl Default for ContextConfig {
  fn default() -> Self {
    Self {
      compaction: CompactionConfig::default(),
      // 环境段默认开启：与 `SystemPromptBuilder::default()` 保持一致
      include_environment: true,
    }
  }
}

/// 压缩配置。
///
/// 默认**关闭**：阈值触发后若未配置摘要模型会静默滑窗丢弃历史，
/// 属高影响行为，必须由应用显式开启。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionConfig {
  /// 是否启用压缩。
  pub enabled: bool,
  /// 触发比例：估算 token 超过 `context_window * trigger_ratio` 时触发。
  pub trigger_ratio: f64,
  /// 保留最近多少个 user 轮（含该轮的全部助手/工具消息）。
  pub keep_recent_turns: usize,
  /// 摘要模型；`None` 时压缩退化为纯滑窗（不调用 LLM）。
  pub summarizer_model: Option<ModelId>,
  /// 摘要最大字符数（超出按字符截断，不按 token）。
  pub max_summary_chars: usize,
}

impl Default for CompactionConfig {
  fn default() -> Self {
    Self {
      // 保守默认：未显式开启绝不丢弃历史
      enabled: false,
      trigger_ratio: 0.75,
      keep_recent_turns: 4,
      summarizer_model: None,
      max_summary_chars: 2048,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// 默认 `CompactionConfig` 必须关闭压缩，避免静默丢历史。
  #[test]
  fn compaction_default_disabled() {
    let config = CompactionConfig::default();
    assert!(!config.enabled);
    assert_eq!(config.trigger_ratio, 0.75);
    assert_eq!(config.keep_recent_turns, 4);
    assert!(config.summarizer_model.is_none());
    assert_eq!(config.max_summary_chars, 2048);
  }

  /// 默认 `ContextConfig` 的环境段开启、压缩配置默认。
  #[test]
  fn context_default_env_enabled() {
    let config = ContextConfig::default();
    assert!(config.include_environment);
    assert!(!config.compaction.enabled);
  }

  /// `HarnessConfig` 默认值与 `sessions_dir` 拼接语义。
  #[test]
  fn harness_default_sessions_dir() {
    let config = HarnessConfig::new("/tmp/ribo-data");
    assert_eq!(config.data_dir, PathBuf::from("/tmp/ribo-data"));
    assert_eq!(
      config.sessions_dir(),
      PathBuf::from("/tmp/ribo-data/sessions")
    );
    assert!(!config.compaction.enabled);
    assert!(config.skills_roots.is_empty());
  }
}
