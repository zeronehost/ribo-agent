//! 分层配置 schema：每层独立、层间零交叉，聚合层负责拼接与校验
//!（14 详细设计 §4；默认模型为 `[routing].default` 二元组，24 号方案 D1）。

use std::path::PathBuf;

use serde::Deserialize;

/// global 层：全局域开关与目录（对应设计§8.1）。
///
/// `enabled` 默认 `true`，但全局目录不存在时自动禁用（非致命，R4：不主动创建）。
/// `dir` 由 `--global-dir` 覆盖；未设置时用平台默认配置目录 + `/ribo`。
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct GlobalSchema {
  /// 是否启用全局域（默认 `true`；目录不存在时仍会自动禁用）。
  pub enabled: bool,
  /// 全局域目录；`None` 表示走平台默认配置目录。
  pub dir: Option<PathBuf>,
}

impl Default for GlobalSchema {
  fn default() -> Self {
    Self {
      enabled: true,
      dir: None,
    }
  }
}

/// provider 层（24 号方案 v1.3）：默认模型为 `[routing].default` 二元组，
/// `[[providers]]` 条目经 `models` 数组声明多模型。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ProviderSchema {
  /// 提供方列表。
  pub providers: Vec<ProviderEntry>,
  /// routing 段（默认二元组 + 降级规则）。
  pub routing: RoutingSchema,
}

impl Default for ProviderSchema {
  /// 默认 = mock 模式：无 provider、default 指向 mock 通道。
  fn default() -> Self {
    Self {
      providers: Vec::new(),
      routing: RoutingSchema::default(),
    }
  }
}

/// 默认模型二元组（D1）。
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct RoutingRef {
  /// 提供方 ID。
  pub provider: String,
  /// 模型 ID。
  pub model: String,
}

/// `[[providers]]` 单条目（通道声明：base_url / api_key / adapter）。
#[derive(Debug, Clone, Deserialize)]
pub struct ProviderEntry {
  /// 提供方 ID（用于注册与路由引用）。
  pub provider_id: String,
  /// 适配器类型：`openai-chat` | `openai-responses` | `anthropic`。
  pub adapter: String,
  /// 基础端点（适配器自动拼接路径）。
  pub base_url: String,
  /// 支持 `$ENV_NAME` 占位（解析期读取环境变量）。
  /// 缺省为空串（Step 8）：Global 来源从 auth.toml 补、Project 必须显式。
  #[serde(default)]
  pub api_key: String,
  /// 该通道可用的模型清单（D2，必填非空）：声明顺序 = same_provider
  /// 隐式备选序（D3）；第一个为该通道默认模型。
  pub models: Vec<ModelEntry>,
}

/// 单模型声明（D2）：窗口随模型走。
#[derive(Debug, Clone, Deserialize)]
pub struct ModelEntry {
  /// 真实模型 ID：同时作为网关模型 ID 与请求线 `model` 字段。
  pub id: String,
  /// 模型上下文窗口（token）：状态行 ctx 占比分母与压缩阈值共用。
  #[serde(default = "default_context_window")]
  pub context_window: u32,
}

/// `context_window` 缺省值（与装配历史基线一致）。
fn default_context_window() -> u32 {
  8192
}

/// 带来源标记的 provider 声明（Step 8 / D1，内部类型）。
///
/// `pub(crate)` 当前足够（只在 `ra-cli::config` 内部使用）；若未来需跨
/// crate 暴露（如插件系统需知道 provider 来源），再升为 `pub`。
#[derive(Debug, Clone)]
pub(crate) struct ProviderWithOrigin {
  /// provider 声明本体。
  pub entry: ProviderEntry,
  /// 来源域（Global / Project）。
  pub origin: crate::config::origin::ConfigOrigin,
}

/// 从 TOML 表解析 `[[providers]]` 数组（Step 8 / P1-3，新增辅助函数）。
///
/// 单条解析失败的条目静默跳过（与 `load_layer` 的局部降级语义一致）；
/// `providers` 键缺失或非数组时返回空列表。
pub(crate) fn providers_from_table(table: &toml::Table) -> Vec<ProviderEntry> {
  let Some(array) = table.get("providers").and_then(|v| v.as_array()) else {
    return Vec::new();
  };
  array
    .iter()
    .filter_map(|v| v.clone().try_into().ok())
    .collect()
}

