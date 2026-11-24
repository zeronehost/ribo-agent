//! 配置聚合：默认 → 文件逐层覆盖 → 环境变量（仅密钥）→ 校验 → 模式显式化。

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

use serde::de::DeserializeOwned;

use self::code_trust::CodeTrustContext;
pub use origin::{ConfigOrigin, ConfigOrigins};
use schema::ProviderWithOrigin;
pub use schema::{
  CompressionSchema, FsMountsSchema, GlobalSchema, HarnessLayerSchema,
  PluginAclSchema, PluginEntrySchema, PluginVerifySchema, PluginsSchema,
  ProviderEntry, ProviderSchema, RetrySchema, RoutingSchema, SessionSchema,
  ToolsSchema,
};

use self::credentials::AuthConfig;

/// 默认配置文件名：`--config` 未指定时自动探测。
pub const DEFAULT_CONFIG_FILE: &str = "ribo.toml";

/// 解析本次启动实际生效的配置文件路径（显式 --config 缺失报错）。
///
/// - **显式指定** `--config <PATH>`：必须存在且是普通文件，否则
///   `Err(config.config_file_missing)`——不再静默降级；
/// - **未指定**：从 cwd 向上探测 `ribo.toml`（`.git` 处停止），命中即采用；
///   否则返回 `Ok(None)`，由 [`LoadOutcome::load`] 走全默认 mock（开发）或
///   release 下由致命错误拦截。
///
/// `ribo` 与 `ribo-tui` 共用此函数，保证两个入口的探测规则一致。
pub fn resolve_config_path(
  specified: Option<&Path>,
) -> ra_core::error::RiboResult<Option<PathBuf>> {
  resolve_with_default(specified, Path::new(DEFAULT_CONFIG_FILE))
}

/// 可注入默认路径的纯逻辑：单测无需改动进程工作目录。
fn resolve_with_default(
  specified: Option<&Path>,
  default: &Path,
) -> ra_core::error::RiboResult<Option<PathBuf>> {
  match specified {
    Some(path) => {
      if !path.is_file() {
        return Err(ra_core::error::RiboError::new(
          "config",
          "config_file_missing",
          format!("config file not found or not a file: {}", path.display()),
        ));
      }
      Ok(Some(path.to_path_buf()))
    }
    None => Ok(find_config_upwards(default)),
  }
}

/// 从 cwd 向上探测配置文件：命中第一个即返回；`.git` 所在目录参与探测
/// 且遇到即停；无 git 仓库探到文件系统根；无命中返回 None（回退 cwd）。
fn find_config_upwards(default: &Path) -> Option<PathBuf> {
  let cwd = std::env::current_dir().ok()?;
  find_config_upwards_from(&cwd, default)
}

/// 可注入起点的纯逻辑（单测无需改动进程工作目录）。
fn find_config_upwards_from(start: &Path, default: &Path) -> Option<PathBuf> {
  let mut dir = start.to_path_buf();
  loop {
    let candidate = dir.join(default);
    if candidate.is_file() {
      return Some(candidate);
    }
    if dir.join(".git").exists() {
      return None;
    }
    if !dir.pop() {
      return None;
    }
  }
}

/// 全局域：跨项目共享的用户级配置根。
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

/// 配置模式（启动横幅显式展示）。
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
  /// 代码信任判定上下文。`None` = 无 Global 域 / 兼容入口，
  /// 装配层退化为「自动加载」的旧语义（回滚开关）。
  pub code_trust: Option<CodeTrustContext>,
  /// fs 只读挂载点（v3 取代资源域；仅全局 settings.toml 声明）。
  pub fs_mounts: FsMountsSchema,
  /// release 构建下的致命配置错误：main 入口须检查并退出。
  /// debug 构建恒为 None（保留 mock 降级）。
  pub fatal_error: Option<ra_core::error::RiboError>,
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
/// 语义（设计）：
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

