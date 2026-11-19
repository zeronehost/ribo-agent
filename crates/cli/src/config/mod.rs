//! 配置聚合：默认 → 文件逐层覆盖 → 环境变量（仅密钥）→ 校验 → 模式显式化
//!（14 详细设计 §5）。

pub mod assemble;
pub mod code_trust;
pub mod credentials;
pub mod origin;
pub mod schema;
pub mod trust;

use std::{
  collections::HashSet,
  path::{Path, PathBuf},
};

use ra_core::path::is_lexically_safe;
use serde::de::DeserializeOwned;

use self::code_trust::CodeTrustContext;
pub use origin::{ConfigOrigin, ConfigOrigins};
pub use schema::{
  CompressionSchema, GlobalSchema, HarnessLayerSchema, PluginAclSchema,
  PluginDomainKind, PluginDomainSchema, PluginEntrySchema, PluginVerifySchema,
  PluginsSchema, ProviderEntry, ProviderSchema, RetrySchema, RoutingSchema,
  SessionSchema, ToolsSchema,
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

/// 全局域：跨项目共享的用户级配置根（设计§3 / §8.1）。
///
/// 目录发现遵循"`--global-dir` > 平台默认配置目录 + `/ribo`"，目录不存在
/// 时域禁用（非致命，R4：不主动创建、不报错）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalDomain {
  /// 全局域根目录（已由发现逻辑确认为存在的目录）。
  pub root: PathBuf,
}

impl GlobalDomain {
  /// 默认目录名：平台配置目录之下的子目录。
  pub const DIR_NAME: &'static str = "ribo";
  /// 全局默认配置文件名。
  pub const SETTINGS_FILE: &'static str = "settings.toml";

  /// 发现全局域：`cli_dir` > `dirs::config_dir()/ribo`；目录不存在返回 `None`。
  ///
  /// `enabled == false` 时直接禁用（对应 `[global] enabled`）。本函数**不做
  /// 任何创建目录的 IO**，仅检查存在性与目录类型。
  pub fn discover(cli_dir: Option<&Path>, enabled: bool) -> Option<Self> {
    if !enabled {
      return None;
    }
    let root = match cli_dir {
      Some(dir) => dir.to_path_buf(),
      None => dirs::config_dir()?.join(Self::DIR_NAME),
    };
    if root.is_dir() {
      tracing::info!(global_root = %root.display(), "global domain discovered");
      Some(Self { root })
    } else {
      None
    }
  }

