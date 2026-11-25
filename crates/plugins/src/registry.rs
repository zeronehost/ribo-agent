//! 插件注册表：进程内插件登记与原子批量注入。
//!
//! [`PluginRegistry`] 以 [`DashMap`] 维护 `plugin_id → Arc<dyn Plugin>`
//! （与 `ToolRegistry` 同款并发容器，无锁中毒、读多写少），并把插件的
//! 工具经 [`PluginToolAdapter`] 以**原子批量**方式注入
//! [`ToolManager`](ra_tools::manager::ToolManager)——任意工具校验失败时
//! 整个插件拒绝注册（不部分成功，「不信任插件」原则的落地）。
//!
//! 生命周期：注册表与进程一致，首版不支持运行时卸载 / 热重载
//! （[`PluginRegistry::unregister`] 仅为测试占位）。

use std::sync::Arc;

use dashmap::DashMap;
use ra_core::error::RiboResult;
use ra_tools::manager::{ToolManager, ToolManagerConfig};

use crate::adapter::{
  PluginToolAdapter, validate_plugin_id, validate_tool_name,
};
use crate::error::{
  PLUGIN_FORBIDDEN, PLUGIN_TIMEOUT, plugin_batch_failed, plugin_duplicate_id,
  plugin_schema_invalid,
};
use crate::traits::{Plugin, ToolManifest};

/// 装配期构造插件感知的 `ToolManagerConfig`。
///
/// 在 `ra-tools` 默认白名单（`tool.cancelled` / `tool.invalid_arguments`）
/// 基础上追加插件域两个**不计数**错误码：
///
/// - [`PLUGIN_TIMEOUT`]：系统级超时，不应拖累内置工具（连续 5 次插件
///   超时熔断整个工具面 30s 是语义错位）；
/// - [`PLUGIN_FORBIDDEN`]：配置 / 权限问题，用户修复 ACL 后不应再等
///   熔断恢复窗口。
///
/// `ra-tools` 不硬编码 `plugin.plugin_*`（避免反向依赖），装配点在
/// 引入插件的进程（本函数）完成追加；匹配为**精确字符串相等**，
/// 不支持前缀通配。
///
/// # Examples
///
/// ```
/// use ra_plugins::registry::plugin_aware_tool_manager_config;
/// use ra_tools::manager::ToolManager;
///
/// let manager = ToolManager::with_config(plugin_aware_tool_manager_config());
/// let _ = manager.list();
/// ```
pub fn plugin_aware_tool_manager_config() -> ToolManagerConfig {
  let mut cfg = ToolManagerConfig::default();
  // 装配期追加：精确匹配，无通配
  cfg.non_counting_codes.push(PLUGIN_TIMEOUT);
  cfg.non_counting_codes.push(PLUGIN_FORBIDDEN);
  cfg
}

/// 插件注册表：进程内 `plugin_id → Plugin` 的并发安全哈希表。
pub struct PluginRegistry {
  plugins: DashMap<String, Arc<dyn Plugin>>,
  tool_manager: Arc<ToolManager>,
}

impl core::fmt::Debug for PluginRegistry {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    // 只输出插件 ID 快照：Plugin 无 Debug 约束，ID 足以定位注册状态
    let mut ids: Vec<String> = self
      .plugins
      .iter()
      .map(|entry| entry.key().to_string())
      .collect();
    ids.sort_unstable();
    f.debug_struct("PluginRegistry")
      .field("plugins", &ids)
      .finish()
  }
}

impl PluginRegistry {
  /// 以目标 `ToolManager` 构造注册表（插件工具将注入该管理器）。
  pub fn new(tool_manager: Arc<ToolManager>) -> Self {
    Self {
      plugins: DashMap::new(),
      tool_manager,
    }
  }