/// 解析全局域根目录。
///
/// 优先级：`--global-dir`（`cli_dir`）> 项目 `[global].dir` > 平台默认
/// 配置目录 + `/ribo`。**不创建目录、不校验存在性**——写入端由调用方
/// `create_dir_all`，读取端由 [`GlobalDomain::discover`] 做 `is_dir()` 校验。
///
/// `project_table` 为项目配置表（`ribo.toml` 解析结果）；`None` 表示无
/// 项目配置，此时只能走 CLI 参数或平台默认。
pub fn resolve_global_dir(
  cli_dir: Option<&Path>,
  project_table: Option<&toml::Table>,
) -> Option<PathBuf> {
  if let Some(d) = cli_dir {
    return Some(d.to_path_buf());
  }
  let dir_override = project_table
    .and_then(|t| t.get("global"))
    .and_then(|v| v.as_table())
    .and_then(|t| t.get("dir"))
    .and_then(|v| v.as_str())
    .map(PathBuf::from);
  if let Some(d) = dir_override {
    return Some(d);
  }
  dirs::config_dir().map(|d| d.join(GlobalDomain::DIR_NAME))
}

/// 合并两域 providers（Step 8，方案伪码）。
///
/// project 在前按声明序全部放入；global 在后只追加 provider_id 未冲突的；
/// 同名 project 整条胜出（不继承全局字段）。`array_overwrite` 环境变量
/// 走旧语义（项目非空则整体替换）；未知值 warn 后走默认 id-merge。
fn merge_providers(
  project: Vec<ProviderWithOrigin>,
  global: Vec<ProviderWithOrigin>,
  warnings: &mut Vec<String>,
) -> Vec<ProviderWithOrigin> {
  let mode = std::env::var("RIBO_PROVIDERS_MERGE_MODE").unwrap_or_default();
  if mode == "array_overwrite" {
    return if project.is_empty() { global } else { project };
  }
  if !mode.is_empty() {
    warnings.push(format!(
      "unknown RIBO_PROVIDERS_MERGE_MODE `{mode}`, using default id-merge"
    ));
  }
  let mut merged: Vec<ProviderWithOrigin> = Vec::new();
  let mut seen: HashSet<String> = HashSet::new();
  for p in project {
    seen.insert(p.entry.provider_id.clone());
    merged.push(p);
  }
  for p in global {
    if !seen.contains(&p.entry.provider_id) {
      merged.push(p);
    }
  }
  merged
}

/// 从 one 域 TOML 表解析 providers 并打来源标签（Step 8）。
///
/// 步骤②：配置文件来源的 api_key 在此先做 `$ENV` 展开；auth.toml
/// 的值不走本函数（保证不展开）。展开失败的 provider 保留原值，交由
/// from_table 的校验链统一跳过。
fn sourced_providers(
  table: Option<&toml::Table>,
  origin: ConfigOrigin,
  warnings: &mut Vec<String>,
) -> Vec<ProviderWithOrigin> {
  let Some(t) = table else {
    return Vec::new();
  };
  schema::providers_from_table(t)
    .into_iter()
    .map(|mut entry| {
      let mut w = Vec::new();
      if !schema::expand_api_key(&mut entry, &mut w) {
        // $ENV 未设置：清空 key，交由后续统一处理——
        // Global 来源仍有 auth.toml 补充机会；Project 来源被 validate 跳过。
        entry.api_key.clear();
      }
      warnings.extend(w);
      ProviderWithOrigin { entry, origin }
    })
    .collect()
}

/// 读取全局域 auth.toml。
///
/// 文件不存在 → 空集（正常）；损坏 → warning + 空集（不致命）；
/// Unix 权限非 0600 → warning 但继续读取（日志不打印完整路径，
/// 避免暴露用户名。
fn load_auth_config(
  root: Option<&Path>,
  warnings: &mut Vec<String>,
) -> AuthConfig {
  let Some(root) = root else {
    return AuthConfig::empty();
  };
  let path = root.join("auth.toml");

  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(&path) {
      let mode = meta.permissions().mode() & 0o777;
      if mode != 0o600 {
        warnings.push(format!(
          "auth.toml should be 0600 (current {:o}); continuing to read",
          mode
        ));
      }
    }
  }
  #[cfg(not(unix))]
  let _ = &path;

  match AuthConfig::load(&path) {
    Ok(cfg) => cfg,
    Err(e) => {
      warnings.push(format!(
        "auth.toml load failed: {e}; global providers will not be supplemented"
      ));
      AuthConfig::empty()
    }
  }
}

