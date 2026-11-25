//! schema → 运行时对象装配：Gateway / LoopDeps / Harness / CliSession。

use std::{
  collections::HashSet,
  path::{Path, PathBuf},
  sync::Arc,
  time::Duration,
};

use ra_agent::{
  adapter::ToolManagerAdapter, agent::BaseAgent, traits::ContextProvider,
  types::LoopDeps,
};
use ra_core::{
  error::RiboResult,
  path::{is_lexically_safe, is_within},
};
use ra_harness::{
  CompactionConfig, Compactor, ContextConfig, Harness, HarnessContext,
  InstructionRoot, JsonlSessionStore, PersistentSessionProvider, SkillRoot,
  SkillsLoader, SystemPromptBuilder, load_instructions,
};
use ra_plugins::acl::{PluginAcl, UrlPolicy};
use ra_plugins::provenance::{
  SignedAuthority, TrustedKeys, VerificationPolicy,
};
use ra_plugins::registry::{PluginRegistry, plugin_aware_tool_manager_config};
use ra_plugins::wasm::host::WasmPluginHost;
use ra_provider::{
  adapters::{
    anthropic::AnthropicProvider, mock::MockStep,
    openai::completion::OpenAiCompatibleAdapter,
    openai::responses::OpenAiResponsesAdapter,
  },
  gateway::{
    FallbackMatch, FallbackRule, FallbackTarget, Gateway, GatewayConfig,
    RouteMap,
  },
  ids::{ModelId, ProviderId},
  model::Model,
  retry::RetryPolicy,
};
use ra_tools::manager::ToolManager;

use super::{ConfigMode, LoadOutcome, PluginAclSchema};
use crate::config::schema::FallbackTargetSchema;

use ra_tools::builtin::FsRoot;

/// CLI 内置系统提示模板（不提供配置入口）。
const DEFAULT_CLI_TEMPLATE: &str = "\
You are a capable AI agent operating in a terminal workspace. \
Use the available tools to inspect files and run commands; \
prefer minimal, verifiable changes and report what you did.";

/// mock 模式的模型名（与 `ProviderSchema::default()` 对齐）。
const MOCK_MODEL: &str = "mock-model";

/// 装配期配置域：装配层自造错误的 `RiboError::domain`（错误码以
/// `config.*` 集中管理；插件自身的 `plugin.plugin_*` 错误由
/// ra-plugins 的错误域产生，不经此处）。
const CONFIG_DOMAIN: &str = "config";

/// REPL 唯一运行时句柄（命名避开 `ra_agent::session::Session`）。
/// `harness` 为 `Arc<Harness>`：`prompt(&self)` 需跨 spawn 任务共享
///（Harness 全 Arc 字段，包装零成本）。
pub struct CliSession {
  pub harness: Arc<Harness>,
  pub events: tokio::sync::broadcast::Receiver<ra_agent::events::AgentEvent>,
  pub mode: ConfigMode,
  pub warnings: Vec<String>,
  /// 状态行数据：当前主模型归属的 provider（装配期取自 `[routing].default`）。
  pub provider_id: String,
  /// 状态行数据：当前主模型名。
  pub model_name: String,
  /// 主模型上下文窗口（状态行 ctx 占比的分母；0 = 未知）。
  pub context_window: u32,
  /// `/history` 默认条数等 REPL 行为参数。
  pub history_lines: usize,
  /// 初始会话键（session 层 `default_key`；`/sessions <名称>` 切换基准）。
  pub default_key: String,
  /// 装配期加载的技能名（`/skills` 输出）。
  pub skills: Vec<String>,
  /// 每个技能的来源域标签（与 `skills` 平行）。
  pub skill_origins: Vec<String>,
  /// 装配期技能诊断（已格式化为单行，`/skills` 输出）。
  pub skill_diagnostics: Vec<String>,
  /// 装配期插件注册表（`plugins.enabled = false` 时为 `None`，且进程
  /// 未触碰 wasmtime 初始化）。插件本体由 `PluginToolAdapter` 经
  /// `ToolManager` 持有，此处保留引用以支撑后续热重载与状态查询。
  pub plugin_registry: Option<Arc<PluginRegistry>>,
  /// 装配期加载成功的插件 ID（启动横幅 / 装配摘要展示）。
  pub loaded_plugins: Vec<String>,
}

/// 解析示例用模型 ID。
fn mid(raw: &str) -> ModelId {
  ModelId::parse(raw).expect("valid model id")
}

/// 会话持久化目录（硬切换——只写/只读 `.ribo/sessions/`，
/// 旧布局 `<root>/sessions/` 不读取、不迁移、不兼容）。
pub(crate) fn sessions_dir(root: &Path) -> PathBuf {
  root.join(".ribo/sessions")
}

/// 解析示例用提供方 ID。
fn pid(raw: &str) -> ProviderId {
  ProviderId::parse(raw).expect("valid provider id")
}

/// 构造满足网关注册校验的模型。
///
/// `context_window` 由调用方给出：真实模式取 `[providers].context_window`
///（缺省 8192），mock 模式用演示窗口；该值同时作为状态行 ctx 占比分母。
fn model_of(
  provider_id: &str,
  model_name: &str,
  base_url: impl Into<String>,
  context_window: u32,
) -> Model {
  Model::new(
    pid(provider_id),
    mid(model_name),
    model_name.to_string(),
    base_url.into(),
  )
  .with_context_window(context_window)
  .with_max_tokens(1024)
}

/// 装配期 compression 配置校验。
///
/// 校验数值范围与摘要模型注册；错误码 `config.compression_invalid`。
/// 仅 Real 模式校验摘要模型注册（mock 模式无真实 provider 目录）。
///
/// # Errors
///
/// 任一范围非法或摘要模型未注册 → `config.compression_invalid` fail-fast。
fn validate_compression(outcome: &LoadOutcome) -> RiboResult<()> {
  const CODE: &str = "compression_invalid";
  let compression = &outcome.compression;
  let invalid = |reason: String| {
    ra_core::error::RiboError::new(CONFIG_DOMAIN, CODE, reason)
  };

  if compression.trigger_ratio.is_nan()
    || !(0.0..=1.0).contains(&compression.trigger_ratio)
  {
    return Err(invalid(format!(
      "trigger_ratio must be in (0, 1], got {}",
      compression.trigger_ratio
    )));
  }
  if compression.hysteresis_ratio.is_nan()
    || !(1.0..=10.0).contains(&compression.hysteresis_ratio)
  {
    return Err(invalid(format!(
      "hysteresis_ratio must be in [1.0, 10.0], got {}",
      compression.hysteresis_ratio
    )));
  }
  if compression.cooldown_turns > 100 {
    return Err(invalid(format!(
      "cooldown_turns must be <= 100, got {}",
      compression.cooldown_turns
    )));
  }
  if !(1..=32).contains(&compression.incremental_rebuild_every) {
    return Err(invalid(format!(
      "incremental_rebuild_every must be in [1, 32], got {}",
      compression.incremental_rebuild_every
    )));
  }

  // 摘要模型注册校验（仅 Real 模式；用目录查询而非仅 ModelId::parse）
  if outcome.mode == ConfigMode::Real
    && let Some(summarizer) = &compression.summarizer_model
  {
    let registered = outcome
      .provider
      .providers
      .iter()
      .any(|provider| provider.models.iter().any(|m| m.id == *summarizer));
    if !registered {
      return Err(invalid(format!(
        "summarizer_model `{summarizer}` is not registered by any provider"
      )));
    }
  }
  Ok(())
}