  /// 注册插件。**真原子性**：全量校验 → 生成工具 → 批量注入，任一
  /// 失败不写入任何工具。
  ///
  /// 处理流程：
  ///
  /// 1. **全量校验（零写入）**：`plugin_id` 命名约束、注册表去重、
  ///    每个工具的短名约束与参数 Schema 编译预检
  ///    （复用 `ra_tools::schema::compile_validator`，不信任插件自述）；
  /// 2. **生成全部 `AgentTool`**（仍零写入）：经 [`PluginToolAdapter`]
  ///    加 `{plugin_id}__` 前缀；
  /// 3. **原子批量注入**：`ToolManager::register_batch` 全成或全不成。
  ///
  /// # Errors
  ///
  /// - 插件 ID 重复 → `plugin.plugin_duplicate_id`；
  /// - ID / 工具名命名非法 → `plugin.plugin_invalid_name`；
  /// - 参数 Schema 非法 → `plugin.plugin_schema_invalid`；
  /// - 批量注入失败 → `plugin.plugin_batch_failed`。
  pub fn register(&self, plugin: Arc<dyn Plugin>) -> RiboResult<()> {
    let pid = plugin.id().to_string();

    // 全量校验（零写入）
    validate_plugin_id(&pid)?;
    if self.plugins.contains_key(&pid) {
      tracing::warn!(plugin = %pid, "duplicate plugin registration refused");
      return Err(plugin_duplicate_id(&pid));
    }
    let manifests: Vec<ToolManifest> = plugin.tools();
    for m in &manifests {
      validate_tool_name(&m.name)?;
      // 注册期 Schema 预检：复用 ra-tools 的编译校验，与
      // ToolManager::register 的注册期拦截语义对齐
      let schema = ra_core::message::Schema(m.parameters.clone());
      ra_tools::schema::compile_validator(&schema)
        .map_err(|e| plugin_schema_invalid(&pid, &m.name, e.message()))?;
    }

    // 生成全部 AgentTool（仍零写入）
    let adapter = PluginToolAdapter::new(plugin.clone());
    let tools: Vec<Arc<dyn ra_tools::traits::AgentTool>> =
      manifests.iter().map(|m| adapter.make_tool(m)).collect();

    // 原子批量注入（全成或全不成）
    self
      .tool_manager
      .register_batch(tools)
      .map_err(|e| plugin_batch_failed(&pid, e.message()))?;

    // 登记注册表
    self.plugins.insert(pid.clone(), plugin);
    tracing::info!(plugin = %pid, tools = manifests.len(), "plugin registered");
    Ok(())
  }

  /// 注销插件元数据。**仅测试占位**：只从注册表移除 `Plugin` 本体，
  /// 已注入 `ToolManager` 的 `Arc<dyn AgentTool>` 不受影响、继续存活
  /// （生产不支持运行时卸载）。
  #[cfg(test)]
  pub fn unregister(&self, id: &str) -> RiboResult<()> {
    match self.plugins.remove(id) {
      Some(_) => {
        tracing::info!(plugin = %id, "plugin metadata unregistered (test only)");
        Ok(())
      }
      None => Ok(()),
    }
  }

  /// 取全部插件实例（顺序不定）。
  pub fn list(&self) -> Vec<Arc<dyn Plugin>> {
    self
      .plugins
      .iter()
      .map(|entry| Arc::clone(entry.value()))
      .collect()
  }

  /// 按 ID 取插件实例快照。
  pub fn get(&self, id: &str) -> Option<Arc<dyn Plugin>> {
    self.plugins.get(id).map(|entry| Arc::clone(entry.value()))
  }
}

