//! 插件声明层：语言无关的 [`Plugin`] trait 与 [`ToolManifest`]。
//!
//! `Plugin` 描述「一个插件提供哪些工具、如何执行」，不绑定任何运行时：
//! native Rust 插件与（Step B 的）WASM guest 共用同一声明边界，经
//! [`PluginToolAdapter`](crate::adapter::PluginToolAdapter) 桥接为
//! `Arc<dyn AgentTool>` 注入 `ToolManager`（主循环零改动）。
//!
//! `#[async_trait]` 实施纪律（设计文档 18_ §0.4）：
//! trait object（`Arc<dyn Plugin>`）要求 dyn-compatible，RPITIT /
//! `async fn in trait` 均不可用；**所有** `impl Plugin` 必须同步标记
//! `#[async_trait]`，漏标直接编译失败。

use std::time::Duration;

use async_trait::async_trait;
use ra_agent::events::AgentEvent;
use ra_core::error::RiboResult;
use ra_tools::context::ToolContext;

/// 插件抽象：插件生态的语言无关声明边界（设计文档 18_ §2.1）。
///
/// 实现方约束：
///
/// - `Send + Sync` 是强制要求——插件以 `Arc<dyn Plugin>` 跨线程共享，
///   内部可变状态必须用 `Mutex` / `RwLock` / 原子类型（禁止
///   `RefCell` / `Cell` 等 `!Sync` 类型）；
/// - `execute` 内部如有阻塞操作须自行 `spawn_blocking` 隔离，不得
///   阻塞 async runtime（Step B 的 WASM 宿主在 `run_tool` 内统一隔离）；
/// - `tools()` 在注册期被调用（Step B 中仅加载期一次并缓存），
///   实现应保持无副作用。
#[async_trait]
pub trait Plugin: Send + Sync {
  /// 插件唯一 ID：注册表去重键，同时作为工具名前缀
  /// （`{plugin_id}__{tool_name}`，命名约束见
  /// [`validate_plugin_id`](crate::adapter::validate_plugin_id)）。
  fn id(&self) -> &str;

  /// 声明本插件提供的工具清单（名称 / 描述 / JSON Schema / 超时 / 幂等）。
  fn tools(&self) -> Vec<ToolManifest>;

  /// 执行指定工具，返回 guest 的 JSON 结果。
  ///
  /// # 契约
  ///
  /// - 返回 [`RiboResult`]，错误由 [`crate::error`] 的构造函数产生，
  ///   按错误码语义分级（业务 / 系统 / 越权 / 内部）；
  /// - `ctx.signal()` 触发时应尽快返回（取消传播链路）；
  /// - `tool` 为工具**短名**（不含 `{plugin_id}__` 前缀，前缀由适配器
  ///   剥离后派发）。
  ///
  /// # Errors
  ///
  /// 工具执行失败（业务错误 / 取消 / 越权 / 内部故障）时返回
  /// `plugin.plugin_*` 错误码，由上层 `ToolManager` 记账与回灌。
  async fn execute(
    &self,
    tool: &str,
    args: serde_json::Value,
    ctx: &ToolContext,
  ) -> RiboResult<serde_json::Value>;

  /// 可选事件订阅（Step B 宿主**不调用**；Step C 经 Manifest 门禁且仅
  /// 转发脱敏元事件 / `type_tag`，避免对话内容泄露与广播路径阻塞）。
  fn on_event(&self, _event: &AgentEvent) {}
}

/// 插件工具声明：适配器据此生成 `AgentTool`（设计文档 18_ §2.1）。
///
/// `parameters` 必须是合法 JSON Schema——注册期经
/// `ra_tools::schema::compile_validator` 预检，非法时整个插件拒绝注册。
#[derive(Debug, Clone)]
pub struct ToolManifest {
  /// 工具短名（无需全局唯一；适配器自动加 `{plugin_id}__` 前缀，
  /// 命名约束见 [`validate_tool_name`](crate::adapter::validate_tool_name)）。
  pub name: String,
  /// 给 LLM 看的工具描述。
  pub description: String,
  /// 参数 JSON Schema（复用 `ra_tools::schema` 校验）。
  pub parameters: serde_json::Value,
  /// 可选：单工具超时覆盖（覆盖全局默认 2s；Step B 读取并换算为
  /// epoch 预算，Step A 阶段仅透传声明）。
  pub timeout_hint: Option<Duration>,
  /// 工具是否幂等：① 决定 `execution_mode`（非幂等 → `Sequential`，
  /// 防止有副作用的工具在并行批中交错执行）；② 供上层重试决策
  /// （仅幂等工具的业务错误可安全重试）。
  pub idempotent: bool,
  /// 可选：单工具线性内存覆写（Step B 两阶段 `min` 的工具级输入，
  /// **只能收紧不能放大**进程默认 32MB）。
  pub memory_limit_bytes: Option<usize>,
}