/// routing 段（24 号方案 v1.3）：默认二元组 + 统一降级规则列表。
#[derive(Debug, Clone, Deserialize)]
pub struct RoutingSchema {
  /// 默认模型二元组（D1）：`(provider, model)` 必须存在于 providers 的
  /// models 中（装配期校验）。
  pub default: RoutingRef,
  /// 降级规则列表（D4）：按声明顺序，首条 `match` 命中生效。
  #[serde(default)]
  pub fallback: Vec<FallbackRuleSchema>,
}

impl Default for RoutingSchema {
  /// 默认 = mock 通道（ProviderSchema::default 的 mock 模式配套）。
  fn default() -> Self {
    Self {
      default: RoutingRef {
        provider: "mock".to_string(),
        model: "mock-model".to_string(),
      },
      fallback: Vec::new(),
    }
  }
}

/// `[[routing.fallback]]` 单条规则：`match` → `targets`（D4）。
#[derive(Debug, Clone, Deserialize)]
pub struct FallbackRuleSchema {
  /// 匹配条件（TOML 键 `match`；Rust 保留字用 rename）。
  #[serde(rename = "match")]
  pub match_rule: FallbackMatchSchema,
  /// 有序目标列表（必填；缺失 serde 失败、空数组装配期拒绝）。
  pub targets: Vec<FallbackTargetSchema>,
}

/// 规则匹配条件两形态（D4）：`{ provider, model }` 精确 / `{ provider }` 通配。
#[derive(Debug, Clone, Deserialize)]
pub struct FallbackMatchSchema {
  /// 匹配的提供方 ID。
  pub provider: String,
  /// 匹配的模型 ID；`None` = provider 级通配。
  #[serde(default)]
  pub model: Option<String>,
}

/// 目标两形态（D4，untagged）：
/// `{ provider, model }` 显式二元组 / `{ provider }` 两级解析（同名→目标默认）。
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum FallbackTargetSchema {
  /// 显式二元组（跨命名体系精确降级）。
  Model { provider: String, model: String },
  /// 仅指定通道（同名 → 目标默认）。
  Provider { provider: String },
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
  /// 滞回比率（默认 1.10）：估算需达到上次触发的该倍数才重算。
  pub hysteresis_ratio: f64,
  /// 触发压缩后的冷却轮数（默认 2）：冷却期复用摘要、不调用 LLM。
  pub cooldown_turns: u32,
  /// 是否启用增量压缩（默认 true）；关闭则每次全量摘要。
  pub enable_incremental: bool,
  /// 每 N 次增量后强制全量重建（默认 8，范围 [1,32]；1 = 每次全量）。
  pub incremental_rebuild_every: u32,
  /// 摘要输入 token 上限（默认 0 = 自动取模型窗口 70%）。
  pub max_summary_input_tokens: usize,
}

impl Default for CompressionSchema {
  fn default() -> Self {
    Self {
      enabled: false,
      trigger_ratio: 0.75,
      keep_recent_turns: 4,
      summarizer_model: None,
      max_summary_chars: 2048,
      hysteresis_ratio: 1.10,
      cooldown_turns: 2,
      enable_incremental: true,
      incremental_rebuild_every: 8,
      max_summary_input_tokens: 0,
    }
  }
}

/// session 层：REPL 会话行为。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SessionSchema {
  /// 初始会话键（`/sessions <名称>` 切换基准）。
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
}

impl Default for ToolsSchema {
  fn default() -> Self {
    Self {
      builtin_enabled: true,
    }
  }
}

/// `fs` 层：项目外只读挂载点（v3 取代资源域）。
///
/// 仅全局 settings.toml 声明；名字 → 绝对路径。项目配置无权声明
/// （项目内路径本就可读，项目外路径的暴露决定权只属用户）。
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct FsMountsSchema {
  /// 挂载名 → 绝对路径（校验见 `assemble.rs::validate_fs_mounts`）。
  pub mounts: std::collections::HashMap<String, PathBuf>,
}

