//! 代码信任闸门装配测试（19_ Step 4，设计§7.1/§7.2）。
//!
//! 覆盖：Project 有插件但 code_trust=false → 不加载 + 提示；
//! code_trust=true → 加载；无 Global 域（code_trust=None）→ 兼容自动加载。

use std::path::{Path, PathBuf};

use ra_cli::config::assemble::assemble_with_steps;
use ra_cli::config::code_trust::CodeTrustContext;
use ra_cli::config::trust::TrustedProjects;
use ra_cli::config::{
  ConfigMode, LoadOutcome, PluginEntrySchema, PluginVerifySchema, PluginsSchema,
};

fn plugins_with_missing_wasm() -> PluginsSchema {
  PluginsSchema {
    enabled: true,
    entries: vec![PluginEntrySchema {
      path: PathBuf::from("missing.wasm"),
      config: None,
      acl: Default::default(),
      manifest: None,
      provenance: None,
    }],
    verify: PluginVerifySchema::default(),
  }
}

fn outcome(
  root: &Path,
  plugins: PluginsSchema,
  trust: Option<CodeTrustContext>,
) -> LoadOutcome {
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
    code_trust: trust,
    fs_mounts: Default::default(),
    fatal_error: None,
  }
}

/// 场景：有 Global 域但项目未信任 → 插件不加载（连缺失 wasm 都不触碰），
/// 装配成功且 warnings 含提示文案；技能/AGENTS.md 不受影响。
#[test]
fn untrusted_project_skips_plugins_with_warning() {
  let dir = tempfile::tempdir().expect("temp dir");
  // 空信任记录 = 项目未信任
  let ctx = CodeTrustContext::new(TrustedProjects::empty());
  let outcome = outcome(dir.path(), plugins_with_missing_wasm(), Some(ctx));
  let session =
    assemble_with_steps(&outcome, Vec::new()).expect("assemble succeeds");
  assert!(session.plugin_registry.is_none());
  assert!(session.loaded_plugins.is_empty());
  assert!(
    session
      .warnings
      .iter()
      .any(|w| w.contains("not code-trusted")),
    "warnings should explain trust: {:?}",
    session.warnings
  );
}

/// 场景：项目已 code-trusted → 尝试加载，缺失 wasm 仍致命（闸门不掩盖真实加载错误）。
#[test]
fn trusted_project_attempts_load_and_stays_fatal_on_missing_wasm() {
  let dir = tempfile::tempdir().expect("temp dir");
  let mut trust = TrustedProjects::empty();
  trust.add_project(&dir.path().to_string_lossy(), None);
  let ctx = CodeTrustContext::new(trust);
  let outcome = outcome(dir.path(), plugins_with_missing_wasm(), Some(ctx));
  let err = match assemble_with_steps(&outcome, Vec::new()) {
    Ok(_) => {
      panic!("trusted project must attempt load and fail on missing wasm")
    }
    Err(e) => e,
  };
  assert_eq!(err.code(), "plugin_file_missing");
}

/// 场景：无 Global 域（code_trust=None）→ 退化为自动加载，缺失 wasm 致命（兼容旧行为）。
#[test]
fn no_global_domain_keeps_legacy_auto_load() {
  let dir = tempfile::tempdir().expect("temp dir");
  let outcome = outcome(dir.path(), plugins_with_missing_wasm(), None);
  let err = match assemble_with_steps(&outcome, Vec::new()) {
    Ok(_) => panic!("legacy auto-load must fail on missing wasm"),
    Err(e) => e,
  };
  assert_eq!(err.code(), "plugin_file_missing");
}
