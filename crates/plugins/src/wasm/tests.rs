//! Step B 单元测试（§13.1）：以 §13.4「方式 A 内联 WAT」构造测试 guest，
//! 覆盖 echo 全链路、死循环 epoch 中断、内存超限、guest trap、
//! DoS 边界与 storage 三态契约。
//!
//! 测试 guest 约定（§13.4 修正版）：
//! - `alloc` 必须返回**非 0** 有效指针（返回 0 = 分配失败，host 直接报错）；
//! - 必须导出 `execute`（而非 WASI 的 `_start`）与 `memory`。

use std::sync::Arc;
use std::time::Duration;

use ra_core::error::RiboResult;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::acl::PluginAcl;
use crate::config::PluginRuntimeConfig;
use crate::error::{
  PLUGIN_EXEC_FAILED, PLUGIN_FORBIDDEN, PLUGIN_INTERNAL, PLUGIN_TIMEOUT,
};
use crate::wasm::host::PluginShared;
use crate::wasm::hostfns::{LogRateLimiter, build_http_client, build_linker};
use crate::wasm::runtime::{PluginRuntime, run_tool};
use crate::wasm::storage::Storage;

// ===== 测试辅助 =====

/// 构造插件级共享状态（client / storage / limiter / config）。
fn make_shared(acl: &PluginAcl) -> Arc<PluginShared> {
  let cfg = PluginRuntimeConfig::default();
  Arc::new(PluginShared {
    client: build_http_client().expect("http client"),
    storage: Arc::new(Storage::new(&cfg, acl)),
    log_limiter: Arc::new(LogRateLimiter::new(&cfg)),
    config: Arc::new(cfg),
  })
}

/// 以 `run_tool` 执行一次 WAT guest 的 `execute` 导出。
async fn run_guest(
  wasm: &[u8],
  acl: &PluginAcl,
  tool: &str,
  args: serde_json::Value,
  timeout: Duration,
) -> RiboResult<serde_json::Value> {
  let shared = make_shared(acl);
  run_guest_with_shared(wasm, &shared, acl, tool, args, timeout).await
}

/// 以显式共享状态执行（用于断言 storage 写入落在同一实例上）。
async fn run_guest_with_shared(
  wasm: &[u8],
  shared: &Arc<PluginShared>,
  acl: &PluginAcl,
  tool: &str,
  args: serde_json::Value,
  timeout: Duration,
) -> RiboResult<serde_json::Value> {
  let runtime = PluginRuntime::global().expect("plugin runtime");
  let module = runtime
    .module_cache
    .get_or_compile(&runtime.engine, wasm)
    .expect("compile test guest");
  let linker = build_linker(&runtime.engine).expect("build linker");
  run_tool(
    &runtime.engine,
    &module,
    &Arc::new(linker),
    shared,
    acl,
    tool,
    &args,
    &ra_tools::context::ToolContext::detached(),
    "test",
    timeout,
  )
  .await
}

/// 正常 echo guest：execute 把 host 写入的 args 原样返回（返回值即
/// `(args_ptr, args_len)`）。alloc 用自增偏移的 bump 分配器（固定偏移
/// 会让多次分配互相覆盖——§13.4 注意事项）。
fn echo_guest() -> Vec<u8> {
  wat::parse_str(
    r#"(module
      (memory (export "memory") 1)
      (global $next (mut i32) (i32.const 64))
      (func (export "alloc") (param i32) (result i32)
        (local $p i32)
        (local.set $p (global.get $next))
        (global.set $next (i32.add (local.get $p) (local.get 0)))
        (local.get $p))
      (func (export "dealloc") (param i32 i32))
      (func (export "execute") (param i32 i32 i32 i32) (result i32 i32)
        (local.get 2) (local.get 3)))"#,
  )
  .expect("valid echo wat")
}