/// 装配期 routing 引用校验：
///
/// - `[routing].default` 的 `(provider, model)` 必须存在于某条目的 models；
/// - fallback 规则的 `match.provider` / `targets[].provider` 必须已注册；
/// - 规则中出现的 `model` 必须存在于对应 provider 的 models；
/// - 通配规则遮蔽后序精确规则 → shadowing warning（不拒绝）。
///
/// 任一引用悬空 → `config.routing_invalid`（fail-fast）。
fn validate_routing_refs(outcome: &LoadOutcome) -> RiboResult<()> {
  const CODE: &str = "config.routing_invalid";
  let registered: std::collections::HashMap<&str, Vec<&str>> = outcome
    .provider
    .providers
    .iter()
    .map(|p| {
      (
        p.provider_id.as_str(),
        p.models.iter().map(|m| m.id.as_str()).collect(),
      )
    })
    .collect();
  // default 二元组存在性
  let default_ok = registered
    .get(outcome.provider.routing.default.provider.as_str())
    .is_some_and(|models| {
      models.contains(&outcome.provider.routing.default.model.as_str())
    });
  if !default_ok {
    return Err(ra_core::error::RiboError::new(
      "config",
      CODE,
      format!(
        "[routing].default references unknown provider/model: {}/{}",
        outcome.provider.routing.default.provider,
        outcome.provider.routing.default.model
      ),
    ));
  }
  // fallback 规则引用 + shadowing
  for (index, rule) in outcome.provider.routing.fallback.iter().enumerate() {
    if !registered.contains_key(rule.match_rule.provider.as_str()) {
      return Err(ra_core::error::RiboError::new(
        "config",
        CODE,
        format!(
          "fallback rule #{} matches unknown provider '{}'",
          index + 1,
          rule.match_rule.provider
        ),
      ));
    }
    if let Some(model) = &rule.match_rule.model
      && !registered
        .get(rule.match_rule.provider.as_str())
        .is_some_and(|models| models.contains(&model.as_str()))
    {
      return Err(ra_core::error::RiboError::new(
        "config",
        CODE,
        format!(
          "fallback rule #{} matches unknown model '{}/{}'",
          index + 1,
          rule.match_rule.provider,
          model
        ),
      ));
    }
    if rule.targets.is_empty() {
      return Err(ra_core::error::RiboError::new(
        "config",
        CODE,
        format!("fallback rule #{} has empty targets", index + 1),
      ));
    }
    for target in &rule.targets {
      let (provider, model) = match target {
        FallbackTargetSchema::Model { provider, model } => {
          (provider.as_str(), Some(model.as_str()))
        }
        FallbackTargetSchema::Provider { provider } => {
          (provider.as_str(), None)
        }
      };
      let Some(models) = registered.get(provider) else {
        return Err(ra_core::error::RiboError::new(
          "config",
          CODE,
          format!(
            "fallback rule #{} targets unknown provider '{provider}'",
            index + 1
          ),
        ));
      };
      if let Some(model) = model
        && !models.contains(&model)
      {
        return Err(ra_core::error::RiboError::new(
          "config",
          CODE,
          format!(
            "fallback rule #{} targets unknown model '{provider}/{model}'",
            index + 1
          ),
        ));
      }
    }
  }
  // shadowing 检测（warning 不拒绝，伪码）
  let rules = &outcome.provider.routing.fallback;
  for (i, prev) in rules.iter().enumerate() {
    for (j, next) in rules.iter().enumerate().skip(i + 1) {
      let provider_covered =
        prev.match_rule.provider == next.match_rule.provider;
      let model_covered = match (&prev.match_rule.model, &next.match_rule.model)
      {
        (None, _) => true,
        (Some(p), Some(n)) => p == n,
        (Some(_), None) => false,
      };
      if provider_covered && model_covered {
        tracing::warn!(
          shadowing_rule = j + 1,
          shadowed_by = i + 1,
          "fallback rule #{} is shadowed by rule #{}",
          j + 1,
          i + 1
        );
      }
    }
  }
  Ok(())
}

/// 真实模式网关：逐条构造适配器注册 + 降级路由 + retry 参数。
fn build_real_gateway(
  outcome: &LoadOutcome,
) -> RiboResult<(Arc<Gateway>, Model)> {
  let cfg = GatewayConfig {
    retry: RetryPolicy {
      initial_backoff: Duration::from_millis(outcome.retry.initial_backoff_ms),
      max_retries: outcome.retry.max_retries,
      jitter_ratio: outcome.retry.jitter_ratio,
      ..RetryPolicy::default()
    },
    ..GatewayConfig::default()
  };
  let mut gateway = Gateway::new(cfg);
  for entry in &outcome.provider.providers {
    // 多模型注册：models 全量进通道目录
    let adapter_models: Vec<Model> = entry
      .models
      .iter()
      .map(|m| {
        model_of(
          &entry.provider_id,
          &m.id,
          entry.base_url.clone(),
          m.context_window,
        )
      })
      .collect();
    let api_key = Some(entry.api_key.clone());
    // 三个分支构造签名一致（id/models/http/api_key），差异在协议端点与
    // 请求/响应编解码——按 adapter 声明走各自的适配器
    let adapter = match entry.adapter.as_str() {
      "openai-chat" => Arc::new(OpenAiCompatibleAdapter::new(
        pid(&entry.provider_id),
        adapter_models.clone(),
        reqwest::Client::new(),
        api_key,
      )) as Arc<dyn ra_provider::traits::Provider>,
      "openai-responses" => Arc::new(OpenAiResponsesAdapter::new(
        pid(&entry.provider_id),
        adapter_models.clone(),
        reqwest::Client::new(),
        api_key,
      )) as Arc<dyn ra_provider::traits::Provider>,
      "anthropic" => Arc::new(AnthropicProvider::new(
        pid(&entry.provider_id),
        adapter_models,
        reqwest::Client::new(),
        api_key,
      )) as Arc<dyn ra_provider::traits::Provider>,
      other => unreachable!("adapter 白名单已在 load 阶段拦截：{other}"),
    };
    gateway.register(adapter)?;
  }
  let mut routes = RouteMap::default();
  // 隐式同 provider 链（models 声明序，其余模型）
  for entry in &outcome.provider.providers {
    for (index, m) in entry.models.iter().enumerate() {
      let chain: Vec<ModelId> = entry
        .models
        .iter()
        .enumerate()
        .filter(|(j, _)| *j != index)
        .map(|(_, x)| mid(&x.id))
        .collect();
      routes.insert_same_provider(mid(&m.id), chain);
    }
  }
  // 统一 fallback 规则列表
  let rules: Vec<FallbackRule> = outcome
    .provider
    .routing
    .fallback
    .iter()
    .map(|r| FallbackRule {
      match_: FallbackMatch {
        provider: pid(&r.match_rule.provider),
        model: r.match_rule.model.as_ref().map(|m| mid(m)),
      },
      targets: r
        .targets
        .iter()
        .map(|t| match t {
          FallbackTargetSchema::Model { provider, model } => {
            FallbackTarget::Model {
              provider: pid(provider),
              model: mid(model),
            }
          }
          FallbackTargetSchema::Provider { provider } => {
            FallbackTarget::Provider(pid(provider))
          }
        })
        .collect(),
    })
    .collect();
  routes.insert_fallback_rules(rules);
  let gateway = Arc::new(gateway.with_routes(routes));
  // 默认二元组定位主模型（load 阶段已校验存在性）
  let primary = outcome
    .provider
    .providers
    .iter()
    .find(|p| {
      p.provider_id == outcome.provider.routing.default.provider
        && p
          .models
          .iter()
          .any(|m| m.id == outcome.provider.routing.default.model)
    })
    .expect("default (provider, model) 已在 load 阶段校验");
  let primary_model = primary
    .models
    .iter()
    .find(|m| m.id == outcome.provider.routing.default.model)
    .expect("default model 已在 load 阶段校验");
  let model = model_of(
    &primary.provider_id,
    &primary_model.id,
    primary.base_url.clone(),
    primary_model.context_window,
  );
  Ok((gateway, model))
}

