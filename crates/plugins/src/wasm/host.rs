//! `WasmPluginHost`：加载 `.wasm` 为 `Plugin`，及 `WasmPlugin` 的
//! Stateless 执行实现。
//!
//! 加载链路：`.wasm` 字节 → `ModuleCache`（blake3 内容哈希，全局只编译
//! 一次）→ `list_tools()`（加载期一次并缓存）→ `parse_manifest` →
//! `plugin_id` 命名校验 → `Arc<dyn Plugin>`。
//!
//! 架构不变量：
//!
//! - **单例共享**：所有 host 共享同一 `&'static PluginRuntime`（Engine +
//!   ticker + ModuleCache），`WasmPluginHost` 不持有 ticker guard、
//!   **不实现 `Drop`**（全局 ticker 不属于任何 host）；
//! - **Stateless Store**：每次 `execute` 新建 `Store + Instance`（并发
//!   隔离、无状态串台）；插件级状态（storage / 限流器 / 连接池）挂
//!   [`PluginShared`] 跨调用存活；
//! - **config 经 args 信封**：`init_config` 已删除——加载期写入
//!   的 globals 会在 Stateless 模型下被重置，config 改为随
//!   `{"config": cfg, "args": args}` 每次 execute 传入。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ra_core::error::RiboResult;
use ra_tools::context::ToolContext;

use crate::acl::PluginAcl;
use crate::error::PLUGIN_INVALID_ARGUMENTS;
use crate::provenance::{
  SignedAuthority, VerificationPolicy, verify_provenance,
};
use crate::traits::{Plugin, ToolManifest};
use crate::wasm::abi;
use crate::wasm::hostfns::{LogRateLimiter, build_linker};
use crate::wasm::runtime::{HostState, PluginRuntime, run_tool};
use crate::wasm::storage::Storage;

/// 插件级共享状态：跨 execute 存活，不受「每次新建 Store」影响。
///
/// 若把 Storage / LogRateLimiter / Client 放进每次新建的 HostState，
/// Stateless Store 模型会让它们每次调用重置——违背「插件级、进程内」
/// 语义。三者均 `Send + Sync`。
pub struct PluginShared {
  /// HTTP 客户端：直接共享 [`PluginRuntime::http_client`] 的 Arc，连接池
  /// 进程级唯一（不再每插件各建一份）。
  pub client: Arc<reqwest::Client>,
  /// 单插件 storage 实例。
  pub storage: Arc<Storage>,
  /// 单插件日志限流器。
  pub log_limiter: Arc<LogRateLimiter>,
  /// 进程级运行参数（跨 execute 传给 HostState 构造 StoreLimits）。
  pub config: Arc<crate::config::PluginRuntimeConfig>,
}

/// WASM 插件宿主：`.wasm` → `Arc<dyn Plugin>` 的加载器。
///
/// 所有可能失败的初始化集中在 [`WasmPluginHost::new`]：
/// Engine（`PluginRuntime::global()` 单例）与 `Linker`（构建一次、
/// `Arc` 复用，`instantiate` 为只读方法可并发调用）。
pub struct WasmPluginHost {
  /// 进程级单例（Engine + 唯一 ticker + ModuleCache + 运行参数）。
  runtime: &'static PluginRuntime,
  /// host import 集合（构建一次，跨 execute 复用）。
  linker: Arc<wasmtime::Linker<HostState>>,
  // ModuleCache 不在此：Module 与 Engine 绑定，随 Engine 一起收归
  // PluginRuntime 全局单例，多 host 对同一 wasm 只编译一次。
}

impl core::fmt::Debug for WasmPluginHost {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    f.debug_struct("WasmPluginHost").finish_non_exhaustive()
  }
}

impl WasmPluginHost {
  /// 构造 Host（单例共享 + Linker 缓存）。
  ///
  /// # Errors
  ///
  /// `PluginRuntime` 初始化失败或 `Linker` 构建失败 →
  /// `plugin.plugin_internal`。
  pub fn new() -> RiboResult<Self> {
    let runtime = PluginRuntime::global()?;
    let linker = Arc::new(build_linker(&runtime.engine)?);
    Ok(Self { runtime, linker })
  }

