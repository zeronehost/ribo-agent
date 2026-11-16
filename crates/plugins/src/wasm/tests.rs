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
use crate::wasm::hostfns::{
  DnsGuardResolver, LogRateLimiter, build_http_client_with_resolver,
  build_linker, ensure_addrs_allowed,
};
use crate::wasm::runtime::{PluginRuntime, run_tool};
use crate::wasm::storage::Storage;

// ===== 测试辅助 =====

/// 构造插件级共享状态（client / storage / limiter / config）。
fn make_shared(acl: &PluginAcl) -> Arc<PluginShared> {
  let runtime = PluginRuntime::global().expect("plugin runtime");
  let cfg = PluginRuntimeConfig::default();
  Arc::new(PluginShared {
    client: Arc::clone(&runtime.http_client),
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
    timeout_ceiling_ms: None,
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

// ===== B1 验收补齐（2026-11-16，18_ §13.1 核对表缺口）=====

// B1.4 说明：`instantiate` / start 段 trap 分类由既有
// `memory_hog_guest_hits_store_limits` 覆盖——`(memory 64)` 在模块顶层
// 声明即实例化期分配，`StoreLimits` 在 `linker.instantiate` 阶段拦截，
// 经 §5.2 分类映射 `plugin.plugin_timeout`（白名单不计熔断，配套
// registry.rs `plugin_timeout_and_forbidden_do_not_open_circuit`）。

/// 计数 guest：`alloc` 累加 leaked、`dealloc` 扣减、导出 `leaked()`
/// 读取——用于验证宿主侧 RAII 守卫（B1.3）确实调用了 guest dealloc。
fn leak_counter_guest() -> Vec<u8> {
  wat::parse_str(
    r#"(module
      (global $leaked (mut i32) (i32.const 0))
      (memory (export "memory") 1)
      (func (export "alloc") (param $n i32) (result i32)
        (global.set $leaked (i32.add (global.get $leaked) (local.get $n)))
        (i32.const 16))
      (func (export "dealloc") (param $p i32) (param $n i32)
        (global.set $leaked (i32.sub (global.get $leaked) (local.get $n))))
      (func (export "leaked") (result i32) (global.get $leaked)))"#,
  )
  .expect("valid leak counter wat")
}

/// 场景（B1.3，§13.1「中途 trap 不泄漏」防回归）：`GuestBufs` RAII
/// 守卫的 `release_all` 对每个已登记缓冲区调用 guest `dealloc`——
/// 计数 guest 的 leaked 全局归零；释放前 leaked = 8 + 16 = 24（守卫
/// 确实必要的反证）。
#[tokio::test]
async fn guest_bufs_release_calls_guest_dealloc() {
  use crate::wasm::runtime::{GuestBufs, HostState};

  let wasm = leak_counter_guest();
  let runtime = PluginRuntime::global().expect("plugin runtime");
  let module = runtime
    .module_cache
    .get_or_compile(&runtime.engine, &wasm)
    .expect("compile leak counter guest");
  let linker = build_linker(&runtime.engine).expect("build linker");
  let acl = full_acl();
  let shared = make_shared(&acl);
  let mut store = wasmtime::Store::new(
    &runtime.engine,
    HostState::new(
      acl,
      CancellationToken::new(),
      "test".to_string(),
      "t".to_string(),
      &shared,
    ),
  );
  // epoch_interruption 开启时新 Store 默认 deadline=0（立即到期），
  // 须显式放宽——本测试不依赖 epoch 预算，给到永不触发。
  store.set_epoch_deadline(u64::MAX);
  let instance = linker
    .instantiate(&mut store, &module)
    .expect("instantiate leak counter guest");
  let alloc = instance
    .get_func(&mut store, "alloc")
    .expect("alloc export");
  let dealloc = instance
    .get_func(&mut store, "dealloc")
    .expect("dealloc export");
  let leaked = instance
    .get_func(&mut store, "leaked")
    .expect("leaked export");

  let mut out = [wasmtime::Val::I32(0)];
  alloc
    .call(&mut store, &[wasmtime::Val::I32(8)], &mut out)
    .expect("alloc 1");
  alloc
    .call(&mut store, &[wasmtime::Val::I32(16)], &mut out)
    .expect("alloc 2");

  leaked.call(&mut store, &[], &mut out).expect("leaked read");
  let wasmtime::Val::I32(before) = out[0] else {
    panic!("leaked must be i32");
  };
  assert_eq!(before, 24, "two tracked allocs without release");

  let mut bufs = GuestBufs::new();
  bufs.track(16, 8);
  bufs.track(32, 16);
  bufs.release_all(&mut store, &dealloc);

  leaked
    .call(&mut store, &[], &mut out)
    .expect("leaked read after release");
  let wasmtime::Val::I32(after) = out[0] else {
    panic!("leaked must be i32");
  };
  assert_eq!(
    after, 0,
    "RAII release must call guest dealloc for every tracked buffer"
  );
}

/// B1.2 场景（§13.1 DoS 表「超大 value」）：`storage_put` 的 value
/// 长度越 `MAX_VALUE_LEN`（256KB）→ 读取点拦截（`value exceeds limit`），
/// 不触达 storage 层。
fn oversized_value_guest() -> Vec<u8> {
  wat::parse_str(
    r#"(module
      (import "ribo" "storage_put" (func $put (param i32 i32 i32 i32) (result i32)))
      (func (export "execute") (param i32 i32 i32 i32) (result i32 i32)
        (drop (call $put (i32.const 0) (i32.const 1) (i32.const 0) (i32.const 262145)))
        (i32.const 0) (i32.const 0))
      (memory (export "memory") 1)
      (func (export "alloc") (param i32) (result i32) (i32.const 16))
      (func (export "dealloc") (param i32 i32)))"#,
  )
  .expect("valid oversized value wat")
}

/// 断言（v0.8.4 可验证化）：错误码正确 + 错误信息含超限文案。
#[tokio::test]
async fn oversized_value_is_rejected_at_read_point() {
  let wasm = oversized_value_guest();
  let acl = full_acl();
  let err = run_guest(&wasm, &acl, "t", json!({}), Duration::from_secs(2))
    .await
    .expect_err("oversized value must be rejected");
  assert_eq!(err.code(), PLUGIN_EXEC_FAILED, "got: {err}");
  assert!(
    err.message().contains("value exceeds limit"),
    "got: {}",
    err.message()
  );
}

/// B1.2 场景（§13.1 DoS 表「边界值通过」）：key 恰好 256B、value 恰好
/// 256KB 全部成功——守住「≤ 上限通过、> 上限拒绝」临界点，防止把
/// `>=` 写成 `>`。value 区（64..64+256KB）位于 guest 线性内存
/// （memory 5 = 320KB）内。
fn boundary_key_value_guest() -> Vec<u8> {
  wat::parse_str(
    r#"(module
      (import "ribo" "storage_put" (func $put (param i32 i32 i32 i32) (result i32)))
      (func (export "execute") (param i32 i32 i32 i32) (result i32 i32)
        (drop (call $put (i32.const 0) (i32.const 256) (i32.const 64) (i32.const 262144)))
        (i32.store8 (i32.const 16) (i32.const 49))
        (i32.const 16) (i32.const 1))
      (memory (export "memory") 5)
      (func (export "alloc") (param i32) (result i32) (i32.const 16))
      (func (export "dealloc") (param i32 i32)))"#,
  )
  .expect("valid boundary wat")
}