/// mock 模式网关：按脚本步骤构造（生产入口空脚本——回合以
/// INVALID_REQUEST 终结属预期，mock 模式定位为「克隆即跑」的装配演示
/// 与 E2E 基座；E2E 经 [`assemble_with_steps`] 精确供给脚本）。
fn build_mock_gateway(
  steps: Vec<MockStep>,
) -> RiboResult<(Arc<Gateway>, Model)> {
  let provider_id = pid("mock");
  let model = model_of("mock", MOCK_MODEL, "http://localhost", 8192);
  let mock = ra_provider::adapters::mock::MockProvider::new(
    provider_id.clone(),
    vec![model.clone()],
    steps,
  );
  let mut gateway = Gateway::new(GatewayConfig::default());
  gateway.register(Arc::new(mock))?;
  Ok((Arc::new(gateway), model))
}

/// 把 schema 层 ACL 声明转换为运行时 [`PluginAcl`]（装配层唯一转换点）。
///
/// `http = true` 且白名单非空时构造 [`UrlPolicy`]；空白名单保持
/// `None`——`check_url` 对空策略拒绝一切 host，显式不构造可让
/// 「忘了配 hosts」在配置审查时一眼可见（最小权限语义不变）。
fn build_plugin_acl(acl: &PluginAclSchema) -> PluginAcl {
  PluginAcl {
    log: acl.log,
    storage: acl.storage,
    http: acl.http,
    url_policy: (acl.http && !acl.http_hosts.is_empty())
      .then(|| Arc::new(UrlPolicy::new(acl.http_hosts.clone()))),
    memory_limit_bytes: acl.memory_limit_bytes,
    storage_quota_bytes: acl.storage_quota_bytes,
    timeout_ceiling_ms: acl.timeout_ceiling_ms,
  }
}

/// 内部装配单元：已解析为完整路径的插件声明（roots 扫描项与
/// entries 清单项的统一形态）。
struct PluginSpec {
  /// wasm 文件完整路径（harness.root 已拼接）。
  full: PathBuf,
  config: Option<serde_json::Value>,
  acl: PluginAclSchema,
  /// 权威清单 + 侧载签名（来源校验；roots 扫描项为 None）。
  authority: Option<SignedAuthority>,
  /// 来源校验策略（同一进程内由 [plugins.verify] 统一构造）。
  verify_policy: VerificationPolicy,
  /// 来源域标记（装配期 plugin_id 冲突检测错误信息用）。
  source: String,
}

/// 路径是否逃逸沙箱基准（词法判定）：绝对路径或含 `..` 逃逸组件。
///
/// 与 `plugins.entries` / fs 工具族同一沙箱纪律——配置层给出的
/// 相对路径不得脱离 harness.root。注意本判定只看**词法**形态：符号链接
/// 与目录联接的逃逸由规范化后的归属校验兜底（见 [`collect_plugin_specs`]）。
/// 判定逻辑复用 [`is_lexically_safe`] 统一原语，避免与 fs 沙箱 / 技能路径漂移。
fn escapes_root(path: &Path) -> bool {
  !is_lexically_safe(path)
}

/// 校验 fs 只读挂载点。
///
/// 逐条校验全局 `settings.toml [fs.mounts]` 的声明：
/// 1. 名字匹配 `^[a-z][a-z0-9_]{0,31}$`（复用 `is_valid_domain_id`）且不在保留字清单；
/// 2. 路径必须绝对、存在、是目录；
/// 3. 路径不得落在当前工作目录 `harness.root` 内（尤其 `.ribo/` 子树）——
///    否则 `internal://trusted_keys.json` 会架空 `.ribo` 黑名单（单次启动校验）。
///
/// 返回 `(名字, 规范根)` 列表，供 [`build_named_roots`] 注入 fs（全部只读）。
///
/// # Errors
///
/// 非法名字 / 保留字 / 路径非绝对 / 不存在 / 在工作目录内 → `config.mount_invalid`。
const RESERVED_MOUNT_NAMES: &[&str] =
  &["ribo", "resources", "fs", "http", "https"];

pub(crate) fn validate_fs_mounts(
  outcome: &LoadOutcome,
) -> RiboResult<Vec<(String, PathBuf)>> {
  let work_dir = &outcome.harness.root;
  let mut valid = Vec::with_capacity(outcome.fs_mounts.mounts.len());
  for (name, root) in &outcome.fs_mounts.mounts {
    if !super::schema::is_valid_domain_id(name)
      || RESERVED_MOUNT_NAMES.contains(&name.as_str())
    {
      return Err(ra_core::error::RiboError::new(
        CONFIG_DOMAIN,
        "mount_invalid",
        format!("invalid fs mount name `{name}`"),
      ));
    }
    if !root.is_absolute() || !root.is_dir() {
      return Err(ra_core::error::RiboError::new(
        CONFIG_DOMAIN,
        "mount_invalid",
        format!(
          "fs mount `{name}` root must be an existing absolute directory"
        ),
      ));
    }
    let canonical_root = root.canonicalize().unwrap_or_else(|_| root.clone());
    let canonical_work =
      work_dir.canonicalize().unwrap_or_else(|_| work_dir.clone());
    if canonical_root.starts_with(&canonical_work) {
      return Err(ra_core::error::RiboError::new(
        CONFIG_DOMAIN,
        "mount_invalid",
        format!(
          "fs mount `{name}` root must not be inside work directory {}",
          work_dir.display()
        ),
      ));
    }
    valid.push((name.clone(), canonical_root));
  }
  Ok(valid)
}

/// 按声明序构建具名根表注入 fs（全部只读）。
fn build_named_roots(
  mounts: &[(String, PathBuf)],
  main_root: &Path,
) -> RiboResult<FsRoot> {
  let mut fs_root = FsRoot::new(main_root)?;
  for (name, root) in mounts {
    fs_root.add_named_root(name.clone(), FsRoot::named(name, root, true)?);
  }
  Ok(fs_root)
}