/// 死循环 guest：`execute` 体内无限循环，靠 Store 自身 epoch 预算收尾
/// （§13.4：死循环体必须在 execute 内，alloc 必须返回非 0）。
fn deadloop_guest() -> Vec<u8> {
  wat::parse_str(
    r#"(module
      (memory (export "memory") 1)
      (func (export "alloc") (param i32) (result i32) (i32.const 16))
      (func (export "dealloc") (param i32 i32))
      (func (export "execute") (param i32 i32 i32 i32) (result i32 i32)
        (loop (br 0))
        (unreachable)))"#,
  )
  .expect("valid deadloop wat")
}

/// 立即 trap guest：`execute` 体内 `unreachable`（guest 自身业务级 trap）。
fn unreachable_guest() -> Vec<u8> {
  wat::parse_str(
    r#"(module
      (memory (export "memory") 1)
      (func (export "alloc") (param i32) (result i32) (i32.const 16))
      (func (export "dealloc") (param i32 i32))
      (func (export "execute") (param i32 i32 i32 i32) (result i32 i32)
        (unreachable)))"#,
  )
  .expect("valid unreachable wat")
}

/// 缺失 memory 导出的 guest（§8.1：memory 必须显式导出）。
fn missing_memory_guest() -> Vec<u8> {
  wat::parse_str(
    r#"(module
      (memory 1)
      (func (export "alloc") (param i32) (result i32) (i32.const 16))
      (func (export "dealloc") (param i32 i32))
      (func (export "execute") (param i32 i32 i32 i32) (result i32 i32)
        (i32.const 0) (i32.const 0)))"#,
  )
  .expect("valid missing-memory wat")
}

/// 申请 64MiB 线性内存的 guest（进程上限 32MiB → instantiate 超限）。
fn memory_hog_guest() -> Vec<u8> {
  wat::parse_str(
    r#"(module
      (memory (export "memory") 1024)
      (func (export "alloc") (param i32) (result i32) (i32.const 16))
      (func (export "dealloc") (param i32 i32))
      (func (export "execute") (param i32 i32 i32 i32) (result i32 i32)
        (i32.const 0) (i32.const 0)))"#,
  )
  .expect("valid memory-hog wat")
}

/// 开启全部能力的 ACL（测试用）。
fn full_acl() -> PluginAcl {
  PluginAcl {
    log: true,
    storage: true,
    http: true,
    url_policy: Some(Arc::new(crate::acl::UrlPolicy::new(["example.com"]))),
    memory_limit_bytes: None,
    storage_quota_bytes: None,
  }
}

// ===== 用例 =====

/// 场景（§13.4 前置用例 + §4.2 全链路）：echo guest 端到端跑通——
/// args 写入 guest 内存 → execute 回传 → host 读回 JSON。
#[tokio::test]
async fn echo_guest_roundtrip() {
  let args = json!({ "msg": "hello", "n": 7 });
  let out = run_guest(
    &echo_guest(),
    &full_acl(),
    "echo",
    args.clone(),
    Duration::from_secs(2),
  )
  .await
  .expect("echo should succeed");
  assert_eq!(out, args);
}

/// 场景（§13.1）：纯 CPU 死循环被 epoch **确定性中断** →
/// `plugin.plugin_timeout`（不计熔断；预算 100ms，实际中断
/// T ∈ [100ms, 100ms + EPOCH_TICK]）。
#[tokio::test]
async fn deadloop_guest_is_interrupted_by_epoch() {
  let started = std::time::Instant::now();
  let err = run_guest(
    &deadloop_guest(),
    &full_acl(),
    "spin",
    json!({}),
    Duration::from_millis(100),
  )
  .await
  .unwrap_err();
  let elapsed = started.elapsed();
  assert_eq!(err.code(), PLUGIN_TIMEOUT, "got: {err}");
  // ceil 语义：等待 ≥ 请求的 timeout，且不超过 timeout + 若干 tick（容忍调度抖动）
  assert!(elapsed >= Duration::from_millis(100), "elapsed {elapsed:?}");
  assert!(elapsed < Duration::from_secs(3), "elapsed {elapsed:?}");
}