impl LoadOutcome {
  /// 日志根目录：Global 域启用（`code_trust` 为 Some，即 enabled 且目录
  /// 存在）→ 全局域目录；否则 → 项目内部目录 `.ribo/`（logging 层在其
  /// 下拼接 `logs/`，最终落 `.ribo/logs/`，收敛）。
  ///
  /// 日志属"运行期产物"，集中到全局域便于跨项目统一收集；未启用全局域
  /// 时退回项目 `.ribo/`（与 sessions/skills/plugins 同一内部目录纪律）。
  pub fn log_root(&self) -> PathBuf {
    match self.code_trust.as_ref().and_then(|ctx| ctx.global_root()) {
      Some(global) => global.to_path_buf(),
      None => self.harness.root.join(".ribo"),
    }
  }

  /// 加载配置（`path` 为 None 或文件不存在 → 全默认 mock 模式）。
  ///
  /// `cli_root` 为 `--root` 参数，覆盖 harness 层 `root`。
  ///
  /// **兼容入口**：不发现 Global 域、`origins` 恒为空，行为与 v1（单一
  /// 配置文件）完全一致。新入口 [`Self::load_with_global`] 才启用分层合并。
  pub fn load(path: Option<&Path>, cli_root: Option<&Path>) -> LoadOutcome {
    let table = parse_table(path.and_then(|p| std::fs::read_to_string(p).ok()));
    // 兼容入口：从单文件解析 providers（Project 来源），保持 v1 行为——
    // 展开 $ENV（经由 sourced_providers）、不读 auth.toml、origins 恒空。
    let mut warnings = Vec::new();
    let providers =
      sourced_providers(table.as_ref(), ConfigOrigin::Project, &mut warnings);
    // 第 2 档：--config（或兼容入口显式 path）文件父目录作为工作目录
    // 默认值；相对路径 "ribo.toml" 的父目录为空 → 回退 cwd 档。
    let config_parent = path
      .and_then(|p| p.parent())
      .filter(|p| !p.as_os_str().is_empty());
    Self::from_table(
      table,
      cli_root,
      warnings,
      ConfigOrigins::new(),
      None,
      providers,
      AuthConfig::empty(),
      FsMountsSchema::default(), // 兼容入口无全局域：挂载点不可用
      config_parent,
    )
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
    // 唯一真值：读写两端共用 resolve_global_dir。
    let effective_dir = resolve_global_dir(global_dir, project_table.as_ref());

    // ===== ③ 发现 Global 域（目录缺失 / enabled=false → None，非致命）=====
    let global =
      GlobalDomain::discover(effective_dir.as_deref(), global_schema.enabled);

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

    // ===== ④ 分来源解析 providers + 合并 + auth.toml =====
    // 数组字段 providers 不走 merge_tables 的"整体覆盖"：
    // 分别解析两域、先做 $ENV 展开（步骤②），再按 provider_id 合并。
    let project_providers = sourced_providers(
      project_table.as_ref(),
      ConfigOrigin::Project,
      &mut warnings,
    );
    let global_providers = sourced_providers(
      global_table.as_ref(),
      ConfigOrigin::Global,
      &mut warnings,
    );
    let merged_providers =
      merge_providers(project_providers, global_providers, &mut warnings);

    // auth.toml 在入口读一次（权限检查 + 脱敏），传给 from_table 补充
    let auth_config = load_auth_config(
      global.as_ref().map(|g| g.root.as_path()),
      &mut warnings,
    );

    // ===== ⑤ 逐字段合并 + 来源标记 =====
    let mut origins = ConfigOrigins::new();
    let merged =
      merge_tables(global_table.as_ref(), project_table.as_ref(), &mut origins);

    // ===== ⑤ fs.mounts：仅全局 settings.toml 声明=====
    // 项目配置无权声明——发现 [fs.*] 键即 warning + 忽略（防项目越权暴露
    // 沙箱外路径：挂载点语义是用户级路径别名，暴露决定权天然属于用户）。
    if project_table.as_ref().is_some_and(|t| t.contains_key("fs")) {
      warnings.push(
        "[fs] declared in project config is ignored; mounts may only be \
         declared in global settings.toml"
          .to_string(),
      );
    }
    let fs_mounts: FsMountsSchema =
      match global_table.as_ref().and_then(|t| t.get("fs")).cloned() {
        Some(fs_val) => match fs_val.try_into() {
          Ok(m) => m,
          Err(_) => {
            warnings.push(
              "global [fs.mounts] parse failed, mounts ignored".to_string(),
            );
            FsMountsSchema::default()
          }
        },
        None => FsMountsSchema::default(),
      };

    // ===== ⑥ 代码信任上下文=====
    // 仅当 Global 域存在时启用闸门；无 Global 域 → None（回滚为自动加载）。
    // 信任记录位于 global.root/trusted_projects.toml（设计）。
    let code_trust = global.as_ref().map(|g| {
      let trust = self::trust::TrustedProjects::load(
        &g.root.join("trusted_projects.toml"),
      )
      .unwrap_or_else(|e| {
        warnings.push(format!(
          "trusted_projects.toml load failed: {e}; code trust denied"
        ));
        self::trust::TrustedProjects::empty()
      });
      CodeTrustContext::new(trust).with_global_root(&g.root)
    });

    // 第 2/3 档：已解析配置文件（--config 或向上探测命中）的父目录
    // 作为工作目录默认值；None（零配置）→ 回退 cwd 档。
    let config_parent = path
      .and_then(|p| p.parent())
      .filter(|p| !p.as_os_str().is_empty());
    Self::from_table(
      merged,
      cli_root,
      warnings,
      origins,
      code_trust,
      merged_providers,
      auth_config,
      fs_mounts,
      config_parent,
    )
  }