/// 校验插件 wasm 路径：canonicalize + harness.root 沙箱归属判定。
///
/// 词法判定（`escapes_root`）只挡得住配置中的绝对路径 / `..`；符号链接
/// 与目录联接须在规范化后复核归属，否则**扫描项与清单项**都能指向沙箱
/// 之外的文件。返回规范化路径供去重使用。
///
/// # Errors
/// - wasm 文件不存在或不可读 → `config.plugin_file_missing`；
/// - 规范化后路径不在 harness.root 之下 → `config.plugin_load_failed`。
fn validate_plugin_path(
  full: &Path,
  sandbox: Option<&Path>,
) -> RiboResult<PathBuf> {
  let canonical = std::fs::canonicalize(full).map_err(|e| {
    ra_core::error::RiboError::new(
      CONFIG_DOMAIN,
      "plugin_file_missing",
      format!("plugin wasm {} missing: {e}", full.display()),
    )
  })?;
  if let Some(root) = sandbox
    && !is_within(root, &canonical)
  {
    return Err(ra_core::error::RiboError::new(
      CONFIG_DOMAIN,
      "plugin_load_failed",
      format!("plugin path escapes harness.root: {}", canonical.display()),
    ));
  }
  Ok(canonical)
}

/// 构造来源校验策略：读取宿主侧载的 Ed25519 公钥白名单。
///
/// 路径纪律与 wasm / manifest 一致——相对 harness.root、词法无逃逸、
/// canonicalize 后仍归属沙箱。生产模式缺配置或文件不可读即启动致命，
/// 不以「空列表」静默降级（空列表会让所有签名校验失败，掩盖配置错误）。
fn build_verify_policy(
  outcome: &LoadOutcome,
) -> RiboResult<VerificationPolicy> {
  let verify = &outcome.plugins.verify;
  if !verify.require_signature {
    return Ok(VerificationPolicy::development());
  }
  // 公钥白名单固定为相对 harness.root 的 .ribo/trusted_keys.json
  let keys_path = Path::new(".ribo/trusted_keys.json");
  if escapes_root(keys_path) {
    return Err(ra_core::error::RiboError::new(
      CONFIG_DOMAIN,
      "plugin_load_failed",
      format!(
        "trusted keys path must be relative under harness.root: {}",
        keys_path.display()
      ),
    ));
  }
  let full = outcome.harness.root.join(keys_path);
  let canonical = std::fs::canonicalize(&full).map_err(|e| {
    // 语义区分：文件不存在（尚未配置信任清单）与存在但不可读（权限 /
    // IO 故障）分别报码，便于运维定位；两者均为启动致命（见函数文档）。
    let code = if e.kind() == std::io::ErrorKind::NotFound {
      "plugin_trusted_keys_missing"
    } else {
      "plugin_trusted_keys_unreadable"
    };
    ra_core::error::RiboError::new(
      CONFIG_DOMAIN,
      code,
      format!("trusted keys {} unreadable: {e}", full.display()),
    )
  })?;
  if let Some(root) = std::fs::canonicalize(&outcome.harness.root).ok()
    && !is_within(&root, &canonical)
  {
    return Err(ra_core::error::RiboError::new(
      CONFIG_DOMAIN,
      "plugin_load_failed",
      format!(
        "trusted keys path escapes harness.root: {}",
        canonical.display()
      ),
    ));
  }
  let bytes = std::fs::read(&full).map_err(|e| {
    ra_core::error::RiboError::new(
      CONFIG_DOMAIN,
      "plugin_trusted_keys_unreadable",
      format!("trusted keys {} unreadable: {e}", full.display()),
    )
  })?;
  let keys = TrustedKeys::from_json(&bytes).map_err(|e| {
    ra_core::error::RiboError::new(
      CONFIG_DOMAIN,
      "plugin_trusted_keys_invalid",
      format!("trusted keys {} invalid: {}", full.display(), e.message()),
    )
  })?;
  if keys.is_empty() {
    return Err(ra_core::error::RiboError::new(
      CONFIG_DOMAIN,
      "plugin_trusted_keys_invalid",
      format!("trusted keys {} contains no keys", full.display()),
    ));
  }
  Ok(VerificationPolicy::production(keys))
}

