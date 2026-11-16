//! fuel 模式独立进程用例（v0.8.8）。
//!
//! `PluginRuntime` 是 `OnceLock` 单例，`Engine` 的 `consume_fuel` 在进程
//! 初始化时固化——同一测试进程无法先跑默认配置再切 fuel 配置。本文件
//! 作为**独立集成测试二进制**运行，在进程最早处设置启动期环境变量，
//! 从而验证「改配置 + 重启进程」这条 Step C 启用路径：
//!
//! 1. `enable_fuel` 通过 env 生效；
//! 2. `OnceLock` 语义不变（两次 `global()` 同一指针）；
//! 3. fuel 耗尽的死循环 guest 被中断，错误码为 `plugin.plugin_timeout`
//!    （结构化 `Trap::OutOfFuel` → Timeout，而不是 guest/宿主错误）。

use ra_plugins::acl::PluginAcl;
use ra_plugins::error::PLUGIN_TIMEOUT;
use ra_plugins::wasm::host::WasmPluginHost;
use ra_plugins::wasm::runtime::PluginRuntime;
use ra_tools::context::ToolContext;

#[tokio::test]
async fn fuel_enabled_process_interrupts_infinite_guest() {
  // SAFETY: 集成测试二进制内只有本文件一个用例，且在首次调用
  // PluginRuntime::global() 之前设置；无其它线程并发读取这些变量。
  unsafe {
    std::env::set_var("RIBO_PLUGINS_ENABLE_FUEL", "1");
    std::env::set_var("RIBO_PLUGINS_FUEL_LIMIT", "20000");
  }

  let runtime = PluginRuntime::global().expect("fuel runtime init");
  assert!(runtime.cfg.enable_fuel, "fuel must be enabled from env");
  assert_eq!(runtime.cfg.fuel_limit, 20_000);
  let again = PluginRuntime::global().expect("runtime init again");
  assert!(
    std::ptr::eq(runtime, again),
    "OnceLock must keep the same runtime instance"
  );

  // manifest 用 WAT 字符串承载，双引号需转义；长度按原始 JSON 计算
  let manifest = r#"{"plugin_id":"fuelguest","tools":[{"name":"t","description":"spin","parameters":{},"timeout_hint_ms":5000,"idempotent":true}]}"#;
  let manifest_wat = manifest.replace('"', "\\\"");
  let wat_src = format!(
    r#"(module
      (memory (export "memory") 1)
      (data (i32.const 16) "{manifest_wat}")
      (func (export "alloc") (param i32) (result i32) (i32.const 4096))
      (func (export "dealloc") (param i32 i32))
      (func (export "list_tools") (result i32 i32)
        (i32.const 16) (i32.const {len}))
      (func (export "execute") (param i32 i32 i32 i32) (result i32 i32)
        (local $i i32)
        (block $exit
          (loop $spin
            (br_if $exit (i32.ge_u (local.get $i) (i32.const 1000000000)))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $spin)))
        (i32.const 0) (i32.const 0)))"#,
    len = manifest.len(),
  );
  let wasm = wat::parse_str(&wat_src).expect("valid WAT");

  let host = WasmPluginHost::new().expect("host");
  let plugin = host
    .load(&wasm, None, &PluginAcl::default())
    .expect("load must succeed with fuel budget in list_tools store");
  let err = plugin
    .execute("t", serde_json::json!({}), &ToolContext::detached())
    .await
    .expect_err("fuel must abort the infinite guest");
  assert_eq!(err.code(), PLUGIN_TIMEOUT, "got: {err}");
  let reason = err
    .context()
    .and_then(|ctx| ctx.get("trap_reason"))
    .expect("trap_reason context must be set by plugin_timeout");
  assert!(
    reason.to_lowercase().contains("fuel"),
    "structured OutOfFuel reason must reach the caller: {reason}"
  );
}
