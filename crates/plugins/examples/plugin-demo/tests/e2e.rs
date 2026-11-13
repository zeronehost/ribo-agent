//! 端到端集成测试（18_ §13.2，方式 B「预编译 wasm」）：
//!
//! 1. 以 `cargo build --target wasm32-unknown-unknown` 构建 PDK guest；
//! 2. `WasmPluginHost::load` 加载 manifest（真实 PDK 宏导出 + ABI v0）；
//! 3. `PluginRegistry` 原子注册 → `ToolManager` 全链路执行；
//! 4. 验证 config 信封、storage 跨 execute 持久、epoch 超时与
//!    熔断白名单（§1.3 决策 1）。
//!
//! 运行方式（独立于 `cargo test --workspace` 主路径）：
//!
//! ```text
//! cargo test --manifest-path crates/plugins/examples/plugin-demo/Cargo.toml
//! ```
//!
//! 前置：`rustup target add wasm32-unknown-unknown`。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use ra_core::message::ToolCall;
use ra_plugins::acl::PluginAcl;
use ra_plugins::registry::{PluginRegistry, plugin_aware_tool_manager_config};
use ra_plugins::wasm::host::WasmPluginHost;
use ra_tools::context::ToolContext;
use ra_tools::manager::ToolManager;
use serde_json::{Value, json};

/// 构建 guest wasm 并读取字节（§13.4 方式 B）。
///
/// 构建失败直接 panic：集成测试的前提就是 guest 可构建，失败信息
/// 附 stderr 便于定位（编译错误 / 缺 wasm32 target）。
fn build_guest_wasm() -> Vec<u8> {
  let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
  let cargo = env!("CARGO");

  let output = Command::new(cargo)
    .arg("build")
    .arg("--manifest-path")
    .arg(&manifest)
    .arg("--target")
    .arg("wasm32-unknown-unknown")
    .output()
    .expect("spawn cargo build for guest");

  assert!(
    output.status.success(),
    "guest build failed:\n{}",
    String::from_utf8_lossy(&output.stderr)
  );

  // 本 crate 为独立 workspace 根，target 目录在 crate 内
  let wasm: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
    .join("target")
    .join("wasm32-unknown-unknown")
    .join("debug")
    .join("plugin_demo.wasm");
  std::fs::read(&wasm)
    .unwrap_or_else(|e| panic!("read guest artifact {}: {e}", wasm.display()))
}

/// 构造一次 `ToolCall`。
fn call(name: &str, arguments: Value) -> ToolCall {
  ToolCall {
    id: format!("call-{name}"),
    name: name.to_string(),
    arguments,
    signature: None,
    namespace: None,
  }
}

/// 场景 1（§14 DoD 1 / 2 的最小闭环）：加载 → 注册 → 全链路执行
/// echo，插件工具以 `demo__echo` 之名对主循环可见；greet 验证
/// config 信封注入与 storage 跨 execute 持久（§13.1 三态回归）。
#[tokio::test]
async fn demo_plugin_loads_registers_and_executes() {
  let wasm = build_guest_wasm();
  let host = WasmPluginHost::new().expect("wasm host init");

  // ACL：log + storage 开启（最小权限，http 关闭）
  let acl = PluginAcl {
    log: true,
    storage: true,
    ..PluginAcl::default()
  };
  let plugin = host
    .load(&wasm, Some(json!({ "greeting_prefix": "你好" })), &acl)
    .expect("load demo plugin");
  assert_eq!(plugin.id(), "demo");

  // manifest 来自真实 PDK 宏导出（list_tools ABI v0）
  let mut tool_names: Vec<String> =
    plugin.tools().into_iter().map(|m| m.name).collect();
  tool_names.sort();
  assert_eq!(tool_names, vec!["echo", "fetch", "greet", "spin"]);

  // 注册（原子批量注入）+ 前缀可见性
  let manager =
    Arc::new(ToolManager::with_config(plugin_aware_tool_manager_config()));
  let registry = PluginRegistry::new(Arc::clone(&manager));
  registry.register(plugin).expect("register demo plugin");

  let mut schemas: Vec<String> = manager
    .get_tool_schemas()
    .into_iter()
    .map(|s| s.name)
    .collect();
  schemas.sort();
  assert_eq!(
    schemas,
    vec!["demo__echo", "demo__fetch", "demo__greet", "demo__spin"]
  );

  // echo 全链路：ToolManager → PluginAgentTool → run_tool → guest
  let result = manager
    .execute(
      &call("demo__echo", json!({ "x": 1 })),
      &ToolContext::detached(),
    )
    .await
    .expect("echo executes");
  assert_eq!(result.details, Some(json!({ "x": 1 })));

  // greet ①：首次调用（key 不存在 → seen=0，config 前缀生效）
  let r1 = manager
    .execute(
      &call("demo__greet", json!({ "name": "a" })),
      &ToolContext::detached(),
    )
    .await
    .expect("greet executes (first)");
  assert_eq!(r1.details, Some(json!({ "message": "你好, a! seen=0" })));

  // greet ②：storage 跨 execute 持久（Stateless Store 下经
  // PluginShared 存活，§13.1「greet 首次 seen=0」三态回归）
  let r2 = manager
    .execute(
      &call("demo__greet", json!({ "name": "a" })),
      &ToolContext::detached(),
    )
    .await
    .expect("greet executes (second)");
  assert_eq!(r2.details, Some(json!({ "message": "你好, a! seen=1" })));
}