/// 从 plugins 层配置收集全部插件声明（固定目录扫描 + 显式清单）。
///
/// 目录扫描项取最小权限（ACL 全关、无注入 config）；需要授予能力 /
/// 注入 config 的插件应走 `[[plugins.entries]]` 清单。同一文件被
/// 固定扫描与 entries 同时引用时按规范化路径只保留一份（canonicalize
/// 兼做存在性检查，缺失即启动致命）。
///
/// 沙箱纪律（与 fs 工具族同源）：插件 wasm 恒位于 harness.root
/// 之下，两道防线——
///
/// 1. **词法**：扫描目录（固定 `.ribo/plugins`）与 entries 的路径必须是
///    root 下的相对子路径（绝对路径或 `..` 逃逸组件直接拒绝）。`Path::join`
///    遇绝对路径会替换基址，故 roots 的校验是扫描目录不越界的必要条件；
/// 2. **归属**：规范化（canonicalize）后的路径必须仍在规范化后的
///    harness.root 之下——堵住词法合法但经符号链接 / 目录联接指向沙箱
///    外的路径。扫描项在加入 `specs` 前即由 [`validate_plugin_path`]
///    校验，清单项在去重阶段由同一校验器复核。
///
/// # Errors
/// - 扫描目录不可读 → `config.plugin_dir_unreadable`；
/// - roots / 清单路径为绝对路径或含 `..` 逃逸组件 → `config.plugin_load_failed`；
/// - 规范化后路径逃逸 harness.root → `config.plugin_load_failed`；
/// - wasm 文件缺失 → `config.plugin_file_missing`。
fn collect_plugin_specs(outcome: &LoadOutcome) -> RiboResult<Vec<PluginSpec>> {
  let cfg = &outcome.plugins;
  // 规范化后的 harness.root 即沙箱边界；root 不存在时 canonicalize 失败
  // （None），此时 root 下的文件同样不存在——逐条 canonicalize 会以
  // `plugin_file_missing` 终结，故边界缺失按「无边界」处理即可。
  let sandbox = std::fs::canonicalize(&outcome.harness.root).ok();

  // 来源校验策略：信任清单是进程级唯一信任锚
  // 生产模式（require_signature = true）必须配置非空公钥清单；公钥文件与
  // wasm / manifest 同属 harness.root 沙箱，路径逃逸一律启动致命。
  let verify_policy = build_verify_policy(outcome)?;
  let mut specs: Vec<PluginSpec> = Vec::new();

  // 目录扫描：固定 .ribo/plugins 下全部 *.wasm（确定性排序）
  {
    let root = Path::new(".ribo/plugins");
    let dir = outcome.harness.root.join(root);
    let mut scanned: Vec<PathBuf> = Vec::new();
    // 固定路径不存在属正常（项目可只用 entries 清单）——跳过扫描
    if let Ok(entries) = std::fs::read_dir(&dir) {
      for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "wasm") {
          scanned.push(path);
        }
      }
    }
    scanned.sort();
    for path in scanned {
      validate_plugin_path(&path, sandbox.as_deref())?;
      specs.push(PluginSpec {
        full: path,
        config: None,
        acl: PluginAclSchema::default(),
        authority: None,
        verify_policy: verify_policy.clone(),
        source: format!("plugin root `{}`", root.display()),
      });
    }
  }

  // 显式清单：路径安全校验（与 fs 沙箱纪律同源）
  for entry in &cfg.entries {
    if escapes_root(&entry.path) {
      return Err(ra_core::error::RiboError::new(
        CONFIG_DOMAIN,
        "plugin_load_failed",
        format!(
          "plugin path must be relative under harness.root: {}",
          entry.path.display()
        ),
      ));
    }
    // 权威清单侧载：相对 root 解析，缺失 / 解析失败 =
    // 启动致命（信任链不完整不如明确失败）
    let manifest_bytes = match entry.manifest.as_ref() {
      Some(mpath) => {
        if escapes_root(mpath) {
          return Err(ra_core::error::RiboError::new(
            CONFIG_DOMAIN,
            "plugin_load_failed",
            format!(
              "authority manifest path must be relative under harness.root: {}",
              mpath.display()
            ),
          ));
        }
        let mfull = outcome.harness.root.join(mpath);
        let bytes = std::fs::read(&mfull).map_err(|e| {
          ra_core::error::RiboError::new(
            CONFIG_DOMAIN,
            "plugin_manifest_unreadable",
            format!("authority manifest {} unreadable: {e}", mfull.display()),
          )
        })?;
        Some(bytes)
      }
      None => None,
    };
    // 侧载签名块（来源校验）：相对 root、沙箱归属校验同 manifest。
    let provenance_bytes = match entry.provenance.as_ref() {
      Some(ppath) => {
        if escapes_root(ppath) {
          return Err(ra_core::error::RiboError::new(
            CONFIG_DOMAIN,
            "plugin_load_failed",
            format!(
              "provenance path must be relative under harness.root: {}",
              ppath.display()
            ),
          ));
        }
        let pfull = outcome.harness.root.join(ppath);
        let canonical = std::fs::canonicalize(&pfull).map_err(|e| {
          ra_core::error::RiboError::new(
            CONFIG_DOMAIN,
            "plugin_provenance_unreadable",
            format!("provenance {} unreadable: {e}", pfull.display()),
          )
        })?;
        if let Some(root) = std::fs::canonicalize(&outcome.harness.root).ok()
          && !is_within(&root, &canonical)
        {
          return Err(ra_core::error::RiboError::new(
            CONFIG_DOMAIN,
            "plugin_load_failed",
            format!(
              "provenance path escapes harness.root: {}",
              canonical.display()
            ),
          ));
        }
        Some(std::fs::read(&pfull).map_err(|e| {
          ra_core::error::RiboError::new(
            CONFIG_DOMAIN,
            "plugin_provenance_unreadable",
            format!("provenance {} unreadable: {e}", pfull.display()),
          )
        })?)
      }
      None => None,
    };
    let authority = match manifest_bytes {
      Some(bytes) => Some(
        SignedAuthority::from_json(&bytes, provenance_bytes.as_deref())
          .map_err(|e| {
            ra_core::error::RiboError::new(
              CONFIG_DOMAIN,
              "plugin_manifest_invalid",
              format!("authority manifest invalid: {}", e.message()),
            )
          })?,
      ),
      None => None,
    };
    specs.push(PluginSpec {
      full: outcome.harness.root.join(&entry.path),
      config: entry.config.clone(),
      acl: entry.acl.clone(),
      authority,
      verify_policy: verify_policy.clone(),
      source: format!("plugin entry `{}`", entry.path.display()),
    });
  }

  // 按规范化路径去重（归属校验统一由 validate_plugin_path 承担）
  // 扫描项已在 push 前校验；清单项在此统一复核（避免改变 manifest /
  // provenance 的错误优先级）。校验器同时产出规范化路径，供 roots /
  // entries 指向同一文件时去重。
  let mut seen: HashSet<PathBuf> = HashSet::new();
  let mut deduped: Vec<PluginSpec> = Vec::new();
  for spec in specs {
    let canonical = validate_plugin_path(&spec.full, sandbox.as_deref())?;
    if seen.insert(canonical) {
      deduped.push(spec);
    }
  }
  Ok(deduped)
}

/// 加载并注册全部插件（失败语义 = **启动致命**）。
///
/// `PluginRuntime::global()` 为 `OnceLock` 单例，首次初始化失败即
/// 毒化整个进程——半装配状态只会把问题推迟到首次工具调用，故任何
/// 插件加载 / 注册失败都直接上抛，由启动流程终结。
///
/// `allow_duplicate_plugin_id`（逃生舱口）：`false` 时在注册前检测
/// 跨源 `plugin_id` 冲突并**致命**（列出全部来源）；`true` 时降级为
/// Warning + 首声明胜出（与 registry 二道防线行为一致）。
///
/// # Errors
/// - wasm 文件读取失败（`config.plugin_load_failed`）；
/// - [`WasmPluginHost::load`] 失败（编译 / manifest / 命名，`plugin.plugin_*`）；
/// - [`PluginRegistry::register`] 失败（重复 ID / schema 非法 / 批量注入）；
/// - plugin_id 跨源冲突且未传逃生舱口 → `config.plugin_duplicate_id`。
fn load_and_register_plugins(
  outcome: &LoadOutcome,
  registry: &PluginRegistry,
  allow_duplicate_plugin_id: bool,
) -> RiboResult<Vec<String>> {
  let specs = collect_plugin_specs(outcome)?;
  let host = WasmPluginHost::new()?;
  let mut loaded = Vec::with_capacity(specs.len());
  let mut seen: std::collections::HashMap<String, String> =
    std::collections::HashMap::new();
  let mut duplicates: Vec<(String, Vec<String>)> = Vec::new();
  for spec in specs {
    let started = std::time::Instant::now();
    let wasm = std::fs::read(&spec.full).map_err(|e| {
      ra_core::error::RiboError::new(
        CONFIG_DOMAIN,
        "plugin_load_failed",
        format!("read plugin wasm {} failed: {e}", spec.full.display()),
      )
    })?;
    let acl = build_plugin_acl(&spec.acl);
    let plugin = host.load_verified(
      &wasm,
      spec.config.clone(),
      &acl,
      spec.authority.as_ref(),
      &spec.verify_policy,
    )?;
    let plugin_id = plugin.id().to_string();
    tracing::info!(
      plugin = %plugin_id,
      duration_ms = started.elapsed().as_millis() as u64,
      "plugin loaded"
    );

    // 装配期 plugin_id 冲突检测
    if let Some(first_source) = seen.get(&plugin_id) {
      tracing::warn!(
        plugin = %plugin_id,
        first = %first_source,
        later = %spec.source,
        "duplicate plugin_id across sources"
      );
      if allow_duplicate_plugin_id {
        continue; // 首声明胜出，跳过本条
      }
      duplicates.push((
        plugin_id.clone(),
        vec![first_source.clone(), spec.source.clone()],
      ));
    } else {
      seen.insert(plugin_id.clone(), spec.source.clone());
      registry.register(Arc::clone(&plugin))?;
      tracing::info!(plugin = %plugin_id, "plugin registered");
      loaded.push(plugin_id);
    }
  }
  if !duplicates.is_empty() && !allow_duplicate_plugin_id {
    let mut msg = String::from("duplicate plugin_id from multiple sources:\n");
    for (id, sources) in &duplicates {
      msg.push_str(&format!("  - \"{id}\"\n"));
      for src in sources {
        msg.push_str(&format!("      {src}\n"));
      }
    }
    msg.push_str(
      "Resolve by renaming one plugin, or pass --allow-duplicate-plugin-id \
       to load with the first declaration winning (not recommended).",
    );
    return Err(ra_core::error::RiboError::new(
      CONFIG_DOMAIN,
      "plugin_duplicate_id",
      msg,
    ));
  }
  Ok(loaded)
}