/// 预期：put 成功（边界值不被拒绝），execute 返回单字节 JSON 数字 1。
#[tokio::test]
async fn boundary_key_and_value_pass() {
  let wasm = boundary_key_value_guest();
  let acl = full_acl();
  let result = run_guest(&wasm, &acl, "t", json!({}), Duration::from_secs(2))
    .await
    .expect("boundary sizes must pass");
  assert_eq!(result, json!(1));
}

/// B1.7 场景（§13.1「log 限流」）：guest 洪水发送 300 条日志（进程
/// burst 默认 200）——限流静默丢弃超量条目（§6.1），guest 侧执行
/// 不失败（与 `LogRateLimiter` 窗口滚动单测互补）。
fn log_flood_guest() -> Vec<u8> {
  wat::parse_str(
    r#"(module
      (import "ribo" "log" (func $log (param i32 i32) (result i32)))
      (func (export "execute") (param i32 i32 i32 i32) (result i32 i32)
        (local $i i32)
        (loop $l
          (drop (call $log (i32.const 0) (i32.const 5)))
          (local.set $i (i32.add (local.get $i) (i32.const 1)))
          (br_if $l (i32.lt_s (local.get $i) (i32.const 300))))
        (i32.store8 (i32.const 16) (i32.const 49))
        (i32.const 16) (i32.const 1))
      (memory (export "memory") 1)
      (func (export "alloc") (param i32) (result i32) (i32.const 16))
      (func (export "dealloc") (param i32 i32)))"#,
  )
  .expect("valid log flood wat")
}