/// 场景 2（§14 DoD 2）：死循环 guest 在 epoch 预算内被打断
/// （`plugin.plugin_timeout`）；装配白名单下连续超时**不**打开
/// 工具面熔断（第 7 次仍为超时而非 `tool.rate_limited`，§1.6 验收）。
#[tokio::test]
async fn spin_times_out_without_opening_circuit() {
  let wasm = build_guest_wasm();
  let host = WasmPluginHost::new().expect("wasm host init");

  let acl = PluginAcl::default(); // spin 不需要任何 IO 能力
  let plugin = host.load(&wasm, None, &acl).expect("load demo plugin");

  let manager =
    Arc::new(ToolManager::with_config(plugin_aware_tool_manager_config()));
  let registry = PluginRegistry::new(Arc::clone(&manager));
  registry.register(plugin).expect("register demo plugin");

  for i in 0..7 {
    let err = manager
      .execute(&call("demo__spin", json!({})), &ToolContext::detached())
      .await
      .expect_err("spin must time out");
    assert_eq!(
      err.code(),
      "plugin.plugin_timeout",
      "iteration {i}: {}",
      err.code()
    );
  }
}

/// 场景 3（§14 DoD 3「拒绝越权」）：ACL 未授予 http 能力时，guest 的
/// `ribo::http_get` 在 host function 入口被拦（trap `forbidden:http` →
/// `plugin.plugin_forbidden`，不计熔断白名单）。CI 安全：无任何网络
/// 请求发出即被拒绝。
#[tokio::test]
async fn http_get_denied_without_http_acl() {
  let wasm = build_guest_wasm();
  let host = WasmPluginHost::new().expect("wasm host init");

  // ACL：log 授予（echo 需要）、http 未授予——guest 的任何 http
  // 尝试都会在 host function 入口被拦
  let acl = PluginAcl {
    log: true,
    ..PluginAcl::default()
  };
  let plugin = host.load(&wasm, None, &acl).expect("load demo plugin");

  let manager =
    Arc::new(ToolManager::with_config(plugin_aware_tool_manager_config()));
  let registry = PluginRegistry::new(Arc::clone(&manager));
  registry.register(plugin).expect("register demo plugin");

  let err = manager
    .execute(
      &call("demo__fetch", json!({ "url": "https://example.com/x" })),
      &ToolContext::detached(),
    )
    .await
    .expect_err("http must be denied without ACL");
  assert_eq!(err.code(), "plugin.plugin_forbidden", "got: {}", err.code());
  // 越权错误不计熔断（§1.3 白名单）：连续越权后工具面仍可用
  let echo = manager
    .execute(
      &call("demo__echo", json!({ "ok": 1 })),
      &ToolContext::detached(),
    )
    .await;
  assert!(echo.is_ok(), "circuit must not open on forbidden errors");
}
