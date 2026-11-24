//! 配置来源标记：字段级记录每个配置值的来源域。
//!
//! 三层信任域模型要求合并后的配置可追溯——`ribo config --show-origin` 能
//! 显示每个字段"来自哪一层"。本模块以**并行元数据**形式记录来源，不改动
//! 现有 7 个 schema struct，从而对下游装配（`assemble.rs`）零侵入。
//!
//! # 设计取舍
//!
//! - **字段名作为键**：以配置字段路径（如 `harness.root`、`retry.max_retries`）
//!   作为键，而非 schema 字段引用——避免为 7 个 struct 的每个字段加包装类型。
//! - **缺省即 Default**：未记录的字段其来源视为 [`ConfigOrigin::Default`]，
//!   因此零配置场景下所有字段天然显示 `default`，无需显式写入每个键。

use std::collections::BTreeMap;

/// 配置字段的来源域。
///
/// # Examples
///
/// ```
/// use ra_cli::config::origin::ConfigOrigin;
///
/// assert_eq!(ConfigOrigin::Default.to_string(), "default");
/// assert_eq!(ConfigOrigin::Global.to_string(), "global");
/// assert_eq!(ConfigOrigin::Cli.to_string(), "cli");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConfigOrigin {
  /// 内建默认值（字段未在任何配置文件中出现）。
  Default,
  /// 全局域：平台默认配置目录 + `/ribo` 下的 `settings.toml`。
  Global,
  /// 项目域：`ribo.toml`（或 `--config` 指定的文件）。
  Project,
  /// 命令行参数覆盖（`--root` 等）。
  Cli,
}

impl ConfigOrigin {
  /// 来源域的稳定小写名称，用于 `--show-origin` 输出。
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Default => "default",
      Self::Global => "global",
      Self::Project => "project",
      Self::Cli => "cli",
    }
  }
}

impl core::fmt::Display for ConfigOrigin {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    f.write_str(self.as_str())
  }
}

/// 字段级来源映射：配置字段路径 → 来源域。
///
/// # Examples
///
/// ```
/// use ra_cli::config::origin::{ConfigOrigin, ConfigOrigins};
///
/// let mut origins = ConfigOrigins::new();
/// origins.set("retry.max_retries", ConfigOrigin::Global);
/// origins.set("harness.root", ConfigOrigin::Cli);
/// assert_eq!(origins.get("retry.max_retries"), ConfigOrigin::Global);
/// // 未记录的字段视为默认值来源
/// assert_eq!(origins.get("session.default_key"), ConfigOrigin::Default);
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigOrigins {
  fields: BTreeMap<String, ConfigOrigin>,
}

impl ConfigOrigins {
  /// 创建空映射（所有字段默认来源为 [`ConfigOrigin::Default`]）。
  pub fn new() -> Self {
    Self::default()
  }

  /// 记录字段的来源域；同键以新值覆盖。
  pub fn set(&mut self, field: impl Into<String>, origin: ConfigOrigin) {
    self.fields.insert(field.into(), origin);
  }

  /// 查询字段来源；未记录时返回 [`ConfigOrigin::Default`]。
  pub fn get(&self, field: &str) -> ConfigOrigin {
    self
      .fields
      .get(field)
      .copied()
      .unwrap_or(ConfigOrigin::Default)
  }

  /// 是否有任何字段被显式记录（零配置为 `false`）。
  pub fn is_empty(&self) -> bool {
    self.fields.is_empty()
  }

  /// 遍历所有已记录字段（按字段名字典序）。
  pub fn iter(&self) -> impl Iterator<Item = (&str, ConfigOrigin)> {
    self.fields.iter().map(|(k, v)| (k.as_str(), *v))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// 场景：来源域的字符串表示。
  /// 预期：四种来源分别渲染为稳定小写名。
  #[test]
  fn origin_display_is_lowercase_stable() {
    assert_eq!(ConfigOrigin::Default.to_string(), "default");
    assert_eq!(ConfigOrigin::Global.to_string(), "global");
    assert_eq!(ConfigOrigin::Project.to_string(), "project");
    assert_eq!(ConfigOrigin::Cli.to_string(), "cli");
  }

  /// 场景：来源映射的读写与缺省。
  /// 预期：已设置字段返回对应来源，未设置字段返回 Default。
  #[test]
  fn origins_set_get_and_default() {
    let mut origins = ConfigOrigins::new();
    assert!(origins.is_empty());
    assert_eq!(origins.get("retry.max_retries"), ConfigOrigin::Default);

    origins.set("retry.max_retries", ConfigOrigin::Global);
    origins.set("harness.root", ConfigOrigin::Cli);
    assert!(!origins.is_empty());
    assert_eq!(origins.get("retry.max_retries"), ConfigOrigin::Global);
    assert_eq!(origins.get("harness.root"), ConfigOrigin::Cli);

    // 同键覆盖
    origins.set("harness.root", ConfigOrigin::Project);
    assert_eq!(origins.get("harness.root"), ConfigOrigin::Project);
  }

  /// 场景：遍历按字段名字典序。
  /// 预期：iter 输出有序，便于 show-origin 稳定展示。
  #[test]
  fn origins_iter_is_sorted() {
    let mut origins = ConfigOrigins::new();
    origins.set("z.field", ConfigOrigin::Global);
    origins.set("a.field", ConfigOrigin::Cli);
    let names: Vec<_> = origins.iter().map(|(k, _)| k).collect();
    assert_eq!(names, vec!["a.field", "z.field"]);
  }
}
