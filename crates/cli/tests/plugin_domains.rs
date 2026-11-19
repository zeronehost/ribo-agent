//! 插件域（Plugin Domain）装配集成测试（19_ Step 3）。
//!
//! 覆盖域选择器致命校验、无域时行为不变、[plugins] 与域共存解析。

use std::path::Path;

use ra_cli::config::assemble::{self, assemble_with_steps};
use ra_cli::config::{
  ConfigMode, LoadOutcome, PluginDomainKind, PluginDomainSchema, PluginsSchema,
};

/// 构造仅含 plugins 层的 LoadOutcome（mock 模式，其余取默认）。
fn outcome_with(root: &Path, plugins: PluginsSchema) -> LoadOutcome {
  LoadOutcome {
    provider: Default::default(),
    retry: Default::default(),
    compression: Default::default(),
    session: Default::default(),
    tools: Default::default(),
    plugins,
    harness: ra_cli::config::HarnessLayerSchema {
      root: root.to_path_buf(),
      ..Default::default()
    },
    mode: ConfigMode::Mock,
    warnings: Vec::new(),
    origins: Default::default(),
    code_trust: None,
  }
}

/// 域声明辅助。
fn domain(id: &str, root: &Path, kind: PluginDomainKind) -> PluginDomainSchema {
  PluginDomainSchema {
    id: id.to_string(),
    root: root.to_path_buf(),
    kind,
    read_only: None,
    trusted: false,
  }
}

/// 场景：无域时装配成功、fs 保持主根行为。
#[test]
fn no_domains_keeps_single_root_behavior() {
  let dir = tempfile::tempdir().expect("temp dir");
  let outcome = outcome_with(dir.path(), PluginsSchema::default());
  let session = assemble_with_steps(&outcome, Vec::new()).expect("assemble");
  assert!(session.plugin_registry.is_none());
}

/// 场景：域 root 非绝对 → 启动致命。
#[test]
fn relative_domain_root_is_fatal() {
  let dir = tempfile::tempdir().expect("temp dir");
  let mut plugins = PluginsSchema::default();
  plugins.domains.push(domain(
    "dataset",
    Path::new("relative/path"),
    PluginDomainKind::Data,
  ));
  let outcome = outcome_with(dir.path(), plugins);
  let err = match assemble::assemble(&outcome) {
    Ok(_) => panic!("relative root must fail"),
    Err(e) => e,
  };
  assert_eq!(err.code(), "plugin_domain_invalid");
}

/// 场景：域 root 不存在 → 启动致命。
#[test]
fn missing_domain_root_is_fatal() {
  let dir = tempfile::tempdir().expect("temp dir");
  let missing = dir.path().join("does-not-exist");
  let mut plugins = PluginsSchema::default();
  plugins
    .domains
    .push(domain("dataset", &missing, PluginDomainKind::Data));
  let outcome = outcome_with(dir.path(), plugins);
  let err = match assemble::assemble(&outcome) {
    Ok(_) => panic!("missing root must fail"),
    Err(e) => e,
  };
  assert_eq!(err.code(), "plugin_domain_invalid");
}

/// 场景：域 id 重复 → 启动致命。
#[test]
fn duplicate_domain_id_is_fatal() {
  let dir = tempfile::tempdir().expect("temp dir");
  let mut plugins = PluginsSchema::default();
  plugins
    .domains
    .push(domain("data", dir.path(), PluginDomainKind::Data));
  plugins
    .domains
    .push(domain("data", dir.path(), PluginDomainKind::Skills));
  let outcome = outcome_with(dir.path(), plugins);
  let err = match assemble::assemble(&outcome) {
    Ok(_) => panic!("duplicate id must fail"),
    Err(e) => e,
  };
  assert_eq!(err.code(), "domain_conflict");
}

/// 场景：合法域声明装配成功。
#[test]
fn valid_domains_assemble_successfully() {
  let dir = tempfile::tempdir().expect("temp dir");
  let data_dir = tempfile::tempdir().expect("data dir");
  let mut plugins = PluginsSchema::default();
  plugins.domains.push(domain(
    "dataset",
    data_dir.path(),
    PluginDomainKind::Data,
  ));
  let outcome = outcome_with(dir.path(), plugins);
  assemble::assemble(&outcome).expect("valid domains must assemble");
}

/// 场景：[plugins] 条目与 [[plugins.domains]] 域共存解析（TOML）。
#[test]
fn plugins_entries_and_domains_coexist_in_toml() {
  let text = r#"
[plugins]
enabled = false
[[plugins.entries]]
path = "foo.wasm"
[[plugins.domains]]
id = "dataset"
root = "/mnt/datasets"
kind = "data"
"#;
  let table: toml::Table = toml::from_str(text).expect("valid TOML");
  let plugins: PluginsSchema =
    table["plugins"].clone().try_into().expect("schema");
  assert_eq!(plugins.entries.len(), 1);
  assert_eq!(plugins.domains.len(), 1);
}