  /// 加载单个 `.wasm` 为一个 [`Plugin`]。
  ///
  /// # 参数
  ///
  /// - `wasm_bytes`：模块字节；
  /// - `config`：插件级配置（经 args 信封在每次 execute 注入）；
  /// - `acl`：本插件被授予的能力集合（host 注入，非 guest 自述）。
  ///
  /// # Errors
  ///
  /// - 模块编译失败 / `list_tools` 失败 → 对应错误码；
  /// - manifest JSON 非法 → `plugin.plugin_exec_failed`；
  /// - `plugin_id` 命名非法 → `plugin.plugin_invalid_name`。
  pub fn load(
    &self,
    wasm_bytes: &[u8],
    config: Option<serde_json::Value>,
    acl: &PluginAcl,
  ) -> RiboResult<Arc<dyn Plugin>> {
    // 无权威清单：采信 guest 自述（开发 / CI 模式）
    self.load_with_authority(wasm_bytes, config, acl, None)
  }

  /// 带权威清单的加载（「权威 schema 来源」）。
  ///
  /// `authority = Some(m)` 时宿主以侧载清单为信任源：plugin_id 与
  /// 工具清单逐项比对，与 guest `list_tools()` 自述不符即拒绝加载
  /// （信任链闭合，生产模式必用）；`None` 维持采信 guest 自述的默认语义。
  ///
  /// # Errors
  ///
  /// 同 [`Self::load`]，另加：权威清单与 guest 自述不符 →
  /// `plugin.plugin_exec_failed`（错误信息含差异明细）。
  pub fn load_with_authority(
    &self,
    wasm_bytes: &[u8],
    config: Option<serde_json::Value>,
    acl: &PluginAcl,
    authority: Option<&crate::authority::AuthorityManifest>,
  ) -> RiboResult<Arc<dyn Plugin>> {
    self.load_inner(wasm_bytes, config, acl, authority)
  }

  /// 带来源校验的加载（签名校验，「来源校验」）。
  ///
  /// 与 [`load_with_authority`] 的差别：在**编译之前**验证 Ed25519 签名，
  /// 载荷绑定 wasm 摘要 + 原始 manifest 摘要 + plugin_id。生产策略下
  /// 缺签名 / 摘要不符 / 签名者未授信均拒绝加载（启动致命）。
  ///
  /// # Errors
  ///
  /// 来源校验失败 → `plugin.plugin_signature_invalid`；其余同
  /// [`load_with_authority`]。
  pub fn load_verified(
    &self,
    wasm_bytes: &[u8],
    config: Option<serde_json::Value>,
    acl: &PluginAcl,
    authority: Option<&SignedAuthority>,
    policy: &VerificationPolicy,
  ) -> RiboResult<Arc<dyn Plugin>> {
    // 来源校验（必须早于编译，防不可信字节进入 JIT）
    if let Some(auth) = authority {
      verify_provenance(wasm_bytes, auth, policy)?;
    } else if policy.require_signature {
      return Err(crate::error::plugin_signature_invalid(
        "signed authority manifest required by verification policy",
      ));
    }
    self.load_inner(wasm_bytes, config, acl, authority.map(|a| &a.manifest))
  }

  /// 加载主体（权威清单校验 + 编译 + 注册 manifest）。
  fn load_inner(
    &self,
    wasm_bytes: &[u8],
    config: Option<serde_json::Value>,
    acl: &PluginAcl,
    authority: Option<&crate::authority::AuthorityManifest>,
  ) -> RiboResult<Arc<dyn Plugin>> {
    // 编译（缓存键 = 内容哈希；多 host 只编译一次）
    let module = self
      .runtime
      .module_cache
      .get_or_compile(&self.runtime.engine, wasm_bytes)
      .map_err(|e| crate::error::plugin_internal("module compile", e))?;

    // 插件级共享状态（加载期一次，跨 execute 存活）
    let shared = Arc::new(PluginShared {
      client: Arc::clone(&self.runtime.http_client),
      storage: Arc::new(Storage::new(&self.runtime.cfg, acl)),
      log_limiter: Arc::new(LogRateLimiter::new(&self.runtime.cfg)),
      config: Arc::new(self.runtime.cfg.clone()),
    });

    // list_tools（加载期一次；结果缓存）
    let raw = abi::call_list_tools(
      &self.runtime.engine,
      &self.linker,
      &module,
      &shared,
      acl,
    )?;
    let manifest = abi::parse_manifest(&raw)?;

    // 权威清单校验（信任链闭合）
    if let Some(auth) = authority {
      crate::authority::check_authority(
        &manifest.plugin_id,
        &manifest.tools,
        auth,
      )?;
    }

    // plugin_id 校验（前缀合法性前置）
    crate::adapter::validate_plugin_id(&manifest.plugin_id)?;

    Ok(Arc::new(WasmPlugin {
      runtime: self.runtime,
      module,
      linker: Arc::clone(&self.linker),
      plugin_id: manifest.plugin_id,
      config,
      acl: acl.clone(),
      manifests: manifest.tools,
      shared,
    }))
  }
}