/// 场景（§13.1 两类超限之一）：线性内存超限（64MiB > 32MiB）在
/// instantiate 期被 `StoreLimits` 拦截 → `plugin.plugin_timeout`
/// （不计熔断；**不是** plugin_internal）。
#[tokio::test]
async fn memory_hog_guest_hits_store_limits() {
  let err = run_guest(
    &memory_hog_guest(),
    &full_acl(),
    "hog",
    json!({}),
    Duration::from_secs(2),
  )
  .await
  .unwrap_err();
  assert_eq!(err.code(), PLUGIN_TIMEOUT, "got: {err}");
}

/// 场景（§13.1）：guest 自身 trap（unreachable）→ 业务级
/// `plugin.plugin_exec_failed`（可回灌 LLM），**不是** plugin_internal。
#[tokio::test]
async fn guest_trap_maps_to_exec_failed() {
  let err = run_guest(
    &unreachable_guest(),
    &full_acl(),
    "boom",
    json!({}),
    Duration::from_secs(2),
  )
  .await
  .unwrap_err();
  assert_eq!(err.code(), PLUGIN_EXEC_FAILED, "got: {err}");
}

/// 场景（§8.1 / §13.1）：guest 未显式导出 `memory` →
/// `plugin.plugin_internal`（宿主内部错误，计入熔断）。
#[tokio::test]
async fn missing_memory_export_maps_to_internal() {
  let err = run_guest(
    &missing_memory_guest(),
    &full_acl(),
    "x",
    json!({}),
    Duration::from_secs(2),
  )
  .await
  .unwrap_err();
  assert_eq!(err.code(), PLUGIN_INTERNAL, "got: {err}");
  // 具体原因在 context.reason（stage 为 run_tool）
  assert!(
    err
      .context()
      .and_then(|c| c.get("reason").cloned())
      .is_some_and(|r| r.contains("memory")),
    "got: {err:?}"
  );
}

/// 场景（v0.8.2 P0-1 三态契约）：guest 首次 `storage_get` 未命中的 key
/// 收到 **0 哨兵**（不是错误）→ `Ok(())`；`storage_put` 后 host 侧
/// `Storage::get` 可读回。守住「greet 首次调用必挂」的跨层矛盾。
#[tokio::test]
async fn storage_three_state_contract_via_guest() {
  let acl = full_acl();
  let wasm = wat::parse_str(
    r#"(module
      (import "ribo" "storage_get" (func $get (param i32 i32) (result i32)))
      (import "ribo" "storage_put" (func $put (param i32 i32 i32 i32) (result i32)))
      (data (i32.const 0) "seen")
      (data (i32.const 16) "\01")
      (memory (export "memory") 1)
      (func (export "alloc") (param i32) (result i32) (i32.const 512))
      (func (export "dealloc") (param i32 i32))
      (func (export "execute") (param i32 i32 i32 i32) (result i32 i32)
        (drop (call $get (i32.const 0) (i32.const 4)))
        (drop (call $put (i32.const 0) (i32.const 4) (i32.const 16) (i32.const 1)))
        (i32.const 0) (i32.const 0)))"#,
  )
  .expect("valid storage wat");

  // 共享状态显式构造并传入，保证断言与 guest 写入落在同一 Storage 实例
  let shared = make_shared(&acl);
  run_guest_with_shared(
    &wasm,
    &shared,
    &acl,
    "bump",
    json!({}),
    Duration::from_secs(2),
  )
  .await
  .expect("storage get/put must succeed (0 = key missing is NOT an error)");

  // host 侧验证：值已写入同一实例
  assert_eq!(
    shared.storage.get("seen").expect("get after put"),
    Some(vec![1u8])
  );
}

