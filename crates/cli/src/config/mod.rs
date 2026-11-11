//! 配置聚合：默认 → 文件逐层覆盖 → 环境变量（仅密钥）→ 校验 → 模式显式化
//!（14 详细设计 §5）。

pub mod assemble;
pub mod schema;

use std::{
  collections::HashSet,
  path::{Path, PathBuf},
};

use serde::de::DeserializeOwned;

pub use schema::{
  CompressionSchema, HarnessLayerSchema, ProviderEntry, ProviderSchema,
  RetrySchema, RoutingSchema, SessionSchema, ToolsSchema,
};

/// 默认配置文件名：`--config` 未指定时自动探测。
pub const DEFAULT_CONFIG_FILE: &str = "ribo.toml";

/// 解析本次启动实际生效的配置文件路径。
///
/// - **显式指定** `--config <PATH>`：原样采用。文件是否存在仍由
///   [`LoadOutcome::load`] 判定，缺失时保持既有的 mock 降级语义；
/// - **未指定**：仅当 [`DEFAULT_CONFIG_FILE`] 存在且为普通文件时采用，
///   否则返回 `None`，由 `LoadOutcome::load` 走全默认 mock（原逻辑）。
///
/// `ribo` 与 `ribo-tui` 共用此函数，保证两个入口的探测规则一致。
pub fn resolve_config_path(specified: Option<&Path>) -> Option<PathBuf> {
  resolve_with_default(specified, Path::new(DEFAULT_CONFIG_FILE))
}

/// 可注入默认路径的纯逻辑：单测无需改动进程工作目录。
fn resolve_with_default(
  specified: Option<&Path>,
  default: &Path,
) -> Option<PathBuf> {
  match specified {
    Some(path) => Some(path.to_path_buf()),
    None => default.is_file().then(|| default.to_path_buf()),
  }
}

/// 配置模式（启动横幅显式展示，概设 D3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigMode {
  /// 真实 provider 模式。
  Real,
  /// mock 模式（无配置/占位/校验失败兜底）。
  Mock,
}

/// 装配输入：六层 schema + 模式与降级原因。
#[derive(Debug, Clone)]
pub struct LoadOutcome {
  pub provider: ProviderSchema,
  pub retry: RetrySchema,
  pub compression: CompressionSchema,
  pub session: SessionSchema,
  pub tools: ToolsSchema,
  pub harness: HarnessLayerSchema,
  /// 运行模式。
  pub mode: ConfigMode,
  /// 局部降级与回退原因（启动横幅逐条展示）。
  pub warnings: Vec<String>,
}

/// mock 结果快捷构造。
fn mock_outcome(
  provider: ProviderSchema,
  retry: RetrySchema,
  compression: CompressionSchema,
  session: SessionSchema,
  tools: ToolsSchema,
  harness: HarnessLayerSchema,
  mut warnings: Vec<String>,
) -> LoadOutcome {
  warnings.push("running in mock mode".to_string());
  LoadOutcome {
    provider,
    retry,
    compression,
    session,
    tools,
    harness,
    mode: ConfigMode::Mock,
    warnings,
  }
}

/// 单层反序列化：损坏 → warning + 保留默认（局部降级）。
fn load_layer<T: DeserializeOwned + Default>(
  table: Option<&toml::Table>,
  key: &str,
  warnings: &mut Vec<String>,
) -> T {
  let Some(value) = table.and_then(|t| t.get(key)) else {
    return T::default();
  };
  match value.clone().try_into() {
    Ok(layer) => layer,
    Err(err) => {
      warnings.push(format!("{key} layer parse failed: {err}"));
      T::default()
    }
  }
}