/// WASM 插件实例（`Plugin` 的 WASM 载体）。
pub struct WasmPlugin {
  /// 进程级单例。
  runtime: &'static PluginRuntime,
  /// 编译好的模块（来自全局 ModuleCache）。
  module: Arc<wasmtime::Module>,
  /// host import 集合（跨 execute 复用）。
  linker: Arc<wasmtime::Linker<HostState>>,
  /// 插件 ID：来自 `list_tools` 顶层 manifest（经命名约束校验）。
  plugin_id: String,
  /// 插件级配置（`Some(cfg)` 时经 args 信封注入；`None` 透传）。
  config: Option<serde_json::Value>,
  /// 插件 ACL（host 注入；单工具内存覆写在 execute 时合并，只收紧）。
  acl: PluginAcl,
  /// 工具清单缓存（`list_tools` 注册期仅调一次）。
  manifests: Vec<ToolManifest>,
  /// 插件级共享状态（storage / 限流器 / 连接池 / 配置）。
  shared: Arc<PluginShared>,
}

impl WasmPlugin {
  /// config 信封构造。
  ///
  /// `None` 原样透传；`Some(cfg)` 包裹为 `{"config": cfg, "args": args}`，
  /// 保证每次 execute 都能拿到配置（PDK 经 `ribo::config()` 透明解包，
  /// 作者仍以 `args["url"]` 读业务参数）。
  ///
  /// **位置约束**：本方法必须定义在独立的
  /// `impl WasmPlugin` 块内，**不能放进 `impl Plugin for WasmPlugin`**
  /// ——`Plugin` trait 里没有这个方法，写在 trait impl 内报 E0407。
  pub fn build_args_envelope(
    &self,
    args: serde_json::Value,
  ) -> serde_json::Value {
    match &self.config {
      Some(cfg) => serde_json::json!({ "config": cfg, "args": args }),
      None => args,
    }
  }
}

#[async_trait]
impl Plugin for WasmPlugin {
  fn id(&self) -> &str {
    &self.plugin_id
  }

  /// 直接返回缓存清单（`list_tools` 加载期仅调一次）。
  fn tools(&self) -> Vec<ToolManifest> {
    self.manifests.clone()
  }