/// harness 层：存储与技能目录（概设双真值裁决：不含 compaction）。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct HarnessLayerSchema {
  /// 工作空间根目录（会话存储、日志、技能扫描与内置工具沙箱的统一
  /// 基准；默认 `.`（进程 cwd）；`--root` 参数可覆盖）。
  pub root: PathBuf,
  /// 环境段注入开关（默认 true，与 `ContextConfig::default()` 对齐）。
  pub include_environment: bool,
}

impl Default for HarnessLayerSchema {
  fn default() -> Self {
    Self {
      root: PathBuf::from("."),
      include_environment: true,
    }
  }
}

/// plugins 层：WASM 插件装配声明（阶段 6 Step B 接入，18_ §4.1）。
///
/// 层间零交叉纪律：本层只描述「加载什么文件、授予什么能力」，与
/// [`ra_plugins::acl::PluginAcl`] 的运行时转换在装配层（assemble）完成；
/// 能力语义与 18_ §2.1 一致——ACL 由宿主注入，不信任 guest 自述。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PluginsSchema {
  /// 是否启用插件装配（默认 false：不扫描、不加载，现有配置与 TUI
  /// 测试零行为变化；也未触碰 `PluginRuntime` 的 wasmtime 初始化）。
  pub enabled: bool,
  /// 显式插件清单（相对 harness.root 解析），可逐插件声明 ACL 与 config。
  pub entries: Vec<PluginEntrySchema>,
  /// 来源校验策略（Step C 签名校验）：生产须 `require_signature = true`
  /// 并提供非空公钥白名单；默认关闭，保持既有配置零行为变化。
  pub verify: PluginVerifySchema,
}

impl Default for PluginsSchema {
  /// 默认 = 关闭。
  fn default() -> Self {
    Self {
      enabled: false,
      entries: Vec::new(),
      verify: PluginVerifySchema::default(),
    }
  }
}

/// 校验 fs 挂载名 / 域 id 是否匹配 `^[a-z][a-z0-9_]{0,31}$`（纯 std，无需 regex 依赖）。
/// 资源域已移除，本函数由 `[fs.mounts]` 挂载名校验复用。
pub fn is_valid_domain_id(id: &str) -> bool {
  let mut chars = id.chars();
  if !chars.next().is_some_and(|c| c.is_ascii_lowercase()) {
    return false;
  }
  let tail: Vec<char> = chars.collect();
  tail.len() <= 31
    && tail
      .iter()
      .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_')
}

/// `[plugins.verify]` 来源校验配置。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PluginVerifySchema {
  /// 是否强制每条插件声明提供有效 Ed25519 签名与权威清单。
  pub require_signature: bool,
}

/// `[[plugins.entries]]` 显式清单单条目。
#[derive(Debug, Clone, Deserialize)]
pub struct PluginEntrySchema {
  /// wasm 模块文件路径（相对 harness.root 解析；绝对路径与含 `..`
  /// 逃逸组件的取值在装配期直接拒绝——与 fs 工具同一沙箱纪律）。
  pub path: PathBuf,
  /// 注入插件的 config（经 args 信封在每次 `execute` 生效，18_ §9）。
  pub config: Option<serde_json::Value>,
  /// 权威清单文件（Step C B3.2，可选）：相对 harness.root 的
  /// manifest.json（与 guest ABI 同构）。提供时宿主以其为信任源，
  /// guest `list_tools()` 自述与之不符即拒绝加载；缺省维持 Step B
  /// 语义（采信自述，开发 / CI 模式）。
  pub manifest: Option<PathBuf>,
  /// 侧载签名块（provenance.json，相对 harness.root）：含签名者 ID、
  /// Ed25519 签名与 wasm 摘要；仅在 `manifest` 存在时有意义。生产策略下
  /// 缺签名块即启动致命。
  pub provenance: Option<PathBuf>,
  /// 本插件被授予的能力声明（映射为 `PluginAcl`；默认全关）。
  #[serde(default)]
  pub acl: PluginAclSchema,
}