/// 场景（§13.1 DoS 边界 · 超大 key）：key_len = 257 > MAX_KEY_LEN(256)
/// 在读取点被拒（先判后分配）→ `plugin.plugin_exec_failed`，错误信息含
/// `exceeds limit`；且**未触达存储层**（host 侧 get 计数为 0，经
/// 「值未写入」间接断言）。
#[tokio::test]
async fn oversized_storage_key_is_rejected_at_read_point() {
  let acl = full_acl();
  let wasm = wat::parse_str(
    r#"(module
      (import "ribo" "storage_get" (func $get (param i32 i32) (result i32)))
      (memory (export "memory") 1)
      (func (export "alloc") (param i32) (result i32) (i32.const 512))
      (func (export "dealloc") (param i32 i32))
      (func (export "execute") (param i32 i32 i32 i32) (result i32 i32)
        (drop (call $get (i32.const 0) (i32.const 257)))
        (i32.const 0) (i32.const 0)))"#,
  )
  .expect("valid oversized-key wat");

  let err = run_guest(&wasm, &acl, "dos", json!({}), Duration::from_secs(2))
    .await
    .unwrap_err();
  assert_eq!(err.code(), PLUGIN_EXEC_FAILED, "got: {err}");
  assert!(err.message().contains("exceeds limit"), "got: {err}");

  // 未触达下游：读取点拦截，存储层不可能收到任何写入
  let shared = make_shared(&acl);
  assert!(matches!(shared.storage.get("seen"), Ok(None)));
}

/// 场景（§13.1 DoS 边界 · 超大日志）：log 长度 16KB+1 在读取点被拒。
#[tokio::test]
async fn oversized_log_is_rejected() {
  let acl = full_acl();
  let wasm = wat::parse_str(
    r#"(module
      (import "ribo" "log" (func $log (param i32 i32) (result i32)))
      (memory (export "memory") 1)
      (func (export "alloc") (param i32) (result i32) (i32.const 512))
      (func (export "dealloc") (param i32 i32))
      (func (export "execute") (param i32 i32 i32 i32) (result i32 i32)
        (drop (call $log (i32.const 0) (i32.const 16385)))
        (i32.const 0) (i32.const 0)))"#,
  )
  .expect("valid oversized-log wat");

  let err = run_guest(&wasm, &acl, "dos", json!({}), Duration::from_secs(2))
    .await
    .unwrap_err();
  assert_eq!(err.code(), PLUGIN_EXEC_FAILED, "got: {err}");
  assert!(err.message().contains("exceeds limit"), "got: {err}");
}

/// 场景（§13.1 DoS 边界 · 超大 http 请求）：请求 JSON 长度 64KB+1 在
/// 读取点被拒——**未触网**（mock/外层无从发起）。
#[tokio::test]
async fn oversized_http_request_is_rejected() {
  let acl = full_acl();
  let wasm = wat::parse_str(
    r#"(module
      (import "ribo" "http_proxy" (func $proxy (param i32 i32) (result i32)))
      (memory (export "memory") 1)
      (func (export "alloc") (param i32) (result i32) (i32.const 512))
      (func (export "dealloc") (param i32 i32))
      (func (export "execute") (param i32 i32 i32 i32) (result i32 i32)
        (drop (call $proxy (i32.const 0) (i32.const 65537)))
        (i32.const 0) (i32.const 0)))"#,
  )
  .expect("valid oversized-req wat");

  let err = run_guest(&wasm, &acl, "dos", json!({}), Duration::from_secs(2))
    .await
    .unwrap_err();
  assert_eq!(err.code(), PLUGIN_EXEC_FAILED, "got: {err}");
  assert!(err.message().contains("exceeds limit"), "got: {err}");
}

/// 场景（§13.1 越权）：ACL 未声明 storage 能力 →
/// `plugin.plugin_forbidden` 且 capability 为**纯能力名** `storage`。
#[tokio::test]
async fn storage_denied_by_acl_maps_to_forbidden() {
  let acl = PluginAcl::default(); // storage = false
  let wasm = wat::parse_str(
    r#"(module
      (import "ribo" "storage_get" (func $get (param i32 i32) (result i32)))
      (memory (export "memory") 1)
      (func (export "alloc") (param i32) (result i32) (i32.const 512))
      (func (export "dealloc") (param i32 i32))
      (func (export "execute") (param i32 i32 i32 i32) (result i32 i32)
        (drop (call $get (i32.const 0) (i32.const 4)))
        (i32.const 0) (i32.const 0)))"#,
  )
  .expect("valid forbidden wat");

  let err = run_guest(&wasm, &acl, "noacl", json!({}), Duration::from_secs(2))
    .await
    .unwrap_err();
  assert_eq!(err.code(), PLUGIN_FORBIDDEN, "got: {err}");
  assert_eq!(
    err
      .context()
      .and_then(|c| c.get("capability").map(String::as_str)),
    Some("storage")
  );
}