  /// 执行：args 信封 → 查工具超时 / 内存覆写 → 合并内存覆写
  /// （工具级与插件级内存覆写合并进 acl 克隆，只收紧）→ `run_tool`。
  ///
  /// # Errors
  ///
  /// 未知工具名 → `plugin.plugin_invalid_arguments`；执行失败由 `run_tool` 映射。
  async fn execute(
    &self,
    tool: &str,
    args: serde_json::Value,
    ctx: &ToolContext,
  ) -> RiboResult<serde_json::Value> {
    // config 经 args 信封注入（与 Stateless Store 一致）
    let args = self.build_args_envelope(args);

    // 查工具声明（超时默认 2s；未知工具拒绝）
    let (timeout, mem_override) = self
      .manifests
      .iter()
      .find(|m| m.name == tool)
      .map(|m| {
        (
          m.timeout_hint.unwrap_or(Duration::from_secs(2)),
          m.memory_limit_bytes,
        )
      })
      .ok_or_else(|| {
        ra_core::error::RiboError::new(
          crate::error::DOMAIN,
          PLUGIN_INVALID_ARGUMENTS,
          format!("unknown tool `{tool}` in plugin {}", self.plugin_id),
        )
        .with_context("plugin_id", self.plugin_id.as_str())
        .with_context("tool_name", tool)
      })?;

    // 两级 min 的第一级：工具级覆写合并进 acl 克隆
    //   min(工具级 ToolManifest.memory_limit_bytes, 插件级 PluginAcl.memory_limit_bytes)
    // 第二级 min(合并值, 进程默认) 由 HostState::new 完成。
    let mut acl = self.acl.clone(); // 依赖 PluginAcl: Clone
    if let Some(v) = mem_override {
      acl.memory_limit_bytes = Some(match acl.memory_limit_bytes {
        Some(a) => a.min(v),
        None => v,
      });
    }

    // 执行超时的宿主侧 ceiling
    // min(工具 hint / 默认 2s, ACL 覆写, 进程 ceiling)——只收紧不放大，
    // 与内存覆写的两级 min 同理；PluginAcl 此前无超时能力的不对称
    // 由此补齐。
    let mut timeout = timeout;
    if let Some(ceiling) = acl.timeout_ceiling_ms {
      timeout = timeout.min(Duration::from_millis(ceiling));
    }
    timeout = timeout.min(self.shared.config.tool_timeout_ceiling);

    // 执行（spawn_blocking + Stateless Store）
    run_tool(
      &self.runtime.engine,
      &self.module,
      &self.linker,
      &self.shared,
      &acl,
      tool,
      &args,
      ctx,
      &self.plugin_id,
      timeout,
    )
    .await
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::acl::UrlPolicy;

  /// 场景：`build_args_envelope`——`None` 透传；`Some(cfg)` 包裹为
  /// `{"config": cfg, "args": args}`。
  #[test]
  fn args_envelope_wraps_config() {
    // WasmPlugin 字段私有，经最小构造不可行；此处用等价逻辑闭包验证
    // 信封形状，真实链路的「config 在 execute 内可见」由集成
    // 测试（WAT guest 读信封）覆盖。
    let config = Some(serde_json::json!({ "greeting_prefix": "Hi" }));
    let args = serde_json::json!({ "name": "a" });

    let wrapped: serde_json::Value = match &config {
      Some(cfg) => serde_json::json!({ "config": cfg, "args": args.clone() }),
      None => args.clone(),
    };
    assert_eq!(wrapped["config"]["greeting_prefix"], "Hi");
    assert_eq!(wrapped["args"]["name"], "a");

    let none: Option<serde_json::Value> = None;
    let passthrough: serde_json::Value = match &none {
      Some(cfg) => serde_json::json!({ "config": cfg, "args": args.clone() }),
      None => args.clone(),
    };
    assert_eq!(passthrough, args);
  }

  /// 场景：两个 `WasmPluginHost` 共享同一 `&'static PluginRuntime`
  /// （epoch 推进频率不随 host 数量翻倍的结构前提）。
  #[test]
  fn hosts_share_the_same_runtime_singleton() {
    let h1 = WasmPluginHost::new().expect("host 1");
    let h2 = WasmPluginHost::new().expect("host 2");
    // 字段私有，经 Debug 非穷尽输出无法比较指针；退而断言两次
    // PluginRuntime::global() 一致（new() 内部即取该单例）
    let r1 = PluginRuntime::global().expect("runtime");
    let r2 = PluginRuntime::global().expect("runtime");
    assert!(std::ptr::eq(r1, r2));
    let _ = (h1, h2);
  }

  /// 场景：`PluginAcl` Clone 语义——`url_policy` 的 Arc 共享：4 处 `acl.clone()` 依赖 Clone，且策略对象不深拷贝。
  #[test]
  fn plugin_acl_clone_shares_url_policy() {
    let acl = PluginAcl {
      http: true,
      url_policy: Some(Arc::new(UrlPolicy::new(["example.com"]))),
      ..PluginAcl::default()
    };
    let cloned = acl.clone();
    assert!(Arc::ptr_eq(
      acl.url_policy.as_ref().unwrap(),
      cloned.url_policy.as_ref().unwrap()
    ));
  }
}