  /// 全局默认配置文件路径（`root/settings.toml`）。
  pub fn settings_path(&self) -> PathBuf {
    self.root.join(Self::SETTINGS_FILE)
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
  pub plugins: PluginsSchema,
  pub harness: HarnessLayerSchema,
  /// 运行模式。
  pub mode: ConfigMode,
  /// 局部降级与回退原因（启动横幅逐条展示）。
  pub warnings: Vec<String>,
  /// 字段级来源标记：`--show-origin` / `--explain` 的数据源。
  /// 零配置时为空（所有字段隐含来源为 `default`）。
  pub origins: ConfigOrigins,
  /// 代码信任判定上下文（19_ Step 4）。`None` = 无 Global 域 / 兼容入口，
  /// 装配层退化为「自动加载」的旧语义（回滚开关）。
  pub code_trust: Option<CodeTrustContext>,
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

/// 解析 TOML 文本 → 表；解析失败返回 `None`（调用方按降级处理）。
fn parse_table(text: Option<String>) -> Option<toml::Table> {
  text.and_then(|t| match toml::from_str::<toml::Table>(&t) {
    Ok(table) => Some(table),
    Err(err) => {
      tracing::warn!(error = %err, "config layer parse failed");
      None
    }
  })
}

/// 记录一个 TOML 值内所有叶子字段的来源（子表递归，路径以 `.` 连接）。
///
/// 数组（如 `[[providers]]`）视为整体字段，只记录数组名本身——来源标记
/// 服务于 `--show-origin`，无需细分到数组元素。
fn record_origin(
  prefix: &str,
  value: &toml::Value,
  origin: ConfigOrigin,
  origins: &mut ConfigOrigins,
) {
  match value {
    toml::Value::Table(sub) => {
      for (key, v) in sub {
        record_origin(&format!("{prefix}.{key}"), v, origin, origins);
      }
    }
    _ => origins.set(prefix, origin),
  }
}

/// 递归逐字段合并两层 TOML 表：`project` 覆盖 `global`，记录每个叶子来源。
///
/// 语义（设计§5.1）：
/// - 双方同键且都是子表 → **递归合并**（子表内字段各自覆写，这是关键：
///   `[retry]` 只写 `max_retries` 时不应抹掉 global 的 `initial_backoff_ms`）；
/// - 其余情况（标量 / 数组 / 类型不同）→ project 整体覆盖，来源 `Project`；
/// - 仅 global 有的键 → 沿用，来源 `Global`；
/// - TOML 无 `null`："删键 = 未设置 = 沿用父层"，不做显式清空。
///
/// 本函数只合并**已存在的键**，不展开默认值——缺省字段在后续 `load_layer`
/// 中由各 schema 的 `Default` 填充，来源隐含为 `Default`。
fn merge_tables(
  global: Option<&toml::Table>,
  project: Option<&toml::Table>,
  origins: &mut ConfigOrigins,
) -> Option<toml::Table> {
  match (global, project) {
    (None, None) => None,
    (Some(g), None) => {
      for (key, value) in g {
        record_origin(key, value, ConfigOrigin::Global, origins);
      }
      Some(g.clone())
    }
    (None, Some(p)) => {
      for (key, value) in p {
        record_origin(key, value, ConfigOrigin::Project, origins);
      }
      Some(p.clone())
    }
    (Some(g), Some(p)) => {
      let mut merged = g.clone();
      // ===== project 的键逐字段覆盖 =====
      for (key, p_value) in p {
        let merged_value = match (g.get(key), p_value) {
          (Some(toml::Value::Table(g_sub)), toml::Value::Table(p_sub)) => {
            // 双方都是子表 → 递归合并
            let mut sub = g_sub.clone();
            for (sk, sv) in p_sub {
              sub.insert(sk.clone(), sv.clone());
              record_origin(
                &format!("{key}.{sk}"),
                sv,
                ConfigOrigin::Project,
                origins,
              );
            }
            // 子表内仅 global 有的字段
            for sk in g_sub.keys() {
              if !p_sub.contains_key(sk) {
                let gv = &g_sub[sk];
                record_origin(
                  &format!("{key}.{sk}"),
                  gv,
                  ConfigOrigin::Global,
                  origins,
                );
              }
            }
            toml::Value::Table(sub)
          }
          _ => {
            record_origin(key, p_value, ConfigOrigin::Project, origins);
            p_value.clone()
          }
        };
        merged.insert(key.clone(), merged_value);
      }
      // ===== 仅 global 有的顶层键 =====
      for (key, g_value) in g {
        if !p.contains_key(key) {
          record_origin(key, g_value, ConfigOrigin::Global, origins);
        }
      }
      Some(merged)
    }
  }
}

/// 从 Global 配置表提取 `plugins.domains` 声明的域 id 集合。
///
/// 只有 Global 域声明的 `trusted = true` 才生效（设计§7.3）；项目自签
/// 的域不在该集合中，装配层据此拒绝其代码信任。
fn extract_domain_ids(table: Option<&toml::Table>) -> Vec<String> {
  let Some(t) = table else {
    return Vec::new();
  };
  let Some(plugins) = t.get("plugins").and_then(|v| v.as_table()) else {
    return Vec::new();
  };
  let Some(domains) = plugins.get("domains").and_then(|v| v.as_array()) else {
    return Vec::new();
  };
  domains
    .iter()
    .filter_map(|v| v.as_table())
    .filter_map(|d| d.get("id").and_then(|v| v.as_str()))
    .map(str::to_string)
    .collect()
}

impl LoadOutcome {
  /// 加载配置（`path` 为 None 或文件不存在 → 全默认 mock 模式）。
  ///
  /// `cli_root` 为 `--root` 参数，覆盖 harness 层 `root`。
  ///
  /// **兼容入口**：不发现 Global 域、`origins` 恒为空，行为与 v1（单一
  /// 配置文件）完全一致。新入口 [`Self::load_with_global`] 才启用分层合并。
  pub fn load(path: Option<&Path>, cli_root: Option<&Path>) -> LoadOutcome {
    let table = parse_table(path.and_then(|p| std::fs::read_to_string(p).ok()));
    Self::from_table(table, cli_root, Vec::new(), ConfigOrigins::new(), None)
  }

  /// 加载配置（含 Global 域）：`Global.settings.toml` ← `Project.ribo.toml` ← CLI。
  ///
  /// `cli_root` 覆盖 harness.root；`global_dir` 覆盖 `--global-dir`。
  /// Global 目录不存在 / `[global] enabled = false` → 域禁用（非致命），
  /// 退化为与 [`Self::load`] 相同的单项目配置行为。
  pub fn load_with_global(
    path: Option<&Path>,
    cli_root: Option<&Path>,
    global_dir: Option<&Path>,
  ) -> LoadOutcome {
    let mut warnings = Vec::new();

    // ===== ① 先读 Project 表（ribo.toml / --config）=====
    let project_table =
      parse_table(path.and_then(|p| std::fs::read_to_string(p).ok()));

    // ===== ② 解析 [global]：enabled 开关 + dir 覆盖 =====
    // 全局开关必须写在项目配置里——全局目录尚未发现时无从读取其 [global] 段。
    let global_schema: GlobalSchema =
      load_layer(project_table.as_ref(), "global", &mut warnings);
    let effective_dir = global_dir.or(global_schema.dir.as_deref());

    // ===== ③ 发现 Global 域（目录缺失 / enabled=false → None，非致命）=====
    let global = GlobalDomain::discover(effective_dir, global_schema.enabled);

    // ===== ④ 读 Global.settings.toml（单层损坏 → warning + 忽略该层）=====
    let global_table = global.as_ref().and_then(|g| {
      match std::fs::read_to_string(g.settings_path()) {
        Ok(text) => parse_table(Some(text)),
        Err(_) => None, // settings.toml 不存在属正常（全局域可只有资源目录）
      }
    });
    if global.is_some()
      && global_table.is_none()
      && global.as_ref().is_some_and(|g| g.settings_path().exists())
    {
      warnings.push(
        "global settings.toml parse failed, global layer ignored".to_string(),
      );
    }

    // ===== ⑤ 逐字段合并 + 来源标记 =====
    let mut origins = ConfigOrigins::new();
    let merged =
      merge_tables(global_table.as_ref(), project_table.as_ref(), &mut origins);

    // ===== ⑥ 代码信任上下文（19_ Step 4）=====
    // 仅当 Global 域存在时启用闸门；无 Global 域 → None（回滚为自动加载）。
    // 信任记录位于 global.root/trusted_projects.toml（设计§7.1）。
    let code_trust = global.as_ref().map(|g| {
      let global_domain_ids = extract_domain_ids(global_table.as_ref());
      let trust = self::trust::TrustedProjects::load(
        &g.root.join("trusted_projects.toml"),
      )
      .unwrap_or_else(|e| {
        warnings.push(format!(
          "trusted_projects.toml load failed: {e}; code trust denied"
        ));
        self::trust::TrustedProjects::empty()
      });
      CodeTrustContext::new(trust, global_domain_ids).with_global_root(&g.root)
    });

    Self::from_table(merged, cli_root, warnings, origins, code_trust)
  }

  /// 从合并后的表构造 outcome（含逐层反序列化、校验、mock 降级）。
  fn from_table(
    table: Option<toml::Table>,
    cli_root: Option<&Path>,
    mut warnings: Vec<String>,
    mut origins: ConfigOrigins,
    code_trust: Option<CodeTrustContext>,
  ) -> LoadOutcome {
    // ===== 逐层反序列化（单层损坏保留默认）=====
    // provider 层特殊：query_model/[[providers]]/[routing] 均为根级键，
    // 直接从根表整体反序列化（其余五层走 [layer] 子表）
    let provider: ProviderSchema = match &table {
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
    let plugins: PluginsSchema =
      load_layer(table.as_ref(), "plugins", &mut warnings);
    let mut harness: HarnessLayerSchema =
      load_layer(table.as_ref(), "harness", &mut warnings);
    if let Some(dir) = cli_root {
      harness.root = dir.to_path_buf(); // CLI 参数覆盖（概设决策 3）
      origins.set("harness.root", ConfigOrigin::Cli);
    }

    // ===== tools.fs_root 校验：必须是 root 下的相对子路径 =====
    // 沙箱不变式（P1）：fs_root 是域内路径，恒位于 root 之下——绝对路径 /
    // ".." 逃逸组件都会导致沙箱脱离工作空间，warning 后回退 root。
    // 词法判定复用 ra_core::path 统一原语，避免与插件 / 技能路径的判定漂移。
    if let Some(fs) = tools.fs_root.as_ref()
      && !is_lexically_safe(fs)
    {
      warnings.push(format!(
        "tools.fs_root must be a relative path under harness.root, fallback to root: {}",
        fs.display()
      ));
      tools.fs_root = None;
    }

    // ===== 聚合 outcome（先按 Real 构造；各 mock 分支就地降级）=====
    let mut outcome = LoadOutcome {
      provider,
      retry,
      compression,
      session,
      tools,
      plugins,
      harness,
      mode: ConfigMode::Real,
      warnings,
      origins,
      code_trust,
    };

    // ===== 无 provider：mock =====
    if outcome.provider.providers.is_empty() {
      outcome.warnings.push("no providers configured".to_string());
      outcome.warnings.push("running in mock mode".to_string());
      outcome.mode = ConfigMode::Mock;
      return outcome;
    }

    // ===== 逐条校验：占位 → 整体回退 mock =====
    for p in &outcome.provider.providers {
      let mut entry_warnings = Vec::new();
      if !schema::validate_provider_entry(p, &mut entry_warnings) {
        // 先判占位再合并（extend 会移动 entry_warnings）
        let is_placeholder =
          entry_warnings.iter().any(|w| w.contains("placeholder"));
        outcome.warnings.extend(entry_warnings);
        // 占位命中即整体不可信（harness_fallback 实测语义）
        if is_placeholder {
          outcome.warnings.push("running in mock mode".to_string());
          outcome.mode = ConfigMode::Mock;
          return outcome;
        }
      }
    }
    // retain_mut 提供可变引用：expand_api_key 需要原地改写 api_key（$ENV
    // 展开）。warning 先收集到局部再并入 outcome——闭包持有 providers
    // 的可变借用，直接写 outcome.warnings 会撞 E0502。
    let mut prune_warnings = Vec::new();
    outcome.provider.providers.retain_mut(|p| {
      let mut w = Vec::new();
      let ok = schema::validate_provider_entry(p, &mut w)
        && schema::expand_api_key(p, &mut w);
      prune_warnings.extend(w);
      ok
    });
    outcome.warnings.extend(prune_warnings);

    // ===== query_model 悬空（唯一暴露者被跳过）→ 整体 mock =====
    if outcome.provider.providers.is_empty()
      || !outcome
        .provider
        .providers
        .iter()
        .any(|p| p.model == outcome.provider.query_model)
    {
      outcome
        .warnings
        .push("query_model no longer exposed after skip".to_string());
      outcome.warnings.push("running in mock mode".to_string());
      outcome.mode = ConfigMode::Mock;
      return outcome;
    }

    // ===== 路由剔除引用被跳过 provider 的链 =====
    let registered: HashSet<&str> = outcome
      .provider
      .providers
      .iter()
      .map(|p| p.provider_id.as_str())
      .collect();
    // warning 先收集到局部再并入 outcome——闭包持有 routing 的可变
    // 借用，直接写 outcome.warnings 会撞 E0502。
    let mut prune_warnings = Vec::new();
    outcome
      .provider
      .routing
      .cross_provider
      .retain(|from, chain| {
        let ok = registered.contains(from.as_str())
          && chain.iter().all(|c| registered.contains(c.as_str()));
        if !ok {
          prune_warnings.push(format!(
            "routing from {from} references skipped provider, pruned"
          ));
        }
        ok
      });
    outcome.warnings.extend(prune_warnings);
    outcome
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

  /// 场景：全局域目录发现与禁用。
  ///
  /// 预期：目录存在 → Some；`enabled=false` 或目录不存在 → None（非致命）。
  #[test]
  fn global_domain_discover_respects_enabled_and_existence() {
    let dir = tempfile::tempdir().expect("temp");
    let global_root = dir.path().join("global");
    std::fs::create_dir_all(&global_root).expect("mkdir global");

    let discovered = GlobalDomain::discover(Some(&global_root), true)
      .expect("should discover");
    assert_eq!(discovered.root, global_root);
    assert_eq!(
      discovered.settings_path(),
      global_root.join(GlobalDomain::SETTINGS_FILE)
    );

    // enabled=false → 禁用
    assert!(GlobalDomain::discover(Some(&global_root), false).is_none());
    // 目录不存在 → 禁用
    assert!(
      GlobalDomain::discover(Some(&dir.path().join("missing")), true).is_none()
    );
  }

  /// 场景：Global.settings 与 Project.ribo 逐字段合并 + 来源标记。
  ///
  /// 预期：project 有则覆盖 global；来源标记反映最终来源；
  /// 仅 global 有的字段沿用 global 并标记 global。
  #[test]
  fn load_with_global_merges_fields_and_tracks_origin() {
    let dir = tempfile::tempdir().expect("temp");
    let global_root = dir.path().join("global");
    std::fs::create_dir_all(&global_root).expect("mkdir global");
    std::fs::write(
      global_root.join("settings.toml"),
      r#"
[retry]
max_retries = 9
initial_backoff_ms = 123

[session]
default_key = "from-global"
"#,
    )
    .expect("write global settings");

    let project = dir.path().join("ribo.toml");
    std::fs::write(
      &project,
      r#"
[retry]
max_retries = 3

[harness]
root = "./proj"
"#,
    )
    .expect("write project");

    let outcome =
      LoadOutcome::load_with_global(Some(&project), None, Some(&global_root));

    // project 覆盖 global
    assert_eq!(outcome.retry.max_retries, 3);
    assert_eq!(
      outcome.origins.get("retry.max_retries"),
      ConfigOrigin::Project
    );
    // 仅 global 有的字段沿用
    assert_eq!(outcome.retry.initial_backoff_ms, 123);
    assert_eq!(
      outcome.origins.get("retry.initial_backoff_ms"),
      ConfigOrigin::Global
    );
    assert_eq!(outcome.session.default_key, "from-global");
    assert_eq!(
      outcome.origins.get("session.default_key"),
      ConfigOrigin::Global
    );
    // 项目字段来源 project
    assert_eq!(outcome.harness.root, Path::new("./proj"));
    assert_eq!(outcome.origins.get("harness.root"), ConfigOrigin::Project);
  }

  /// 场景：全局域目录缺失但项目配置正常（P3 故障隔离）。
  ///
  /// 预期：项目配置照常加载、无 warning、启动不被全局域缺失拖垮。
  #[test]
  fn load_with_global_missing_domain_keeps_project_alive() {
    let dir = tempfile::tempdir().expect("temp");
    let missing_global = dir.path().join("absent-global");
    let project = dir.path().join("ribo.toml");
    std::fs::write(
      &project,
      "[session]\ndefault_key = \"project-session\"\n[harness]\nroot = \"./work\"\n",
    )
    .expect("write project");

    let outcome = LoadOutcome::load_with_global(
      Some(&project),
      None,
      Some(&missing_global),
    );
    assert_eq!(outcome.session.default_key, "project-session");
    assert_eq!(outcome.harness.root, Path::new("./work"));
    assert_eq!(
      outcome.origins.get("session.default_key"),
      ConfigOrigin::Project
    );
    // 全局域缺失不应产生 fatal；mock warning 属正常（无 provider）
    assert!(outcome.warnings.iter().all(|w| !w.contains("global")));
  }

  /// 场景：CLI --root 覆盖并标记为 cli 来源。
  ///
  /// 预期：harness.root 取 CLI 值，来源标记为 Cli（最高优先级）。
  #[test]
  fn load_with_global_cli_root_wins_with_cli_origin() {
    let dir = tempfile::tempdir().expect("temp");
    let project = dir.path().join("ribo.toml");
    std::fs::write(&project, "[harness]\nroot = \"./from-file\"\n")
      .expect("write project");
    let cli_root = Path::new("/tmp/cli-root");

    let outcome =
      LoadOutcome::load_with_global(Some(&project), Some(cli_root), None);
    assert_eq!(outcome.harness.root, cli_root);
    assert_eq!(outcome.origins.get("harness.root"), ConfigOrigin::Cli);
  }

  /// 场景：零配置 + 全局域缺失 → 退化为默认，来源映射为空。
  ///
  /// 预期：所有字段取默认，`origins.is_empty()`（隐含 default）。
  #[test]
  fn load_with_global_zero_config_degrades_to_defaults() {
    let dir = tempfile::tempdir().expect("temp");
    let missing_global = dir.path().join("no-global-here");
    let outcome =
      LoadOutcome::load_with_global(None, None, Some(&missing_global));
    assert!(outcome.origins.is_empty());
    assert_eq!(outcome.harness.root, Path::new("."));
    assert_eq!(outcome.origins.get("harness.root"), ConfigOrigin::Default);
  }

  /// 场景：[global] enabled=false 关闭全局域。
  ///
  /// 预期：即使 global_dir 存在，settings 也不被合并。
  #[test]
  fn load_with_global_respects_enabled_false() {
    let dir = tempfile::tempdir().expect("temp");
    let global_root = dir.path().join("global");
    std::fs::create_dir_all(&global_root).expect("mkdir global");
    std::fs::write(
      global_root.join("settings.toml"),
      "[session]\ndefault_key = \"should-not-load\"\n",
    )
    .expect("write global settings");

    let project = dir.path().join("ribo.toml");
    std::fs::write(&project, "[global]\nenabled = false\n")
      .expect("write project");

    let outcome =
      LoadOutcome::load_with_global(Some(&project), None, Some(&global_root));
    assert_eq!(outcome.session.default_key, "default");
    assert_eq!(
      outcome.origins.get("session.default_key"),
      ConfigOrigin::Default
    );
  }

  /// 场景：旧入口 `load` 不发现全局域（向后兼容）。
  ///
  /// 预期：即便存在 settings.toml，`load` 也不读取，origins 为空。
  #[test]
  fn legacy_load_ignores_global_domain() {
    let dir = tempfile::tempdir().expect("temp");
    let global_root = dir.path().join("global");
    std::fs::create_dir_all(&global_root).expect("mkdir global");
    std::fs::write(
      global_root.join("settings.toml"),
      "[session]\ndefault_key = \"legacy-should-ignore\"\n",
    )
    .expect("write global settings");

    // 旧入口只接受 config/root 两参数，无法传 global_dir → 不发现全局域
    let outcome = LoadOutcome::load(None, None);
    assert!(outcome.origins.is_empty());
    assert_eq!(outcome.session.default_key, "default");
  }
}