#[cfg(test)]
mod tests {
  use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
  };

  use ra_tools::manager::ToolManager;
  use serde_json::json;
  use tokio_util::sync::CancellationToken;

  use super::*;
  use crate::error::{
    PLUGIN_DUPLICATE_ID, PLUGIN_INVALID_NAME, PLUGIN_SCHEMA_INVALID,
  };
  use crate::traits::ToolManifest;
  use ra_tools::context::ToolContext;

  /// 可配置 Mock 插件：工具名 / Schema / 幂等可注入，`tools()` 带计数。
  struct MockPlugin {
    id: &'static str,
    tool_name: &'static str,
    parameters: serde_json::Value,
    idempotent: bool,
    tools_calls: AtomicUsize,
  }

  impl MockPlugin {
    fn new(id: &'static str, tool_name: &'static str) -> Self {
      Self {
        id,
        tool_name,
        parameters: json!({ "type": "object", "properties": {} }),
        idempotent: true,
        tools_calls: AtomicUsize::new(0),
      }
    }
  }

  #[async_trait::async_trait]
  impl Plugin for MockPlugin {
    fn id(&self) -> &str {
      self.id
    }

    fn tools(&self) -> Vec<ToolManifest> {
      self.tools_calls.fetch_add(1, Ordering::SeqCst);
      vec![ToolManifest {
        name: self.tool_name.to_string(),
        description: "mock tool".to_string(),
        parameters: self.parameters.clone(),
        timeout_hint: None,
        idempotent: self.idempotent,
        memory_limit_bytes: None,
      }]
    }

    async fn execute(
      &self,
      _tool: &str,
      args: serde_json::Value,
      _ctx: &ToolContext,
    ) -> RiboResult<serde_json::Value> {
      Ok(args)
    }
  }

  /// 构造注册表 + 共享管理器。
  fn setup() -> (Arc<ToolManager>, PluginRegistry) {
    let manager = Arc::new(ToolManager::new());
    let registry = PluginRegistry::new(manager.clone());
    (manager, registry)
  }

  /// 场景：注册成功路径——工具以 `{plugin_id}__` 前缀进入 ToolManager，
  /// 且 `tools()` 在注册期仅被调用一次（缓存语义）。
  #[tokio::test]
  async fn register_success_prefixes_tools_and_calls_tools_once() {
    let (manager, registry) = setup();
    let counted = Arc::new(MockPlugin::new("a", "echo"));

    registry
      .register(counted.clone())
      .expect("register plugin a");
    assert!(registry.get("a").is_some());

    let names: Vec<String> = manager
      .get_tool_schemas()
      .into_iter()
      .map(|s| s.name)
      .collect();
    assert_eq!(names, vec!["a__echo"]);

    // tools() 仅注册期调用一次（后续 schema 直接读缓存）
    assert_eq!(counted.tools_calls.load(Ordering::SeqCst), 1);
  }

  /// 场景：重复 ID 拒绝且不覆盖既有插件。
  #[tokio::test]
  async fn duplicate_plugin_id_is_rejected() {
    let (_manager, registry) = setup();
    registry
      .register(Arc::new(MockPlugin::new("dup", "echo")))
      .expect("first registration");

    let err = registry
      .register(Arc::new(MockPlugin::new("dup", "echo")))
      .unwrap_err();
    assert_eq!(err.code(), PLUGIN_DUPLICATE_ID);
    assert!(registry.get("dup").is_some());
  }

  /// 场景：插件 ID 非法命名被拒。
  #[tokio::test]
  async fn invalid_plugin_id_is_rejected() {
    let (_manager, registry) = setup();
    let err = registry
      .register(Arc::new(MockPlugin::new("Bad-ID", "echo")))
      .unwrap_err();
    assert_eq!(err.code(), PLUGIN_INVALID_NAME);
  }

  /// 场景：工具名非法 / Schema 非法在注册期被拒，且零写入。
  #[tokio::test]
  async fn invalid_tool_name_or_schema_rejected_atomically() {
    let (manager, registry) = setup();

    // 工具名非法（大写）
    let err = registry
      .register(Arc::new(MockPlugin::new("a", "Echo")))
      .unwrap_err();
    assert_eq!(err.code(), PLUGIN_INVALID_NAME);
    assert!(manager.list().is_empty(), "zero tools written on failure");

    // Schema 非法（type 取值非法）
    let mut bad = MockPlugin::new("a", "echo");
    bad.parameters = json!({ "type": 123 });
    let err = registry.register(Arc::new(bad)).unwrap_err();
    assert_eq!(err.code(), PLUGIN_SCHEMA_INVALID);
    assert!(manager.list().is_empty(), "zero tools written on failure");
  }

  /// 场景：两个插件提供同名工具，经前缀共存且都能执行。
  #[tokio::test]
  async fn same_tool_name_across_plugins_coexists() {
    let (manager, registry) = setup();
    registry
      .register(Arc::new(MockPlugin::new("a", "echo")))
      .expect("register a");
    registry
      .register(Arc::new(MockPlugin::new("b", "echo")))
      .expect("register b");

    let mut names: Vec<String> = manager
      .get_tool_schemas()
      .into_iter()
      .map(|s| s.name)
      .collect();
    names.sort();
    assert_eq!(names, vec!["a__echo", "b__echo"]);

    // 两个带前缀工具都能执行（经 ToolManager 编排路径）
    for (name, plugin) in [("a__echo", "a"), ("b__echo", "b")] {
      let call = ra_core::message::ToolCall {
        id: format!("call-{plugin}"),
        name: name.to_string(),
        arguments: json!({ "from": plugin }),
        signature: None,
        namespace: None,
      };
      let result = manager
        .execute(
          &call,
          &ToolContext::new(CancellationToken::new(), Arc::new(|_| {})),
        )
        .await
        .expect("prefixed tool should execute");
      assert_eq!(result.details, Some(json!({ "from": plugin })));
    }
  }

  /// 场景：`list` 返回全部已注册插件。
  #[tokio::test]
  async fn list_returns_all_registered_plugins() {
    let (_manager, registry) = setup();
    registry
      .register(Arc::new(MockPlugin::new("a", "echo")))
      .unwrap();
    registry
      .register(Arc::new(MockPlugin::new("b", "echo")))
      .unwrap();

    let mut ids: Vec<String> = registry
      .list()
      .into_iter()
      .map(|p| p.id().to_string())
      .collect();
    ids.sort();
    assert_eq!(ids, vec!["a", "b"]);
  }
  /// 场景：装配期白名单——连续插件超时 /
  /// 越权**不打开**工具面熔断（默认配置下第 5 次连续失败即熔断，
  /// 全部请求将被 `tool.rate_limited` 拒绝）。
  #[tokio::test]
  async fn plugin_timeout_and_forbidden_do_not_open_circuit() {
    let cfg = plugin_aware_tool_manager_config();
    let manager = Arc::new(ToolManager::with_config(cfg));
    let registry = PluginRegistry::new(Arc::clone(&manager));

    // 每次执行都返回 plugin.plugin_timeout 的插件
    struct TimeoutPlugin;
    #[async_trait::async_trait]
    impl Plugin for TimeoutPlugin {
      fn id(&self) -> &str {
        "t"
      }
      fn tools(&self) -> Vec<ToolManifest> {
        vec![ToolManifest {
          name: "spin".to_string(),
          description: "always times out".to_string(),
          parameters: json!({ "type": "object", "properties": {} }),
          timeout_hint: None,
          idempotent: true,
          memory_limit_bytes: None,
        }]
      }
      async fn execute(
        &self,
        _tool: &str,
        _args: serde_json::Value,
        _ctx: &ToolContext,
      ) -> RiboResult<serde_json::Value> {
        Err(crate::error::plugin_timeout("t", "spin", 2000, "epoch"))
      }
    }
    registry
      .register(Arc::new(TimeoutPlugin))
      .expect("register timeout plugin");

    // 超过默认熔断阈值的连续失败：均直接返回 plugin.plugin_timeout，
    // 不得出现 tool.rate_limited（白名单不计入记账）
    for i in 0..7 {
      let call = ra_core::message::ToolCall {
        id: format!("call-{i}"),
        name: "t__spin".to_string(),
        arguments: json!({}),
        signature: None,
        namespace: None,
      };
      let err = manager
        .execute(&call, &ToolContext::detached())
        .await
        .expect_err("timeout plugin must fail");
      assert_eq!(err.code(), PLUGIN_TIMEOUT, "iteration {i}: {}", err.code());
    }
  }
}