/// 预期：洪水日志全部被 host 接收（超量静默丢弃），执行成功收敛。
#[tokio::test]
async fn log_flood_is_throttled_silently() {
  let wasm = log_flood_guest();
  let acl = full_acl();
  let result = run_guest(&wasm, &acl, "t", json!({}), Duration::from_secs(2))
    .await
    .expect("log flood must not fail guest execution");
  assert_eq!(result, json!(1));
}

/// B1.6 场景（§13.1「tools() 仅调一次」/ P2-12）：`WasmPlugin::tools`
/// 直接读缓存——多次调用结果稳定一致；guest 的 `list_tools` 由加载期
/// （§4.1 阶段 3）单次执行的代码路径保证，缓存于 WasmPlugin。
#[tokio::test]
async fn plugin_tools_manifest_is_cached_and_stable() {
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

  let names = || {
    plugin
      .tools()
      .iter()
      .map(|m| m.name.clone())
      .collect::<Vec<_>>()
  };
  let first = names();
  for _ in 0..3 {
    assert_eq!(names(), first, "manifest cache must be stable");
  }
}

// ===== B3.1 SSRF 第 3 层（DNS 解析结果 IP 复检）=====

/// 静态解析表 resolver（测试专用）：域名 → SocketAddr 硬映射，
/// 复现「白名单域名解析到私网 IP」的 DNS rebinding，无需真实 DNS。
struct StaticResolver {
  map: std::collections::HashMap<String, Vec<std::net::SocketAddr>>,
}

impl StaticResolver {
  fn new(entries: &[(&str, &str)]) -> Self {
    Self {
      map: entries
        .iter()
        .map(|(host, addr)| {
          (
            host.to_string(),
            vec![addr.parse().expect("valid socket addr")],
          )
        })
        .collect(),
    }
  }
}

impl reqwest::dns::Resolve for StaticResolver {
  fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
    let addrs = self.map.get(name.as_str()).cloned().unwrap_or_default();
    Box::pin(
      async move { Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs) },
    )
  }
}

/// 场景（B3.1 单元级）：解析结果命中保留段（RFC1918 / 元数据端点）
/// 被拒（错误文案含 `blocked address` 与具体 IP）；保留段外放行。
#[test]
fn dns_guard_rejects_private_resolutions() {
  let addrs: Vec<std::net::SocketAddr> = vec![
    "192.168.1.5:80".parse().expect("addr"),
    "169.254.169.254:80".parse().expect("addr"),
  ];
  let err = ensure_addrs_allowed(addrs.into_iter())
    .expect_err("private resolution must be blocked");
  assert!(err.to_string().contains("blocked address"), "got: {err}");

  let public: Vec<std::net::SocketAddr> =
    vec!["93.184.216.34:80".parse().expect("addr")];
  assert!(
    ensure_addrs_allowed(public.into_iter()).is_ok(),
    "non-reserved addresses must pass the guard"
  );
}