/// 插件能力声明（schema 层镜像 `ra_plugins::acl::PluginAcl` 的可配置子集）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PluginAclSchema {
  /// 允许调用 `log` host function。
  pub log: bool,
  /// 允许调用 `storage_get` / `storage_put` host function。
  pub storage: bool,
  /// 允许调用 `http_proxy` host function（须同时提供非空 `http_hosts`）。
  pub http: bool,
  /// http 出站域名白名单（大小写不敏感；仅 `http = true` 时生效）。
  pub http_hosts: Vec<String>,
  /// 线性内存覆写（字节；只能收紧进程默认 32MB，放大被 min 掉）。
  pub memory_limit_bytes: Option<usize>,
  /// 单插件 storage 总配额覆写（字节；只能收紧进程默认 1MB）。
  pub storage_quota_bytes: Option<usize>,
  /// 单工具执行超时上限覆写（毫秒；只能收紧，与进程 60s ceiling 取 min）。
  pub timeout_ceiling_ms: Option<u64>,
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
  {
    warnings.push(format!("provider entry incomplete: {}", entry.provider_id));
    return false;
  }
  // models 必填非空且各 id 非空（D2：空通道无意义）
  if entry.models.is_empty() {
    warnings.push(format!("provider models empty: {}", entry.provider_id));
    return false;
  }
  if entry.models.iter().any(|m| m.id.trim().is_empty()) {
    warnings.push(format!(
      "provider models contain empty id: {}",
      entry.provider_id
    ));
    return false;
  }
  let mut seen = std::collections::HashSet::new();
  if !entry.models.iter().all(|m| seen.insert(m.id.as_str())) {
    warnings.push(format!(
      "provider models duplicate id: {}",
      entry.provider_id
    ));
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

  /// 场景：`[plugins]` 条目与 `[fs.mounts]` 只读挂载点共存解析。
  #[test]
  fn plugins_and_fs_mounts_coexist() {
    let text = r#"
[plugins]
enabled = true
[[plugins.entries]]
path = "foo.wasm"
[fs.mounts]
datasets = "/mnt/datasets"
refs = "/opt/refs"
"#;
    let table: toml::Table = toml::from_str(text).expect("valid TOML");
    let parsed: PluginsSchema = table["plugins"]
      .clone()
      .try_into()
      .expect("plugins table is valid");
    let mounts: FsMountsSchema =
      table["fs"].clone().try_into().expect("fs table is valid");
    assert!(parsed.enabled);
    assert_eq!(parsed.entries.len(), 1);
    assert_eq!(mounts.mounts.len(), 2);
    assert_eq!(
      mounts.mounts["datasets"],
      std::path::PathBuf::from("/mnt/datasets")
    );
  }

  /// 场景：域 id 正则 `^[a-z][a-z0-9_]{0,31}$`。
  #[test]
  fn domain_id_regex_is_enforced() {
    assert!(is_valid_domain_id("dataset"));
    assert!(is_valid_domain_id("a"));
    assert!(is_valid_domain_id("company_plugins2"));
    assert!(is_valid_domain_id(&"a".repeat(32)));
    assert!(!is_valid_domain_id(""));
    assert!(!is_valid_domain_id("Dataset"));
    assert!(!is_valid_domain_id("_dataset"));
    assert!(!is_valid_domain_id("data-set"));
    assert!(!is_valid_domain_id(&"a".repeat(33)));
  }

  /// 场景：`[[providers.models]]` 解析与 `context_window` 缺省值（D2）。
  ///
  /// 预期：显式值落位；未配置时取 8192（历史演示基线）；多模型按
  /// 声明序排列。
  #[test]
  fn context_window_parses_with_default() {
    let table: toml::Table = r#"
[[providers]]
provider_id = "p"
adapter = "openai-chat"
base_url = "https://real"
api_key = "k"

  [[providers.models]]
  id = "m1"
  context_window = 131072

  [[providers.models]]
  id = "m2"

[[providers]]
provider_id = "q"
adapter = "openai-chat"
base_url = "https://real"
api_key = "k"

  [[providers.models]]
  id = "m"
"#
    .parse()
    .expect("parse");
    let providers = providers_from_table(&table);
    assert_eq!(providers[0].models[0].context_window, 131072);
    assert_eq!(providers[0].models[1].context_window, 8192);
    assert_eq!(providers[1].models[0].id, "m");
  }

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
      models: vec![ModelEntry {
        id: "m".into(),
        context_window: 8192,
      }],
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