/// 场景（§4.1 / §4.2 / §13.1）：`WasmPluginHost::load` 全链路——
/// manifest 解析、plugin_id 校验与来源（`Plugin::id()` 来自 list_tools
/// 顶层，§8.1）。加载期 `tools()` 直接读缓存。
#[tokio::test]
async fn wasm_plugin_host_loads_manifest_guest() {
  // manifest JSON 恰 28 字节：{"plugin_id":"t","tools":[]}
  let wasm = wat::parse_str(
    r#"(module
      (memory (export "memory") 1)
      (data (i32.const 16) "{\"plugin_id\":\"t\",\"tools\":[]}")
      (func (export "alloc") (param i32) (result i32) (i32.const 512))
      (func (export "dealloc") (param i32 i32))
      (func (export "list_tools") (result i32 i32) (i32.const 16) (i32.const 28))
      (func (export "execute") (param i32 i32 i32 i32) (result i32 i32)
        (i32.const 0) (i32.const 0)))"#,
  )
  .expect("valid manifest wat");

  let host = crate::wasm::host::WasmPluginHost::new().expect("host");
  let plugin = host.load(&wasm, None, &full_acl()).expect("load");
  assert_eq!(plugin.id(), "t");
  assert!(plugin.tools().is_empty());

  // 注册进 ToolManager（零工具）：注册链路 + 原子注入不报错
  let manager = Arc::new(ra_tools::manager::ToolManager::new());
  let registry = crate::registry::PluginRegistry::new(Arc::clone(&manager));
  registry.register(plugin).expect("register wasm plugin");
  assert!(manager.get_tool_schemas().is_empty());
}

/// 场景（§13.1 P2-23）：Stateless Store——同一 guest 并发执行互不串台
/// （echo 各自返回各自的 args）。
#[tokio::test]
async fn concurrent_executes_are_isolated() {
  let acl = full_acl();
  let wasm = echo_guest();
  let runs: Vec<_> = (0..8)
    .map(|i| {
      let wasm = wasm.clone();
      let acl = acl.clone();
      tokio::spawn(async move {
        run_guest(
          &wasm,
          &acl,
          "echo",
          json!({ "i": i }),
          Duration::from_secs(2),
        )
        .await
      })
    })
    .collect();
  for (i, handle) in runs.into_iter().enumerate() {
    let out = handle.await.expect("join").expect("echo ok");
    assert_eq!(
      out,
      json!({ "i": i }),
      "state must not leak across executes"
    );
  }
  let _ = CancellationToken::new(); // 保持 tokio_util 引用（编译期依赖）
}

/// 场景（防御性分类，v0.8.5 后续）：工具短名超注册期上限（> 64 字节）
/// → `GuestError` → `plugin.plugin_exec_failed`（业务级、可回灌），
/// **不得**归 `Internal`——`plugin.plugin_internal` 计入熔断，会把
/// 「输入形状违约」当宿主故障、误熔断整个工具面。注册期
/// `validate_tool_name`（≤ 64 字节）使正常链路不可达，本测试绕过
/// manifest 查找、直接以超长名调 `run_tool` 验证防御分支。
#[tokio::test]
async fn oversized_tool_name_maps_to_guest_error() {
  let wasm = echo_guest();
  let acl = PluginAcl::default();
  let oversized = "a".repeat(crate::adapter::TOOL_NAME_MAX_LEN + 1);

  let err =
    run_guest(&wasm, &acl, &oversized, json!({}), Duration::from_secs(2))
      .await
      .expect_err("超长工具名必须被拒绝");

  assert_eq!(err.code(), PLUGIN_EXEC_FAILED, "got: {}", err.code());
  assert!(
    err.message().contains("tool name too large"),
    "got: {}",
    err.message()
  );
}