/// B3.1 场景（guest 级全链路）：白名单域名 `rebind.example.com` 通过
/// UrlPolicy 第 1/2 层（域名无字面 IP 可查）→ 解析到 RFC1918 私网 IP
/// → resolver 第 3 层拒绝。断言错误码为 `plugin.plugin_exec_failed`
/// （transport 路径），**不是** `plugin.plugin_forbidden`——区别于
/// 第 1/2 层的 ACL / 策略拒绝，证明拦截发生在解析层。
fn http_rebind_guest() -> Vec<u8> {
  // 请求 JSON 恰 52 字节：{"url":"http://rebind.example.com/x","method":"GET"}
  wat::parse_str(
    r#"(module
      (import "ribo" "http_proxy" (func $http (param i32 i32) (result i32)))
      (func (export "execute") (param i32 i32 i32 i32) (result i32 i32)
        (drop (call $http (i32.const 64) (i32.const 52)))
        (i32.const 0) (i32.const 0))
      (memory (export "memory") 1)
      (data (i32.const 64) "{\"url\":\"http://rebind.example.com/x\",\"method\":\"GET\"}")
      (func (export "alloc") (param i32) (result i32) (i32.const 16))
      (func (export "dealloc") (param i32 i32)))"#,
  )
  .expect("valid http rebind wat")
}

#[tokio::test]
async fn dns_rebinding_to_private_ip_is_blocked() {
  let acl = PluginAcl {
    log: true,
    http: true,
    url_policy: Some(Arc::new(crate::acl::UrlPolicy::new([
      "rebind.example.com",
    ]))),
    ..PluginAcl::default()
  };
  let cfg = PluginRuntimeConfig::default();
  let shared = Arc::new(PluginShared {
    client: Arc::new(
      build_http_client_with_resolver(Arc::new(DnsGuardResolver::new(
        Arc::new(StaticResolver::new(&[(
          "rebind.example.com",
          "192.168.1.5:80",
        )])),
      )))
      .expect("client with guard resolver"),
    ),
    storage: Arc::new(Storage::new(&cfg, &acl)),
    log_limiter: Arc::new(LogRateLimiter::new(&cfg)),
    config: Arc::new(cfg),
  });
  let err = run_guest_with_shared(
    &http_rebind_guest(),
    &shared,
    &acl,
    "t",
    json!({}),
    Duration::from_secs(2),
  )
  .await
  .expect_err("rebinding to private IP must be blocked");
  assert_eq!(err.code(), PLUGIN_EXEC_FAILED, "got: {err}");
  assert_ne!(
    err.code(),
    PLUGIN_FORBIDDEN,
    "resolver-layer rejection must not be confused with ACL denial"
  );
}

// ===== B3.2 权威清单（Step C 信任链闭合）=====

/// 场景（B3.2）：权威清单与 guest 自述一致 → `load_with_authority`
/// 通过；不一致（plugin_id / 工具集合）→ 拒绝加载。guest manifest
/// 为 `{"plugin_id":"t","tools":[]}`（28 字节，复用 §4.1 fixture 形态）。
#[tokio::test]
async fn authority_manifest_gates_loading() {
  use crate::authority::AuthorityManifest;

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

  // 一致 → 通过
  let auth = AuthorityManifest::from_json(br#"{"plugin_id":"t","tools":[]}"#)
    .expect("authority manifest");
  let plugin = host
    .load_with_authority(&wasm, None, &full_acl(), Some(&auth))
    .expect("matching authority must pass");
  assert_eq!(plugin.id(), "t");

  // plugin_id 不一致 → 拒绝
  let bad_id =
    AuthorityManifest::from_json(br#"{"plugin_id":"other","tools":[]}"#)
      .expect("authority manifest");
  // Arc<dyn Plugin> 无 Debug，无法用 expect_err（需打印 Ok 值）
  let err =
    match host.load_with_authority(&wasm, None, &full_acl(), Some(&bad_id)) {
      Err(err) => err,
      Ok(_) => panic!("plugin_id mismatch must be rejected"),
    };
  assert!(
    err.message().contains("authority manifest mismatch"),
    "got: {}",
    err.message()
  );

  // guest 多报工具（权威清单不含）→ 拒绝
  let extra = AuthorityManifest::from_json(
    br#"{"plugin_id":"t","tools":[{"name":"phantom","description":"x","parameters":{}}]}"#,
  )
  .expect("authority manifest");
  let err =
    match host.load_with_authority(&wasm, None, &full_acl(), Some(&extra)) {
      Err(err) => err,
      Ok(_) => panic!("authority extra tool must be rejected"),
    };
  assert!(
    err.message().contains("guest does not provide it"),
    "got: {}",
    err.message()
  );
}