/// 回合在途句柄：TUI 与 CLI 共用 prompt 发起逻辑。
///
/// `done` 的类型为 `RiboResult<()>`（即 `Result<(), RiboError>`），
/// **不是** `Result<(), RiboResult<()>>`——避免嵌套 Result。
///
/// `done` 通道用于测试或主循环**可选**地观察回合结果（成功 / 失败原因）；
/// **Ctrl-C 退出流程直接 `await handle.task` 等待落盘，不消费 `done`**——
/// 任务本身在 `prompt` 返回后才结束，任务结束即等于「一轮对话落盘完成」。
pub struct TurnHandle {
  /// prompt 任务句柄（任务结束 = 落盘完成）。
  pub task: tokio::task::JoinHandle<()>,
  /// 回合结果通道（容量 1；任务在 `prompt` 返回后先投递再结束）。
  pub done: tokio::sync::mpsc::Receiver<RiboResult<()>>,
}

impl CliSession {
  /// 发起一轮对话并返回在途句柄（结果通道为复位权威，对齐 Repl.turn_done_rx）。
  ///
  /// **Send 边界**：`tokio::spawn` 要求闭包 Future 为 `Send`；`harness`
  /// 为 `Arc<Harness>`，`Harness::prompt` 的 await 链均为 `Send`，故闭包
  /// 满足 `Send`。若未来 `prompt` 引入非 `Send` 局部（如 `Rc`/`RefCell`
  /// 跨 await），须改用 `spawn_local` 或 `LocalSet`。
  ///
  /// # Examples
  ///
  /// ```no_run
  /// # use ra_cli::config::LoadOutcome;
  /// # async fn demo(outcome: LoadOutcome) -> Result<(), ra_core::error::RiboError> {
  /// let session = ra_cli::config::assemble::assemble(&outcome)?;
  /// let handle = session.spawn_turn("default", "hello");
  /// // 等待落盘完成：任务结束即等于一轮对话完成
  /// handle.task.await.expect("prompt 任务不 panic");
  /// # Ok(())
  /// # }
  /// ```
  pub fn spawn_turn(&self, key: &str, text: &str) -> TurnHandle {
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let harness = self.harness.clone(); // Arc<Harness>，Send
    let key = key.to_string();
    let text = text.to_string();
    // prompt 返回后先把结果塞进 done（容量 1，缓冲即返回，不阻塞任务
    // 结束），再让 task 结束——任务结束即「落盘完成」
    let task = tokio::spawn(async move {
      let result = harness.prompt(&key, &text).await.map(|_| ());
      let _ = tx.send(result).await;
    });
    TurnHandle { task, done: rx }
  }
}

/// 由 LoadOutcome 装配 REPL 会话。
///
/// mock 模式使用空脚本；E2E 需要按回合供给脚本时走
/// [`assemble_with_steps`]。
///
/// # Errors
///
/// 网关装配失败（provider 注册校验不通过等）时返回错误。
pub fn assemble(outcome: &LoadOutcome) -> RiboResult<CliSession> {
  assemble_with_steps(outcome, Vec::new())
}

/// 装配选项：控制装配期策略开关。
#[derive(Debug, Clone, Copy, Default)]
pub struct AssembleOptions {
  /// 允许跨源重复 plugin_id（首声明胜出，降级 Warning）。
  pub allow_duplicate_plugin_id: bool,
}

/// 带选项的装配入口：CLI/TUI 传入 `--allow-duplicate-plugin-id` 等开关。
///
/// # Errors
///
/// 同 [`assemble`]。
pub fn assemble_with_options(
  outcome: &LoadOutcome,
  steps: Vec<MockStep>,
  options: AssembleOptions,
) -> RiboResult<CliSession> {
  assemble_impl(outcome, steps, options)
}

/// E2E 专用装配入口：mock 网关注入脚本步骤（步骤按 `stream` 调用
/// 次序逐回合消费）。默认拒绝跨源重复 plugin_id。
///
/// # Errors
///
/// 同 [`assemble`]。
pub fn assemble_with_steps(
  outcome: &LoadOutcome,
  steps: Vec<MockStep>,
) -> RiboResult<CliSession> {
  assemble_impl(outcome, steps, AssembleOptions::default())
}