impl LoadOutcome {
  /// 加载配置（`path` 为 None 或文件不存在 → 全默认 mock 模式）。
  ///
  /// `cli_root` 为 `--root` 参数，覆盖 harness 层 `root`。
  pub fn load(path: Option<&Path>, cli_root: Option<&Path>) -> LoadOutcome {
    let mut warnings = Vec::new();
    let text = path.and_then(|p| std::fs::read_to_string(p).ok());
    let table = text.and_then(|t| toml::from_str::<toml::Table>(&t).ok());

    // ===== 逐层反序列化（单层损坏保留默认）=====
    // provider 层特殊：query_model/[[providers]]/[routing] 均为根级键，
    // 直接从根表整体反序列化（其余五层走 [layer] 子表）
    let mut provider: ProviderSchema = match &table {
      Some(t) => match t.clone().try_into() {
        Ok(p) => p,
        Err(err) => {
          warnings.push(format!("provider layer parse failed: {err}"));
          ProviderSchema::default()
        }
      },
      None => ProviderSchema::default(),
    };
    let retry: RetrySchema = load_layer(table.as_ref(), "retry", &mut warnings);
    let compression: CompressionSchema =
      load_layer(table.as_ref(), "compression", &mut warnings);
    let session: SessionSchema =
      load_layer(table.as_ref(), "session", &mut warnings);
    let mut tools: ToolsSchema =
      load_layer(table.as_ref(), "tools", &mut warnings);
    let mut harness: HarnessLayerSchema =
      load_layer(table.as_ref(), "harness", &mut warnings);
    if let Some(dir) = cli_root {
      harness.root = dir.to_path_buf(); // CLI 参数覆盖（概设决策 3）
    }

    // ===== tools.fs_root 校验：必须是 root 下的相对子路径 =====
    // 沙箱不变式：fs_root 恒位于 root 之下——绝对路径 / ".." 逃逸组件
    // 都会导致沙箱脱离工作空间，warning 后回退 root
    if let Some(fs) = tools.fs_root.as_ref() {
      let escapes = fs.is_absolute()
        || fs
          .components()
          .any(|c| matches!(c, std::path::Component::ParentDir));
      if escapes {
        warnings.push(format!(
          "tools.fs_root must be a relative path under harness.root, fallback to root: {}",
          fs.display()
        ));
        tools.fs_root = None;
      }
    }

    // ===== 无 provider：mock =====
    if provider.providers.is_empty() {
      warnings.push("no providers configured".to_string());
      return mock_outcome(
        provider,
        retry,
        compression,
        session,
        tools,
        harness,
        warnings,
      );
    }

    // ===== 逐条校验：占位 → 整体回退 mock =====
    for p in &provider.providers {
      let mut entry_warnings = Vec::new();
      if !schema::validate_provider_entry(p, &mut entry_warnings) {
        // 先判占位再合并（extend 会移动 entry_warnings）
        let is_placeholder =
          entry_warnings.iter().any(|w| w.contains("placeholder"));
        warnings.extend(entry_warnings);
        // 占位命中即整体不可信（harness_fallback 实测语义）
        if is_placeholder {
          return mock_outcome(
            provider,
            retry,
            compression,
            session,
            tools,
            harness,
            warnings,
          );
        }
      }
    }
    // retain_mut 提供可变引用：expand_api_key 需要原地改写 api_key（$ENV 展开）
    provider.providers.retain_mut(|p| {
      let mut w = Vec::new();
      let ok = schema::validate_provider_entry(p, &mut w)
        && schema::expand_api_key(p, &mut w);
      warnings.extend(w);
      ok
    });

    // ===== query_model 悬空（唯一暴露者被跳过）→ 整体 mock =====
    if provider.providers.is_empty()
      || !provider
        .providers
        .iter()
        .any(|p| p.model == provider.query_model)
    {
      warnings.push("query_model no longer exposed after skip".to_string());
      return mock_outcome(
        provider,
        retry,
        compression,
        session,
        tools,
        harness,
        warnings,
      );
    }

    // ===== 路由剔除引用被跳过 provider 的链 =====
    let registered: HashSet<&str> = provider
      .providers
      .iter()
      .map(|p| p.provider_id.as_str())
      .collect();
    provider.routing.cross_provider.retain(|from, chain| {
      let ok = registered.contains(from.as_str())
        && chain.iter().all(|c| registered.contains(c.as_str()));
      if !ok {
        warnings.push(format!(
          "routing from {from} references skipped provider, pruned"
        ));
      }
      ok
    });

    LoadOutcome {
      provider,
      retry,
      compression,
      session,
      tools,
      harness,
      mode: ConfigMode::Real,
      warnings,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// 场景：配置文件路径解析（CFG-04，未指定时探测默认文件）。
  ///
  /// 预期：显式 `--config` 始终优先；未指定时仅当默认路径存在且为
  /// 普通文件时采用；默认缺失或为目录时返回 `None`（全默认 mock）。
  #[test]
  fn resolve_config_path_probes_default_only_when_unspecified() {
    let dir = tempfile::tempdir().expect("temp");
    let default_file = dir.path().join("ribo.toml");
    let explicit = dir.path().join("custom.toml");

    // 未指定 + 默认不存在 → None（原逻辑：全默认 mock）
    assert_eq!(resolve_with_default(None, &default_file), None);

    // 显式指定始终优先，即便默认文件与显式路径都不存在
    assert_eq!(
      resolve_with_default(Some(&explicit), &default_file),
      Some(explicit.clone())
    );

    // 默认文件存在 → 采用默认；且内容确实被 LoadOutcome 加载
    std::fs::write(&default_file, "[harness]\nroot = \"probe-root\"\n")
      .expect("write");
    let resolved = resolve_with_default(None, &default_file);
    assert_eq!(resolved, Some(default_file.clone()));
    let outcome = LoadOutcome::load(resolved.as_deref(), None);
    assert_eq!(outcome.harness.root, Path::new("probe-root"));

    // 默认文件已存在时，显式指定仍然优先
    assert_eq!(
      resolve_with_default(Some(&explicit), &default_file),
      Some(explicit)
    );

    // 默认路径是目录 → 不采用（is_file 语义）
    let dir_as_default = dir.path().join("a-dir");
    std::fs::create_dir(&dir_as_default).expect("mkdir");
    assert_eq!(resolve_with_default(None, &dir_as_default), None);
  }

  /// 场景：默认值 + 文件逐层覆盖 + 单层损坏保留默认 + 模式标记（CFG-01）。
  #[test]
  fn load_defaults_then_file_overrides_per_layer() {
    // 无文件 → 全默认 mock
    let outcome = LoadOutcome::load(None, None);
    assert_eq!(outcome.mode, ConfigMode::Mock);
    assert_eq!(outcome.session.default_key, "default");
    assert!(outcome.harness.include_environment);

    // 完整文件 → Real + 覆盖生效
    let dir = tempfile::tempdir().expect("temp");
    let path = dir.path().join("ribo.toml");
    std::fs::write(
      &path,
      r#"
query_model = "glm-4.5-flash"
unknown_top_level = 1

[[providers]]
provider_id = "chat-primary"
adapter = "openai-chat"
base_url = "https://real"
api_key = "sk-1"
model = "glm-4.5-flash"

[harness]
root = "custom-data"
include_environment = false

[tools]
fs_root = "tool-space"

[session]
history_lines = 5
"#,
    )
    .expect("write");
    let outcome = LoadOutcome::load(Some(&path), None);
    assert_eq!(outcome.mode, ConfigMode::Real);
    assert_eq!(outcome.harness.root, Path::new("custom-data"));
    assert!(!outcome.harness.include_environment);
    assert_eq!(
      outcome.tools.fs_root.as_deref(),
      Some(Path::new("tool-space"))
    );
    assert_eq!(outcome.session.history_lines, 5);
    assert_eq!(outcome.provider.providers.len(), 1);

    // 坏层（compression 写成字符串）→ 保留默认 + warning，其余层正常
    std::fs::write(
      &path,
      r#"
query_model = "glm-4.5-flash"
compression = "broken"

[[providers]]
provider_id = "chat-primary"
adapter = "openai-chat"
base_url = "https://real"
api_key = "sk-1"
model = "glm-4.5-flash"
"#,
    )
    .expect("write");
    let outcome = LoadOutcome::load(Some(&path), None);
    assert_eq!(outcome.mode, ConfigMode::Real);
    assert!(!outcome.compression.enabled); // 默认
    assert!(
      outcome
        .warnings
        .iter()
        .any(|w| w.contains("compression layer parse failed"))
    );
  }

  /// 场景：$ENV 密钥、占位回退、query_model 悬空、--root 覆盖（CFG-02）。
  #[test]
  fn load_env_key_placeholder_and_root_override() {
    let dir = tempfile::tempdir().expect("temp");

    // 占位 → 整体 Mock
    let path = dir.path().join("p1.toml");
    std::fs::write(
      &path,
      r#"
query_model = "m"
[[providers]]
provider_id = "p1"
adapter = "openai-chat"
base_url = "https://api.example.com"
api_key = "k"
model = "m"
"#,
    )
    .expect("write");
    let outcome = LoadOutcome::load(Some(&path), None);
    assert_eq!(outcome.mode, ConfigMode::Mock);
    assert!(outcome.warnings.iter().any(|w| w.contains("placeholder")));

    // $ENV 未设置 → provider 跳过 → query_model 悬空 → Mock
    let path = dir.path().join("p2.toml");
    std::fs::write(
      &path,
      r#"
query_model = "m"
[[providers]]
provider_id = "p1"
adapter = "openai-chat"
base_url = "https://real"
api_key = "$RIBO_CLI_MISSING_KEY"
model = "m"
"#,
    )
    .expect("write");
    let outcome = LoadOutcome::load(Some(&path), None);
    assert_eq!(outcome.mode, ConfigMode::Mock);
    assert!(
      outcome
        .warnings
        .iter()
        .any(|w| w.contains("query_model no longer exposed"))
    );

    // $ENV 设置 → Real；--root 覆盖 harness 层（edition 2024 下
    // set_var 为 unsafe；键名唯一且测试进程环境无并发读取方）
    #[allow(unsafe_code)]
    unsafe {
      std::env::set_var("RIBO_CLI_CFG_KEY", "sk-live");
    }
    let path = dir.path().join("p3.toml");
    std::fs::write(
      &path,
      r#"
query_model = "m"
[[providers]]
provider_id = "p1"
adapter = "openai-chat"
base_url = "https://real"
api_key = "$RIBO_CLI_CFG_KEY"
model = "m"
"#,
    )
    .expect("write");
    let outcome =
      LoadOutcome::load(Some(&path), Some(Path::new("cli-override")));
    assert_eq!(outcome.mode, ConfigMode::Real);
    assert_eq!(outcome.provider.providers[0].api_key, "sk-live");
    assert_eq!(outcome.harness.root, Path::new("cli-override"));
    #[allow(unsafe_code)]
    unsafe {
      std::env::remove_var("RIBO_CLI_CFG_KEY");
    }
  }

  /// 场景：tools.fs_root 沙箱不变式——恒位于 harness.root 之下。
  ///
  /// 预期：绝对路径与含 `..` 逃逸组件的取值都触发 warning 并回退
  /// `None`（装配期即 root 本身）；合法相对子路径原样保留。
  #[test]
  fn load_rejects_escaping_fs_root() {
    let dir = tempfile::tempdir().expect("temp");

    // 合法相对子路径 → 原样保留（相对 root 解析由装配期 join 完成）
    let path = dir.path().join("ok.toml");
    std::fs::write(
      &path,
      r#"
[tools]
fs_root = "tool-space"
"#,
    )
    .expect("write");
    let outcome = LoadOutcome::load(Some(&path), None);
    assert_eq!(
      outcome.tools.fs_root.as_deref(),
      Some(Path::new("tool-space"))
    );

    // 绝对路径 → warning + 回退 None
    let path = dir.path().join("abs.toml");
    std::fs::write(
      &path,
      r#"
[tools]
fs_root = "C:/absolute/path"
"#,
    )
    .expect("write");
    let outcome = LoadOutcome::load(Some(&path), None);
    assert!(outcome.tools.fs_root.is_none(), "绝对路径应被拒绝");
    assert!(
      outcome
        .warnings
        .iter()
        .any(|w| w.contains("tools.fs_root must be a relative path"))
    );

    // ".." 逃逸组件 → warning + 回退 None
    let path = dir.path().join("escape.toml");
    std::fs::write(
      &path,
      r#"
[tools]
fs_root = "../outside"
"#,
    )
    .expect("write");
    let outcome = LoadOutcome::load(Some(&path), None);
    assert!(outcome.tools.fs_root.is_none(), "`..` 逃逸应被拒绝");
    assert!(
      outcome
        .warnings
        .iter()
        .any(|w| w.contains("tools.fs_root must be a relative path"))
    );
  }
}