  /// 从合并后的表构造 outcome（含逐层反序列化、校验、mock 降级）。
  ///
  /// `mounts` 由调用方从**全局 settings.toml** 单独解析后传入——
  /// 项目配置无权声明 `[fs.mounts]`，故本函数不从合并表读取。
  /// `config_parent` 为已解析配置文件的所在目录（第 2/3 档）：
  /// 仅当 `harness.root` 未被任何来源显式设置时，作为工作目录默认值。
  // 私有构造器、仅 load/load_with_global 两个调用点：参数直接
  // 对应 outcome 字段，抽参数结构体不减少信息量，allow 更直接。
  #[allow(clippy::too_many_arguments)]
  fn from_table(
    table: Option<toml::Table>,
    cli_root: Option<&Path>,
    mut warnings: Vec<String>,
    mut origins: ConfigOrigins,
    code_trust: Option<CodeTrustContext>,
    merged_providers: Vec<ProviderWithOrigin>,
    auth_config: AuthConfig,
    mounts: FsMountsSchema,
    config_parent: Option<&Path>,
  ) -> LoadOutcome {
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

    // ===== 步骤④：Global 来源空 key 从 auth.toml 补充 =====
    // 配置文件来源的 api_key 已在 sourced_providers 中做过 $ENV 展开；
    // 此处只作用于 Global 来源且 trim 后为空的，且直接赋值不调 expand
    //（auth.toml 的值原样使用，即使恰为 "$FOO" 也不展开——T12）。
    let mut merged = merged_providers;
    for p in merged.iter_mut() {
      if p.origin != ConfigOrigin::Global {
        continue; // G2：Project 来源不查 auth.toml
      }
      if !p.entry.api_key.trim().is_empty() {
        continue; // 已配置（含 $ENV 展开后）不覆盖
      }
      let Some(cred) = auth_config.provider(&p.entry.provider_id) else {
        continue; // auth.toml 无对应 provider
      };
      if let Some(key) =
        cred.api_key.as_deref().filter(|k| !k.trim().is_empty())
      {
        p.entry.api_key = key.to_string(); // 原样赋值
      }
    }
    // origins 逐条记录 providers.<id>（整体键取第一个来源）
    // 兼容入口 load()（code_trust=None）不记录，保持 origins 恒空的契约。
    if code_trust.is_some() {
      let first_origin = merged.first().map(|p| p.origin);
      for p in &merged {
        origins.set(format!("providers.{}", p.entry.provider_id), p.origin);
      }
      if let Some(o) = first_origin {
        origins.set("providers", o); // 兼容键：第一个 provider 的来源
      }
    }
    provider.providers = merged.into_iter().map(|p| p.entry).collect();

    let retry: RetrySchema = load_layer(table.as_ref(), "retry", &mut warnings);
    let compression: CompressionSchema =
      load_layer(table.as_ref(), "compression", &mut warnings);
    let session: SessionSchema =
      load_layer(table.as_ref(), "session", &mut warnings);
    let tools: ToolsSchema = load_layer(table.as_ref(), "tools", &mut warnings);
    let plugins: PluginsSchema =
      load_layer(table.as_ref(), "plugins", &mut warnings);

    let mut harness: HarnessLayerSchema =
      load_layer(table.as_ref(), "harness", &mut warnings);
    if let Some(dir) = cli_root {
      harness.root = dir.to_path_buf(); // CLI 参数覆盖（概设决策 3）
      origins.set("harness.root", ConfigOrigin::Cli);
    } else if !table
      .as_ref()
      .is_some_and(|t| t.get("harness").and_then(|v| v.get("root")).is_some())
      && let Some(parent) = config_parent
    {
      // 第 2/3 档：harness.root 未被显式设置（合并表中无该键——项目
      // 文件或全局 settings 均未给出）时，工作目录 = 配置文件所在目录
      //（--config 父目录 / 向上探测命中目录）。不查 origins：兼容入口
      // 的 origins 恒空，表键检查对两条入口语义一致。
      harness.root = parent.to_path_buf();
      origins.set("harness.root", ConfigOrigin::Project);
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
      fs_mounts: mounts,
      fatal_error: None,
    };

    // ===== 无 provider：mock（release 致命）=====
    if outcome.provider.providers.is_empty() {
      #[cfg(not(debug_assertions))]
      {
        outcome.fatal_error = Some(ra_core::error::RiboError::new(
          "config",
          "no_providers_in_release",
          "no providers configured in release build",
        ));
        return outcome;
      }
      #[cfg(debug_assertions)]
      {
        outcome.warnings.push("no providers configured".to_string());
        outcome.warnings.push("running in mock mode".to_string());
        outcome.mode = ConfigMode::Mock;
        return outcome;
      }
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
          #[cfg(not(debug_assertions))]
          {
            outcome.fatal_error = Some(ra_core::error::RiboError::new(
              "config",
              "placeholder_in_release",
              "placeholder config detected in release build",
            ));
            return outcome;
          }
          #[cfg(debug_assertions)]
          {
            outcome.warnings.push("running in mock mode".to_string());
            outcome.mode = ConfigMode::Mock;
            return outcome;
          }
        }
      }
    }
    // retain_mut 提供可变引用：expand_api_key 需要原地改写 api_key（$ENV
    // 展开）。warning 先收集到局部再并入 outcome——闭包持有 providers
    // 的可变借用，直接写 outcome.warnings 会撞 E0502。
    let mut prune_warnings = Vec::new();
    outcome.provider.providers.retain_mut(|p| {
      let mut w = Vec::new();
      // Step 8：$ENV 展开已在 sourced_providers 完成；此处只 validate，
      // 避免 auth.toml 补充的值（可能恰为 "$FOO"）被二次展开（T12）。
      let ok = schema::validate_provider_entry(p, &mut w);
      prune_warnings.extend(w);
      ok
    });
    outcome.warnings.extend(prune_warnings);

    // ===== default 悬空（唯一暴露者被跳过）→ 整体 mock =====
    if outcome.provider.providers.is_empty()
      || !outcome.provider.providers.iter().any(|p| {
        p.provider_id == outcome.provider.routing.default.provider
          && p
            .models
            .iter()
            .any(|m| m.id == outcome.provider.routing.default.model)
      })
    {
      #[cfg(not(debug_assertions))]
      {
        outcome.fatal_error = Some(ra_core::error::RiboError::new(
          "config",
          "query_model_unavailable_in_release",
          "default model no longer exposed after skip (release build)",
        ));
        return outcome;
      }
      #[cfg(debug_assertions)]
      {
        outcome
          .warnings
          .push("default model no longer exposed after skip".to_string());
        outcome.warnings.push("running in mock mode".to_string());
        outcome.mode = ConfigMode::Mock;
        return outcome;
      }
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
    let before = outcome.provider.routing.fallback.len();
    outcome.provider.routing.fallback.retain(|rule| {
      let ok = registered.contains(rule.match_rule.provider.as_str())
        && rule.targets.iter().all(|t| match t {
          crate::config::schema::FallbackTargetSchema::Model {
            provider,
            ..
          } => registered.contains(provider.as_str()),
          crate::config::schema::FallbackTargetSchema::Provider {
            provider,
          } => registered.contains(provider.as_str()),
        });
      if !ok {
        prune_warnings.push(
          "fallback rule references skipped provider, pruned".to_string(),
        );
      }
      ok
    });
    if outcome.provider.routing.fallback.len() < before {
      prune_warnings.push(format!(
        "pruned {} fallback rule(s) referencing skipped providers",
        before - outcome.provider.routing.fallback.len()
      ));
    }
    outcome.warnings.extend(prune_warnings);
    outcome
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// 场景：配置路径解析（显式 --config 缺失报错 + 向上探测）。
  #[test]
  fn resolve_config_path_behavior() {
    let dir = tempfile::tempdir().expect("temp");
    let default_file = dir.path().join("ribo.toml");
    let explicit = dir.path().join("custom.toml");

    // 显式指定但文件不存在 → Err(config_file_missing)
    let err = resolve_with_default(Some(&explicit), &default_file)
      .expect_err("missing explicit config must error");
    assert_eq!(err.code(), "config_file_missing");

    // 显式指定真实存在的文件 → 原样采用
    std::fs::write(&explicit, "[harness]\nroot = \"explicit-root\"\n")
      .expect("write explicit");
    assert_eq!(
      resolve_with_default(Some(&explicit), &default_file).expect("ok"),
      Some(explicit.clone())
    );

    // 默认文件存在 → 采用（None 分支）
    std::fs::write(&default_file, "[harness]\nroot = \"probe-root\"\n")
      .expect("write");
    let resolved = resolve_with_default(None, &default_file).expect("ok");
    assert_eq!(resolved, Some(default_file.clone()));
    let outcome = LoadOutcome::load(resolved.as_deref(), None);
    assert_eq!(outcome.harness.root, std::path::Path::new("probe-root"));

    // 默认路径是目录 → 不采用（is_file 语义）
    let dir_as_default = dir.path().join("a-dir");
    std::fs::create_dir(&dir_as_default).expect("mkdir");
    assert_eq!(
      resolve_with_default(None, &dir_as_default).expect("ok"),
      None
    );
  }

  /// 场景（T1）：`--config /proj/ribo.toml` 且未显式设置 harness.root
  /// → 工作目录 = 配置文件父目录（第 2 档），origin 标记 Project。
  #[test]
  fn t1_config_parent_becomes_work_dir() {
    let dir = tempfile::tempdir().expect("temp");
    let proj = dir.path().join("proj");
    std::fs::create_dir_all(&proj).expect("mkdir proj");
    let cfg = proj.join("ribo.toml");
    std::fs::write(&cfg, "[session]\nhistory_lines = 3\n").expect("write");
    let outcome = LoadOutcome::load(Some(&cfg), None);
    assert_eq!(outcome.harness.root, proj.as_path());
    assert_eq!(outcome.origins.get("harness.root"), ConfigOrigin::Project);
  }

  /// 场景（档位校验）：配置文件显式给出 `[harness].root` → 第 1 档
  /// 文件值胜出，父目录推导不覆盖。
  #[test]
  fn t1_file_harness_root_wins_over_config_parent() {
    let dir = tempfile::tempdir().expect("temp");
    let proj = dir.path().join("proj");
    std::fs::create_dir_all(&proj).expect("mkdir proj");
    let cfg = proj.join("ribo.toml");
    std::fs::write(&cfg, "[harness]\nroot = \"./custom\"\n").expect("write");
    let outcome = LoadOutcome::load(Some(&cfg), None);
    assert_eq!(outcome.harness.root, std::path::Path::new("./custom"));
  }

  /// 场景（T1b）：向上探测命中 `ribo.toml` → 工作目录 = 命中目录
  ///（第 3 档；注入起点避免改动进程 cwd）。
  #[test]
  fn t1b_upward_probe_parent_becomes_work_dir() {
    let dir = tempfile::tempdir().expect("temp");
    let proj = dir.path().join("proj");
    let src = proj.join("src");
    std::fs::create_dir_all(&src).expect("mkdir src");
    std::fs::write(proj.join("ribo.toml"), "").expect("write");
    let found =
      find_config_upwards_from(&src, Path::new("ribo.toml")).expect("hit");
    assert_eq!(found, proj.join("ribo.toml"));
    let outcome = LoadOutcome::load(Some(&found), None);
    assert_eq!(outcome.harness.root, proj.as_path());
  }

  /// 场景（第 4 档）：零配置（无 --config、探测未命中）→ 工作目录
  /// 保持 cwd（`.`），origin 为 Default。
  #[test]
  fn t1_no_config_falls_back_to_cwd() {
    let outcome = LoadOutcome::load(None, None);
    assert_eq!(outcome.harness.root, std::path::Path::new("."));
    assert_eq!(outcome.origins.get("harness.root"), ConfigOrigin::Default);
  }

  /// 场景（T10）：日志根——全局域启用 → 全局目录；未启用 → 项目
  /// `.ribo/`（logging 层拼 `logs/`，最终 `.ribo/logs/`）。
  #[test]
  fn t10_log_root_global_or_project_ribo() {
    let dir = tempfile::tempdir().expect("temp");
    let global_root = dir.path().join("global");
    std::fs::create_dir_all(&global_root).expect("mkdir global");
    let proj_root = dir.path().join("proj");

    // 未启用全局域（code_trust = None）→ 项目 .ribo/
    let mut outcome = LoadOutcome::load(None, None);
    outcome.harness.root = proj_root.clone();
    assert_eq!(outcome.log_root(), proj_root.join(".ribo"));

    // 全局域启用 → 全局目录
    let ctx = CodeTrustContext::new(trust::TrustedProjects::empty())
      .with_global_root(&global_root);
    outcome.code_trust = Some(ctx);
    assert_eq!(outcome.log_root(), global_root);
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
unknown_top_level = 1

[routing]
default = { provider = "chat-primary", model = "glm-4.5-flash" }

[[providers]]
provider_id = "chat-primary"
adapter = "openai-chat"
base_url = "https://real"
api_key = "sk-1"

  [[providers.models]]
  id = "glm-4.5-flash"

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
    assert!(outcome.fs_mounts.mounts.is_empty());
    assert_eq!(outcome.session.history_lines, 5);
    assert_eq!(outcome.provider.providers.len(), 1);
    assert_eq!(outcome.provider.providers[0].models[0].context_window, 8192); // 缺省

    // 显式 context_window → 落位（状态行 ctx 分母 / 压缩预算共用）
    std::fs::write(
      &path,
      r#"
[routing]
default = { provider = "p", model = "m" }

[[providers]]
provider_id = "p"
adapter = "openai-chat"
base_url = "https://real"
api_key = "sk-1"

  [[providers.models]]
  id = "m"
  context_window = 131072
"#,
    )
    .expect("write");
    let outcome = LoadOutcome::load(Some(&path), None);
    assert_eq!(
      outcome.provider.providers[0].models[0].context_window,
      131072
    );

    // 坏层（compression 写成字符串）→ 保留默认 + warning，其余层正常
    std::fs::write(
      &path,
      r#"
compression = "broken"

[routing]
default = { provider = "chat-primary", model = "glm-4.5-flash" }

[[providers]]
provider_id = "chat-primary"
adapter = "openai-chat"
base_url = "https://real"
api_key = "sk-1"

  [[providers.models]]
  id = "glm-4.5-flash"
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

  /// 场景：$ENV 密钥、占位回退、默认模型悬空、--root 覆盖（CFG-02）。
  #[test]
  fn load_env_key_placeholder_and_root_override() {
    let dir = tempfile::tempdir().expect("temp");

    // 占位 → 整体 Mock
    let path = dir.path().join("p1.toml");
    std::fs::write(
      &path,
      r#"
[routing]
default = { provider = "p1", model = "m" }

[[providers]]
provider_id = "p1"
adapter = "openai-chat"
base_url = "https://api.example.com"
api_key = "k"

  [[providers.models]]
  id = "m"
"#,
    )
    .expect("write");
    let outcome = LoadOutcome::load(Some(&path), None);
    assert_eq!(outcome.mode, ConfigMode::Mock);
    assert!(outcome.warnings.iter().any(|w| w.contains("placeholder")));

    // $ENV 未设置 → provider 跳过 → default 悬空 → Mock
    let path = dir.path().join("p2.toml");
    std::fs::write(
      &path,
      r#"
[routing]
default = { provider = "p1", model = "m" }

[[providers]]
provider_id = "p1"
adapter = "openai-chat"
base_url = "https://real"
api_key = "$RIBO_CLI_MISSING_KEY"

  [[providers.models]]
  id = "m"
"#,
    )
    .expect("write");
    let outcome = LoadOutcome::load(Some(&path), None);
    assert_eq!(outcome.mode, ConfigMode::Mock);
    assert!(
      outcome
        .warnings
        .iter()
        .any(|w| w.contains("default model no longer exposed"))
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
[routing]
default = { provider = "p1", model = "m" }

[[providers]]
provider_id = "p1"
adapter = "openai-chat"
base_url = "https://real"
api_key = "$RIBO_CLI_CFG_KEY"

  [[providers.models]]
  id = "m"
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

  /// 场景：全局域目录缺失但项目配置正常（故障隔离）。
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

  /// 场景：项目配置声明 `[fs.mounts]`（越权）→ warning + 忽略；
  /// 全局 settings.toml 声明的挂载正常生效。
  #[test]
  fn project_fs_declaration_is_ignored_with_warning() {
    let dir = tempfile::tempdir().expect("temp");
    let global_root = dir.path().join("global");
    std::fs::create_dir_all(&global_root).expect("mkdir global");
    let mount_src = tempfile::tempdir().expect("mount temp");
    // TOML 字面量字符串（单引号）防 Windows 路径反斜杠转义
    std::fs::write(
      global_root.join("settings.toml"),
      format!("[fs.mounts]\ndatasets = '{}'\n", mount_src.path().display()),
    )
    .expect("write global settings");
    let project = dir.path().join("ribo.toml");
    std::fs::write(&project, "[fs.mounts]\nhack = '/etc'\n")
      .expect("write project");

    let outcome =
      LoadOutcome::load_with_global(Some(&project), None, Some(&global_root));
    // 全局声明的挂载生效；项目声明的被忽略
    assert!(outcome.fs_mounts.mounts.contains_key("datasets"));
    assert!(!outcome.fs_mounts.mounts.contains_key("hack"));
    assert!(
      outcome
        .warnings
        .iter()
        .any(|w| w.contains("ignored") && w.contains("global settings.toml")),
      "warnings: {:?}",
      outcome.warnings
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

  /// T14/T15：无 provider —— debug 降级 mock / release 致命
  #[test]
  fn d7_no_providers_mode() {
    let outcome = LoadOutcome::load(None, None);
    #[cfg(debug_assertions)]
    {
      assert_eq!(outcome.mode, ConfigMode::Mock, "debug keeps mock fallback");
      assert!(outcome.fatal_error.is_none());
    }
    #[cfg(not(debug_assertions))]
    {
      let err = outcome.fatal_error.expect("release must be fatal");
      assert_eq!(err.code(), "no_providers_in_release");
    }
  }

  /// T18：单 provider 缺 key 被跳过 —— 两模式都保留 warning
  #[test]
  fn d7_partial_provider_skip_never_fatal() {
    let dir = tempfile::tempdir().expect("temp");
    let path = dir.path().join("ribo.toml");
    std::fs::write(
      &path,
      r#"
[routing]
default = { provider = "good", model = "m" }

[[providers]]
provider_id = "bad"
adapter = "openai-chat"
base_url = "https://x.invalid"
api_key = "your-key-placeholder"

[[providers]]
provider_id = "good"
adapter = "openai-chat"
base_url = "https://ok.invalid"
api_key = "sk-real"

  [[providers.models]]
  id = "m"
"#,
    )
    .expect("write");
    let outcome = LoadOutcome::load(Some(&path), None);
    assert!(
      outcome.fatal_error.is_none(),
      "single skip must never be fatal"
    );
  }
}