/// 装配实现体（三入口共用的唯一实现）。
///
/// # Errors
///
/// 网关 / 域校验 / 插件装配任一失败 → 启动致命。
fn assemble_impl(
  outcome: &LoadOutcome,
  steps: Vec<MockStep>,
  options: AssembleOptions,
) -> RiboResult<CliSession> {
  // routing 引用校验（Real 模式 fail-fast）
  // 仅 Real 模式校验——mock 模式无 provider 注册，规则本就悬空属预期。
  if outcome.mode == ConfigMode::Real {
    validate_routing_refs(outcome)?;
  }
  // compression 数值范围校验（摘要模型注册校验按模式在函数内判定）
  validate_compression(outcome)?;
  // 网关
  let (gateway, model) = match outcome.mode {
    ConfigMode::Real => build_real_gateway(outcome)?,
    ConfigMode::Mock => build_mock_gateway(steps)?,
  };
  // ctx 占比分母 / 侧栏 provider·model：model 后续移入 Compactor，先留存
  let context_window = model.context_window;
  let provider_id = model.provider.to_string();
  let model_name = model.name.clone();

  // 持久化 + 技能
  let store =
    Arc::new(JsonlSessionStore::new(sessions_dir(&outcome.harness.root)));
  // provider 延后创建（需绑定上下文统计估算器，冷启动回放后写三态）
  // 两域技能汇总（Project > Global）：SkillsLoader 按 roots 顺序
  // 首个胜出，故此处按优先级排列。无插件域技能并入。
  let mut skill_roots: Vec<SkillRoot> = Vec::new();
  // 项目技能根固定 <harness.root>/.ribo/skills/（有且仅有一个）
  skill_roots.push(SkillRoot::new(
    outcome.harness.root.join(".ribo/skills"),
    "project",
  ));
  let global_root = outcome
    .code_trust
    .as_ref()
    .and_then(|ctx| ctx.global_root())
    .map(Path::to_path_buf);
  if let Some(groot) = &global_root {
    skill_roots.push(SkillRoot::new(groot.join("skills"), "global"));
  }
  let (skills, diagnostics) = SkillsLoader::with_origins(skill_roots).load();
  for d in &diagnostics {
    tracing::warn!(code = ?d.code, path = ?d.location, "skill diagnostic");
  }
  // 诊断格式化留给输出层：/skills 逐条单行展示
  let skill_diagnostics = diagnostics
    .iter()
    .map(|d| {
      let location = d
        .location
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<root>".to_string());
      format!("{:?} {location}: {}", d.code, d.message)
    })
    .collect::<Vec<_>>();
  let skill_names: Vec<String> =
    skills.iter().map(|s| s.name.clone()).collect();
  let skill_origins: Vec<String> =
    skills.iter().map(|s| s.origin.clone()).collect();

  // 域内 AGENTS.md 拼接
  // 顺序 Global → Project → Plugin[* prompts/all]（后写更具体）；
  // 文档级资源默认加载，与代码信任无关。
  let mut instruction_roots: Vec<InstructionRoot> = Vec::new();
  if let Some(groot) = &global_root {
    instruction_roots.push(InstructionRoot::new(groot.clone(), "global"));
  }
  instruction_roots.push(InstructionRoot::new(
    outcome.harness.root.clone(),
    "project",
  ));
  let instructions = load_instructions(&instruction_roots);
  for src in &instructions.sources {
    if src.truncated {
      eprintln!(
        "WARN AGENTS.md from {} exceeds 32KB and was truncated",
        src.origin
      );
    }
  }

  // 上下文装配（compression 层 → ContextConfig.compaction 唯一真值）
  let ctx_cfg = ContextConfig {
    compaction: CompactionConfig {
      enabled: outcome.compression.enabled,
      trigger_ratio: outcome.compression.trigger_ratio,
      keep_recent_turns: outcome.compression.keep_recent_turns,
      summarizer_model: outcome
        .compression
        .summarizer_model
        .as_ref()
        .map(|m| mid(m)),
      max_summary_chars: outcome.compression.max_summary_chars,
      hysteresis_ratio: outcome.compression.hysteresis_ratio,
      cooldown_turns: outcome.compression.cooldown_turns,
      enable_incremental: outcome.compression.enable_incremental,
      incremental_rebuild_every: outcome.compression.incremental_rebuild_every,
      max_summary_input_tokens: outcome.compression.max_summary_input_tokens,
    },
    include_environment: outcome.harness.include_environment,
  };
  // 绑定 store，启用摘要记录写入与重启恢复（与 provider 共享同一 Arc）
  let compactor = Arc::new(
    Compactor::new(gateway.clone(), model.clone()).with_store(store.clone()),
  );
  let context = Arc::new(HarnessContext::new(
    SystemPromptBuilder::new()
      .with_template(DEFAULT_CLI_TEMPLATE)
      .with_instructions(instructions),
    Some(compactor),
    skills,
    ctx_cfg,
  ));
  // 冷启动统计估算器：provider 回放后按当前转录估算压缩三态（不压缩），
  // 使首次对话前 TUI 侧栏 / CLI `/status` 也有上下文数值。
  let provider = Arc::new(
    PersistentSessionProvider::new(store.clone())
      .with_stats_estimator(context.clone()),
  );

  // 内置工具 + WASM 插件
  // 沙箱根：以 harness.root 为主根，
  // 沙箱恒位于工作空间之内。
  // 工具面熔断配置统一走插件感知白名单：插件未
  // 启用时白名单多余但无害（内置工具不会产生 plugin.plugin_* 错误码）。
  let manager =
    Arc::new(ToolManager::with_config(plugin_aware_tool_manager_config()));
  if outcome.tools.builtin_enabled {
    // fs 只读挂载点（取代资源域）
    let mounts = validate_fs_mounts(outcome)?;
    let main_root = outcome.harness.root.clone();
    let fs_root = build_named_roots(&mounts, &main_root)?;
    if fs_root.named_root_count() > 0 {
      tracing::info!(
        mounts = fs_root.named_root_count(),
        "read-only mounts injected into fs"
      );
    }
    ra_tools::builtin::register_builtin_with_root(&manager, fs_root)?;
  }
  // 代码信任闸门
  // Project 插件须显式 code_trust 才加载；无 Global 域（code_trust=None）
  // 时退化为自动加载（回滚开关）。未信任不致命——只弃加载 + warning，
  // 技能与 AGENTS.md 仍照常加载（域故障隔离）。
  let mut warnings = outcome.warnings.clone();
  let project_code_trusted = match outcome.code_trust.as_ref() {
    Some(ctx) => {
      let trusted = ctx.is_project_code_trusted(&outcome.harness.root);
      if !trusted && outcome.plugins.enabled {
        let path = outcome.harness.root.display().to_string();
        warnings.push(format!(
          "project `{path}` has plugins but is not code-trusted. \
           Run `ribo trust add {path}` after reviewing the plugins. \
           Skills and AGENTS.md are still loaded."
        ));
        tracing::warn!("project plugins skipped: code trust not granted");
      }
      trusted
    }
    // 无 Global 域 → 兼容旧语义（自动加载）
    None => true,
  };

  let (plugin_registry, loaded_plugins) =
    if outcome.plugins.enabled && project_code_trusted {
      let registry = PluginRegistry::new(Arc::clone(&manager));
      let loaded = load_and_register_plugins(
        outcome,
        &registry,
        options.allow_duplicate_plugin_id,
      )?;
      tracing::info!(plugins = ?loaded, "plugins assembled");
      (Some(Arc::new(registry)), loaded)
    } else {
      if outcome.plugins.enabled && !project_code_trusted {
        tracing::info!("plugins skipped: project not code-trusted");
      } else {
        tracing::debug!("plugins disabled, skipping wasm plugin loading");
      }
      (None, Vec::new())
    };
  let tools = Arc::new(ToolManagerAdapter::new(Arc::clone(&manager)));

  // BaseAgent + Harness + CliSession
  let deps = LoopDeps {
    gateway: gateway.clone(),
    model: mid(&outcome.provider.routing.default.model),
    tools,
    context_provider: context.clone() as Arc<dyn ContextProvider>,
  };
  let agent = BaseAgent::new(deps, provider.clone());
  let harness = Arc::new(Harness::new(agent, provider, store));
  let events = harness.subscribe(); // 透传
  Ok(CliSession {
    harness,
    events,
    mode: outcome.mode,
    warnings,
    context_window,
    provider_id,
    model_name,
    history_lines: outcome.session.history_lines,
    default_key: outcome.session.default_key.clone(),
    skills: skill_names,
    skill_origins,
    skill_diagnostics,
    plugin_registry,
    loaded_plugins,
  })
}

/// 会话目录硬切换到 `.ribo/sessions/`，旧布局
/// `<root>/sessions/` 不读取、不迁移、不兼容。
#[test]
fn t3_sessions_live_under_ribo_dir() {
  let root = Path::new("/proj");
  assert_eq!(sessions_dir(root), Path::new("/proj/.ribo/sessions"));
  assert_ne!(sessions_dir(root), root.join("sessions"));
}

/// 场景：compression 配置校验。
///
/// 覆盖五类非法值 → `compression_invalid`（裸码断言）、老配置新字段
/// 取默认值、Real 模式摘要模型未注册拒绝、Mock 模式跳过注册校验。
#[cfg(test)]
mod compression_validation_tests {
  use super::*;
  use crate::config::LoadOutcome;

  /// 写临时 TOML 并加载 outcome（cli_root=None，单文件 Project 层）。
  fn load_outcome(toml_text: &str) -> LoadOutcome {
    let dir = tempfile::tempdir().expect("temp");
    let path = dir.path().join("ribo.toml");
    std::fs::write(&path, toml_text).expect("write config");
    // TempDir 在函数结束前保持存活：路径在 load 内即时消费，无跨生命周期引用
    LoadOutcome::load(Some(&path), None)
  }

  const BASE: &str = r#"
[routing]
default = { provider = "p", model = "m" }

[[providers]]
provider_id = "p"
adapter = "openai-chat"
base_url = "https://real"
api_key = "sk-1"

  [[providers.models]]
  id = "m"

[compression]
enabled = true
"#;

  #[test]
  fn trigger_ratio_out_of_range_rejected() {
    let outcome =
      load_outcome("[compression]\nenabled = true\ntrigger_ratio = 1.5\n");
    let err = validate_compression(&outcome).expect_err("must reject");
    assert_eq!(err.code(), "compression_invalid");
  }

  #[test]
  fn hysteresis_ratio_out_of_band_rejected() {
    let outcome =
      load_outcome("[compression]\nenabled = true\nhysteresis_ratio = 20.0\n");
    let err = validate_compression(&outcome).expect_err("must reject");
    assert_eq!(err.code(), "compression_invalid");
  }

  #[test]
  fn cooldown_turns_over_limit_rejected() {
    let outcome =
      load_outcome("[compression]\nenabled = true\ncooldown_turns = 200\n");
    let err = validate_compression(&outcome).expect_err("must reject");
    assert_eq!(err.code(), "compression_invalid");
  }

  #[test]
  fn rebuild_every_out_of_range_rejected() {
    let outcome = load_outcome(
      "[compression]\nenabled = true\nincremental_rebuild_every = 64\n",
    );
    let err = validate_compression(&outcome).expect_err("must reject");
    assert_eq!(err.code(), "compression_invalid");
  }

  #[test]
  fn legacy_config_gains_new_field_defaults() {
    // 仅旧 5 字段：serde default 补齐新字段，且校验通过
    let outcome = load_outcome(
      "[compression]\nenabled = true\ntrigger_ratio = 0.75\nmax_summary_chars = 2048\n",
    );
    validate_compression(&outcome).expect("legacy config must pass");
    assert!((outcome.compression.hysteresis_ratio - 1.10).abs() < f64::EPSILON);
    assert_eq!(outcome.compression.cooldown_turns, 2);
    assert!(outcome.compression.enable_incremental);
    assert_eq!(outcome.compression.incremental_rebuild_every, 8);
    assert_eq!(outcome.compression.max_summary_input_tokens, 0);
  }

  #[test]
  fn real_mode_unregistered_summarizer_model_rejected() {
    let text = format!("{BASE}summarizer_model = \"ghost-model\"\n");
    let outcome = load_outcome(&text);
    assert_eq!(outcome.mode, ConfigMode::Real);
    let err = validate_compression(&outcome).expect_err("must reject");
    assert_eq!(err.code(), "compression_invalid");
  }

  #[test]
  fn real_mode_registered_summarizer_model_passes() {
    let text = format!("{BASE}summarizer_model = \"m\"\n");
    let outcome = load_outcome(&text);
    assert_eq!(outcome.mode, ConfigMode::Real);
    validate_compression(&outcome).expect("registered model must pass");
  }

  #[test]
  fn mock_mode_skips_summarizer_registration_check() {
    // 无 provider → Mock 模式；摘要模型悬空属预期，不拒绝
    let outcome = load_outcome(
      "[compression]\nenabled = true\nsummarizer_model = \"ghost\"\n",
    );
    assert_eq!(outcome.mode, ConfigMode::Mock);
    validate_compression(&outcome).expect("mock mode skips registration check");
  }
}

/// 场景（ctx 占比分母）：装配把 `[[providers]].context_window` 传入
/// `CliSession`（状态行 ctx 用），mock 装配缺省 8192。
#[cfg(test)]
mod context_window_tests {
  use super::*;
  use crate::config::LoadOutcome;

  #[test]
  fn context_window_flows_into_session() {
    let dir = tempfile::tempdir().expect("temp");
    let path = dir.path().join("ribo.toml");
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
    let session = assemble_with_steps(&outcome, Vec::new()).expect("assemble");
    assert_eq!(session.context_window, 131072);
  }

  #[test]
  fn mock_session_uses_default_window() {
    let dir = tempfile::tempdir().expect("temp");
    let outcome = LoadOutcome::load(None, Some(dir.path()));
    let session = assemble_with_steps(&outcome, Vec::new()).expect("assemble");
    assert_eq!(session.context_window, 8192);
  }
}

#[cfg(test)]
mod mount_tests {
  use super::*;
  use crate::config::LoadOutcome;
  use std::path::PathBuf;

  fn outcome_with_mounts(
    mounts: Vec<(String, PathBuf)>,
  ) -> (LoadOutcome, tempfile::TempDir) {
    // 临时目录须存活到测试结束——作为 harness.root，挂载路径校验依赖其存在
    let tmp = tempfile::tempdir().expect("temp");
    let mut outcome = LoadOutcome::load(None, Some(tmp.path()));
    outcome.fs_mounts.mounts = mounts.into_iter().collect();
    (outcome, tmp)
  }

  #[test]
  fn t5_valid_mount_is_accepted() {
    let dir = tempfile::tempdir().expect("temp");
    let (outcome, _tmp) =
      outcome_with_mounts(vec![("datasets".into(), dir.path().to_path_buf())]);
    let mounts = validate_fs_mounts(&outcome).expect("valid mount");
    assert_eq!(mounts.len(), 1);
    assert_eq!(mounts[0].0, "datasets");
  }

  #[test]
  fn t6_mount_is_always_read_only() {
    let dir = tempfile::tempdir().expect("temp");
    let named = FsRoot::named("datasets", dir.path(), true).expect("named");
    assert!(named.resolve_for_write("x.txt", "write").is_err());
  }

  #[test]
  fn t7_mount_inside_work_dir_is_rejected() {
    let tmp = tempfile::tempdir().expect("temp");
    let inside = tmp.path().join("data");
    std::fs::create_dir(&inside).expect("mkdir");
    // harness.root 与 outcome_with_mounts 的不同——直接手工构造对齐
    let mut outcome = LoadOutcome::load(None, Some(tmp.path()));
    outcome.fs_mounts.mounts =
      [("data".to_string(), inside)].into_iter().collect();
    let err = validate_fs_mounts(&outcome).expect_err("must reject");
    assert_eq!(err.code(), "mount_invalid");
  }

  #[test]
  fn t8_reserved_mount_name_is_rejected() {
    let dir = tempfile::tempdir().expect("temp");
    let (outcome, _tmp) =
      outcome_with_mounts(vec![("ribo".into(), dir.path().to_path_buf())]);
    let err = validate_fs_mounts(&outcome).expect_err("reserved");
    assert_eq!(err.code(), "mount_invalid");
  }

  #[test]
  fn t9_invalid_mount_name_is_rejected() {
    let dir = tempfile::tempdir().expect("temp");
    let (outcome, _tmp) =
      outcome_with_mounts(vec![("my-mount".into(), dir.path().to_path_buf())]);
    let err = validate_fs_mounts(&outcome).expect_err("invalid name");
    assert_eq!(err.code(), "mount_invalid");
  }
}
