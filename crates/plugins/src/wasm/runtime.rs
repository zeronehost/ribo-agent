//! WASM 执行引擎与取消机制（设计文档 18_ §5）。
//!
//! 核心模型（v0.4 定稿，v0.8 更名 `PluginRuntime`）：
//!
//! - **进程级单例**：`Engine` + 唯一 epoch 推进线程 + `ModuleCache`
//!   收归 [`PluginRuntime`]（`OnceLock`），任意数量的
//!   `WasmPluginHost` 共享同一 `&'static PluginRuntime`——epoch 推进
//!   频率与 host 数量无关（多 host 各自 spawn ticker 会让 epoch
//!   翻倍推进、所有 Store 提前到期）；
//! - **Stateless Store**：每次 `execute` 新建 `Store + Instance`，并发
//!   调用天然隔离、无状态串台；`Module` 编译一次全局缓存（blake3 键）；
//! - **取消不推 epoch**：`engine.increment_epoch()` 只允许出现在
//!   `PluginRuntime::start` 的独立 OS 线程闭包中（§5.4 实现禁令）；
//!   取消信号只经 `CancellationToken` 传播，每 Store 独立 deadline。
//!
//! `Store: !Send`——`Store::new` + `linker.instantiate` + 执行全部在
//! `spawn_blocking` 闭包内完成，闭包边界即 `Send` 边界（§0.4 纪律 2）。

use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use lru::LruCache;
use tokio_util::sync::CancellationToken;

use crate::acl::PluginAcl;
use crate::config::PluginRuntimeConfig;
use crate::error::{
  plugin_exec_failed, plugin_forbidden, plugin_internal, plugin_timeout,
  truncate_error,
};
use crate::wasm::hostfns::LogRateLimiter;
use crate::wasm::storage::Storage;

/// epoch tick：ticker 每 10ms 推进一次（精度需求固定，§14 DoD 的量化
/// 区间 `T ∈ [timeout, timeout + EPOCH_TICK]` 依赖此常量）。
const EPOCH_TICK: Duration = Duration::from_millis(10);

/// 由超时时长换算 epoch tick 数：**向上取整**（v0.7 §三.3）。
///
/// floor 会把 15ms 算成 1 tick（=10ms），实际等待 < 请求的 15ms，
/// 超时断言测试会永远失败；ceil 保证「等待时长 ≥ 请求的 timeout」。
pub(crate) fn epoch_ticks_for(timeout: Duration) -> u64 {
  let tick = EPOCH_TICK.as_millis();
  timeout.as_millis().div_ceil(tick).max(1) as u64
}

/// 构建 wasmtime Engine：开启 epoch interruption（确定性中断的前提），
/// fuel 由配置读取（Step B 恒 false，§0.3 #5）。
fn build_engine(
  runtime_cfg: &PluginRuntimeConfig,
) -> wasmtime::Result<wasmtime::Engine> {
  let mut cfg = wasmtime::Config::new();
  cfg.epoch_interruption(true); // 必须开启，否则 set_epoch_deadline 无效
  cfg.consume_fuel(runtime_cfg.enable_fuel);
  wasmtime::Engine::new(&cfg)
}

/// Module 缓存：以 wasm 字节内容的 blake3 哈希为键，与 Engine 同生命周期。
///
/// 内容哈希（而非路径）保证「同一份 wasm 从不同路径加载也能命中」；
/// blake3 为进程间稳定哈希，未来磁盘缓存不需改键（§0.3 决策 5）。
/// 容量由 [`PluginRuntimeConfig::module_cache_capacity`] 注入，超限按
/// **LRU** 淘汰最久未使用的条目（§4.1 新问题 5 的收口）：
/// 恶意插件反复换字节加载会填充缓存，无上界即无界内存增长。
pub struct ModuleCache {
  entries: Mutex<LruCache<[u8; 32], Arc<wasmtime::Module>>>,
}

/// 缓存临界区的 guard 别名（把 poison 处理收口在 [`ModuleCache::lock_entries`]）。
type EntriesGuard<'a> =
  MutexGuard<'a, LruCache<[u8; 32], Arc<wasmtime::Module>>>;

impl ModuleCache {
  pub(crate) fn new(capacity: usize) -> Self {
    // NonZeroUsize 不接受 0；进程配置保证 > 0，此处防御性兜底为 1。
    let cap = NonZeroUsize::new(capacity.max(1)).expect("non-zero capacity");
    Self {
      entries: Mutex::new(LruCache::new(cap)),
    }
  }

  /// 取缓存锁：poison 时返回错误而非 panic。
  ///
  /// mutex poison 意味着持锁线程曾在临界区内 panic，缓存内部结构可能已
  /// 损坏——不强行 `into_inner()` 恢复，而是把 poison 交给上层按错误路径
  /// 处理（graceful shutdown），避免 panic 在 spawn_blocking / 宿主 线程
  /// 中升级为进程级故障。
  fn lock_entries(&self) -> wasmtime::Result<EntriesGuard<'_>> {
    self.entries.lock().map_err(|e| {
      // 关键错误分支：缓存内部结构可能已损坏，记录后交上层走错误路径
      tracing::error!("module cache mutex poisoned");
      wasmtime::Error::msg(format!("module cache mutex poisoned: {e}"))
    })
  }

  /// 取缓存 Module；未命中则编译并写入缓存（多 host 对同一 wasm 只编译一次）。
  ///
  /// 编译在锁外进行（构建模块可能耗时，持锁会串行化所有插件加载）；
  /// 写回前二次检查，避免并发编译时同一 hash 被重复插入 / 覆盖。
  ///
  /// # Errors
  ///
  /// - 缓存锁 poisoned → `wasmtime::Error`（上层映射为 `plugin_internal`）；
  /// - wasm 编译失败 → `wasmtime::Error`。
  pub(crate) fn get_or_compile(
    &self,
    engine: &wasmtime::Engine,
    wasm_bytes: &[u8],
  ) -> wasmtime::Result<Arc<wasmtime::Module>> {
    let hash = *blake3::hash(wasm_bytes).as_bytes();
    if let Some(m) = self.lock_entries()?.get(&hash) {
      return Ok(Arc::clone(m));
    }
    let module = Arc::new(wasmtime::Module::new(engine, wasm_bytes)?);
    let mut entries = self.lock_entries()?;
    if let Some(existing) = entries.get(&hash) {
      return Ok(Arc::clone(existing));
    }
    entries.put(hash, Arc::clone(&module));
    Ok(module)
  }

  /// 缓存条目数（仅测试用）。poison 时取回 guard 只读长度——断言用途，
  /// 不执行插件代码，也不 panic。
  #[cfg(test)]
  pub(crate) fn len(&self) -> usize {
    self.entries.lock().unwrap_or_else(|e| e.into_inner()).len()
  }
}

impl core::fmt::Debug for ModuleCache {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    // Debug 绝不可 panic：若 panic 展开期间触发 Debug（如 unwrap_err
    // 打印错误值），二次 panic 会直接 abort 进程。poison 时只报告标记，
    // 不读取可能不一致的缓存内容。
    match self.entries.lock() {
      Ok(entries) => f
        .debug_struct("ModuleCache")
        .field("len", &entries.len())
        .field("cap", &entries.cap().get())
        .finish(),
      Err(_) => f
        .debug_struct("ModuleCache")
        .field("poisoned", &true)
        .finish(),
    }
  }
}

/// 进程级单例：Engine + 唯一 epoch 推进线程 + ModuleCache + 运行参数。
///
/// 职责边界：
///
/// - `engine`：编译 / 实例化配置，供所有 Store 共享；
/// - `module_cache`：blake3 内容为键的 Module 缓存（与 engine 同生命周期）；
/// - 后台 ticker：本类型唯一的「时钟」职责（独立 OS 线程，见 [`start`](Self::start)）。
///
/// **禁令（§5.4）**：`engine.increment_epoch()` 只允许出现在 `start` 的
/// 线程闭包中；`WasmPluginHost::new()` / `load()` / 取消路径 / host
/// function / `run_tool` 内一律禁止调用。
pub struct PluginRuntime {
  /// 共享 Engine（`epoch_interruption(true)` 已开启）。
  pub engine: wasmtime::Engine,
  /// 全局 Module 缓存（多 host 共享，同一 wasm 只编译一次）。
  pub module_cache: ModuleCache,
  /// 进程级运行参数（内存 / storage / 限流 / fuel），全 host 共享。
  pub cfg: PluginRuntimeConfig,
  /// 进程级共享 HTTP client（连接池 / DNS 守卫构造一次，所有插件复用）。
  /// 每插件各建一个 client 会让连接池按插件数碎片化，且重复构建守卫。
  pub http_client: Arc<reqwest::Client>,
}

impl core::fmt::Debug for PluginRuntime {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    f.debug_struct("PluginRuntime")
      .field("cfg", &self.cfg)
      .finish()
  }
}

impl PluginRuntime {
  /// 全局唯一入口（`OnceLock` 单例）。
  ///
  /// **不可重试语义（v0.7 §二.2，有意为之）**：一旦初始化返回 `Err`，
  /// `OnceLock` 会**永久缓存该错误**，整个进程无法恢复——epoch 基础
  /// 设施是进程级前提，初始化失败意味着运行环境不可用，应定位根因而
  /// 非靠重试掩盖。
  pub fn global() -> ra_core::error::RiboResult<&'static PluginRuntime> {
    static RUNTIME: std::sync::OnceLock<
      ra_core::error::RiboResult<PluginRuntime>,
    > = std::sync::OnceLock::new();
    // RiboError: Clone（source 为 Arc 持有），失败时可安全取出错误副本
    RUNTIME
      .get_or_init(Self::start)
      .as_ref()
      .map_err(Clone::clone)
  }

  /// 实际初始化：构建 Engine 并 spawn 唯一的 epoch 推进线程。
  ///
  /// ticker 是**进程级后台线程**：用独立 OS 线程 + `thread::sleep`
  /// （不绑定任何 tokio runtime，避免 `OnceLock` 里的 task 绑定首个
  /// 短生命周期测试 runtime）；`JoinHandle` 有意丢弃（detached，
  /// v0.8.1 P2-4）——`OnceLock` 保证全进程只会 spawn 一个，不提供
  /// stop / shutdown（v0.7 撤销 `shutdown_for_test`：停 ticker 后
  /// 单例无法重启会导致后续同进程测试假死）。
  fn start() -> ra_core::error::RiboResult<Self> {
    // ===== 初始化阶段：进程参数仅此处构造一次（OnceLock 后不可变） =====
    let cfg = PluginRuntimeConfig::from_env();
    let engine = build_engine(&cfg)
      .map_err(|e| plugin_internal("engine init failed", e))?;
    // HTTP client 同样是进程级基础设施：一次构造，所有插件共享同一连接池
    let http_client = Arc::new(crate::wasm::hostfns::build_http_client()?);

    // ===== 后台 ticker：唯一 epoch 推进者 =====
    let e = engine.clone();
    std::thread::Builder::new()
      .name("ra-plugins-epoch-ticker".into())
      .spawn(move || {
        loop {
          std::thread::sleep(EPOCH_TICK);
          e.increment_epoch(); // 唯一调用点（§5.4 实现禁令）
        }
      })
      .map_err(|e| plugin_internal("epoch ticker thread spawn", e))?;

    Ok(Self {
      engine,
      module_cache: ModuleCache::new(cfg.module_cache_capacity),
      cfg,
      http_client,
    })
  }
}

/// Store 的 data：所有 host function 的访问入口。
///
/// 每次 execute 新建，但插件级共享状态经 `Arc<PluginShared>` 跨调用
/// 存活（storage / 限流器 / 连接池不被 Stateless Store 重置）。
/// 约束：`Send + 'static`（§0.4 纪律 2，编译期断言见本模块测试）。
pub struct HostState {
  /// 本插件被授予的能力集合（含单工具收紧后的内存覆写）。
  pub acl: PluginAcl,
  /// 取消信号（`ctx.signal().clone()` 传入；host function 内响应）。
  pub signal: CancellationToken,
  /// 插件 ID（log / 错误 context 用）。
  pub plugin_id: String,
  /// 当前工具短名（log / 错误 context 用）。
  pub tool_name: String,
  /// HTTP 客户端（进程级单例的 Arc，连接池跨插件 / 跨调用复用）。
  pub client: Arc<reqwest::Client>,
  /// 插件级 storage（跨 Stateless Store 存活）。
  pub storage: Arc<Storage>,
  /// 插件级日志限流器。
  pub log_limiter: Arc<LogRateLimiter>,
  /// 内置 ResourceLimiter：线性内存上限（默认 32MB，可被插件收紧）。
  pub limits: wasmtime::StoreLimits,
}

impl HostState {
  /// 装配 HostState：内存上限取 `min(插件覆写, 进程默认)`（两阶段 min
  /// 的阶段 2，只能收紧；v0.8 #15 配置注入，不新增 run_tool 参数）。
  pub fn new(
    acl: PluginAcl,
    signal: CancellationToken,
    plugin_id: String,
    tool_name: String,
    shared: &Arc<crate::wasm::host::PluginShared>,
  ) -> Self {
    let cfg = &shared.config;
    let mem_cap = acl
      .memory_limit_bytes
      .map_or(cfg.memory_limit_bytes, |v| v.min(cfg.memory_limit_bytes));
    let limits = wasmtime::StoreLimitsBuilder::new()
      .memory_size(mem_cap)
      .instances(1)
      .build();
    Self {
      acl,
      signal,
      plugin_id,
      tool_name,
      client: Arc::clone(&shared.client),
      storage: Arc::clone(&shared.storage),
      log_limiter: Arc::clone(&shared.log_limiter),
      limits,
    }
  }

  /// 取 owned 快照，供 host function 闭包在 `&mut caller` 之前调用
  /// （v0.8 P0-#10：消除 `caller.data()` 不可变借用与 `&mut caller`
  /// 的借用冲突）。字段均为 Clone 且共享底层资源（非深拷贝）。
  pub fn snapshot(&self) -> HostStateSnapshot {
    HostStateSnapshot {
      acl: self.acl.clone(),
      signal: self.signal.clone(),
      plugin_id: self.plugin_id.clone(),
      tool_name: self.tool_name.clone(),
      client: Arc::clone(&self.client),
      storage: Arc::clone(&self.storage),
      log_limiter: Arc::clone(&self.log_limiter),
    }
  }
}

/// host function 闭包专用的 owned 快照（v0.8 P0-#10）。
///
/// `func_wrap` 闭包里先 `snapshot()` 取 owned 数据，`caller` 的不可变
/// 借用即结束，之后 `&mut caller` 合法（NLL 不总能证明生命周期不重叠，
/// 显式快照彻底消除 E0502）。
#[derive(Clone, Debug)]
pub struct HostStateSnapshot {
  /// 本插件被授予的能力集合。
  pub acl: PluginAcl,
  /// 取消信号。
  pub signal: CancellationToken,
  /// 插件 ID。
  pub plugin_id: String,
  /// 当前工具短名。
  pub tool_name: String,
  /// HTTP 客户端（共享进程级连接池）。
  pub client: Arc<reqwest::Client>,
  /// 插件级 storage。
  pub storage: Arc<Storage>,
  /// 插件级日志限流器。
  pub log_limiter: Arc<LogRateLimiter>,
}

/// runtime 层与 `RiboError` 之间的中间错误类型（spawn_blocking 闭包返回）。
///
/// 不引入第二套对外错误类型：出 `run_tool` 时全部映射为 §1.2 的错误码。
#[derive(Debug)]
pub(crate) enum WasmRuntimeError {
  /// 系统级：epoch 到期 / 内存超限（`trap_reason` 记录来源）。
  Timeout(String),
  /// 业务级：guest 返回错误 / ABI 违约 / guest 自身 trap（可回灌 LLM）。
  GuestError(String),
  /// 越权：capability 为**纯能力名**（`storage` / `url_policy` / `log` / `http`）。
  Forbidden(String),
  /// 取消：host function 内 signal 触发。
  Cancelled,
  /// 宿主内部：Linker / instantiate / join / Engine / host 侧内存读写失败。
  Internal(String),
}

/// 取 wasmtime 错误消息里的 trap 载荷（v0.8.3 P1-1，v0.8.4 P3-1）。
///
/// wasmtime 的 trap 消息形如 `"wasm trap: <payload>"`（可能包 backtrace /
/// cause 链）。取**第一个** `"wasm trap: "` 之后的全部内容；没有该标记
/// 时返回 `None`（普通 wasmtime 错误，如 epoch / 内存超限）。用 `find`
/// 而非 `rfind`：payload 是 `Trap::new()` 传入的原串，契约上不含
/// `"wasm trap: "` 本身，取第一个即可。
fn trap_payload(msg: &str) -> Option<&str> {
  msg
    .find("wasm trap: ")
    .map(|i| &msg[i + "wasm trap: ".len()..])
}

/// 线性内存超限的判定（v0.8.3 P1-1）。
///
/// **禁止裸 `contains("memory")`**：宿主自造的
/// `plugin_internal("guest memory export missing")` 同样含 "memory"，
/// 裸匹配会把宿主内部错误判成 Timeout（漏计熔断）。只认 wasmtime 的
/// 内存超限措辞候选集合——措辞随版本变化，锁定版本后按实测收敛，
/// 并以 §13.1 回归测试钉住。
fn is_memory_limit_message(msg: &str) -> bool {
  const MARKERS: [&str; 4] = [
    "out of memory",
    "memory limit",
    "memory_limit",
    "exceeds memory limits",
  ];
  MARKERS.iter().any(|m| msg.contains(m))
}

/// epoch 到期判定（措辞候选集合，随版本收敛）。
///
/// wasmtime 50 实测：epoch interruption 触发的是预定义
/// `Trap::Interrupt`（Display 为 `"wasm trap: interrupt"`）；
/// 同时保留旧版措辞候选，锁定版本后按实测收敛。
fn is_epoch_exhausted_message(msg: &str) -> bool {
  msg.contains("wasm trap: interrupt")
    || msg.contains("epoch_exhausted")
    || msg.contains("epoch deadline exceeded")
}

/// 第零层分类：wasmtime 结构化 [`Trap`](wasmtime::Trap)（v0.8.8 P0-1）。
///
/// wasmtime 50 的 `wasmtime::Error`（anyhow 兼容）可在错误链中
/// `downcast_ref::<Trap>()`；遍历整条链而非只看 top-level，兼容
/// `Func::call` 用 context 包裹 trap 的形态。结构化判定优先于字符串：
///
/// - `Trap::Interrupt`（epoch）/ `Trap::OutOfFuel`（fuel）→ Timeout
///   （资源限制，计入熔断、不计 guest 责任）；
/// - 其余预定义 trap（`unreachable` / 除零 / OOB 等）→ GuestError
///   （guest 自身业务失败，可回灌 LLM）。
///
/// host function 经 `WasmError::msg("forbidden:...")` 产生的错误**不是**
/// Trap，仍由下一层结构化前缀字符串匹配处理。锁定的 wasmtime 50 不存在
/// `Trap::i32_exit_status`（WASI 层的 `I32Exit` 与本宿主无关），故直接
/// 以 Trap 枚举变体判定，不做已废弃 API 的字符串模拟。
fn classify_trap(e: &wasmtime::Error) -> Option<WasmRuntimeError> {
  for cause in e.chain() {
    if let Some(trap) = cause.downcast_ref::<wasmtime::Trap>() {
      let reason = trap.to_string();
      return Some(match trap {
        wasmtime::Trap::Interrupt | wasmtime::Trap::OutOfFuel => {
          WasmRuntimeError::Timeout(reason)
        }
        _ => WasmRuntimeError::GuestError(reason),
      });
    }
  }
  None
}

/// 第一层分类：结构化前缀精确匹配（§5.2 三段式）。
///
/// 兼容两种形态（wasmtime 50 实测校准）：
///
/// ① **host function 直接返回的错误**——消息本身就是结构化前缀
///    （`Err` 原样从 `Func::call` 透出，无 "wasm trap: " 包裹）；
/// ② **guest 自身 trap**——消息形如 `"wasm trap: <payload>"`
///    （经 [`trap_payload`] 剥出后再匹配，保留兼容）。
///
/// 必须用 `strip_prefix` **精确匹配**——不得用全文 `contains`
/// （guest 业务文本里的 "cancelled" 字样会被误判为取消、误杀整个
/// 并行工具批）。
fn classify_structured(msg: &str) -> Option<WasmRuntimeError> {
  fn match_payload(payload: &str) -> Option<WasmRuntimeError> {
    if let Some(stage) = payload.strip_prefix("internal:") {
      return Some(WasmRuntimeError::Internal(stage.to_string()));
    }
    if let Some(cap) = payload.strip_prefix("forbidden:") {
      // 只取 capability 段（纯能力名），供 §7 熔断表与 tracing 字段使用
      return Some(WasmRuntimeError::Forbidden(cap.to_string()));
    }
    if payload == "cancelled" {
      // 必须完全相等：guest 业务文本里的 "cancelled" 不带结构化前缀
      return Some(WasmRuntimeError::Cancelled);
    }
    if let Some(reason) = payload.strip_prefix("guest_error:") {
      // 消息模板为 `guest_error: {e}`（带空格），剥离后去掉首部空白
      return Some(WasmRuntimeError::GuestError(
        reason.trim_start().to_string(),
      ));
    }
    if payload == "storage_full" || payload == "storage_bad_key" {
      return Some(WasmRuntimeError::GuestError(payload.to_string()));
    }
    if let Some(stage) = payload.strip_prefix("storage_internal:") {
      return Some(WasmRuntimeError::Internal(stage.to_string()));
    }
    None
  }
  // ① host function 裸前缀
  if let Some(e) = match_payload(msg) {
    return Some(e);
  }
  // ② guest trap 包裹形态
  trap_payload(msg).and_then(match_payload)
}

/// 第二层分类：wasmtime 自身的 epoch / 内存超限措辞（候选集合）。
fn classify_resource(msg: &str) -> Option<WasmRuntimeError> {
  if is_epoch_exhausted_message(msg) || is_memory_limit_message(msg) {
    return Some(WasmRuntimeError::Timeout(msg.to_string()));
  }
  None
}

/// 将 wasmtime 错误分类为 [`WasmRuntimeError`]（用于 **instantiate**
/// 及加载期调用，含 start 段 trap，§5.2 新问题 1）。
///
/// 分类顺序：① 结构化前缀（最先判）→ ② epoch / 内存超限措辞 →
/// ③ **其余一律 Internal**（保守默认：计入熔断，暴露问题而非掩盖）。
///
/// **消息来源（wasmtime 50 实测校准）**：host function 返回的 `Err`
/// 会被包裹为 `"error while executing at wasm backtrace: ...\nCaused by:\n..."`
/// 的链式错误——结构化前缀在 **root cause** 中。故对 `to_string()`
/// 与 `root_cause()` 两层都做匹配。
pub(crate) fn classify_wasmtime_error(e: wasmtime::Error) -> WasmRuntimeError {
  if let Some(classified) = classify_trap(&e) {
    return classified;
  }
  let root = e.root_cause().to_string();
  classify_both(e.to_string(), &root)
    .unwrap_or(WasmRuntimeError::Internal(root))
}

/// 将 **guest 函数调用期间**的 wasmtime 错误分类（与
/// [`classify_wasmtime_error`] 的差别仅在兜底分支）。
///
/// guest 代码在执行期 trap（`unreachable` / 除零 / 主动 trap 等）属于
/// **guest 自身的业务级失败**——按 §5.3 / §7「trap →
/// `plugin.plugin_exec_failed`」映射为 [`WasmRuntimeError::GuestError`]，
/// 可回灌 LLM 自我修正；**不得**归 `Internal`（宿主故障、误计熔断）。
pub(crate) fn classify_guest_call_error(
  e: wasmtime::Error,
) -> WasmRuntimeError {
  if let Some(classified) = classify_trap(&e) {
    return classified;
  }
  let root = e.root_cause().to_string();
  classify_both(e.to_string(), &root)
    .unwrap_or(WasmRuntimeError::GuestError(root))
}

/// 对错误的两层消息（完整 Display 与 root cause）分别做
/// 结构化前缀与资源措辞匹配。
fn classify_both(msg: String, root: &str) -> Option<WasmRuntimeError> {
  classify_structured(&msg)
    .or_else(|| classify_structured(root))
    .or_else(|| classify_resource(&msg))
    .or_else(|| classify_resource(root))
}

/// `run_tool`：执行一次 WASM 插件工具（spawn_blocking 内新建 Stateless
/// Store → instantiate → ABI v0 调用 → 读回结果）。
///
/// 取消与超时：
///
/// - 每 Store 独立 `set_epoch_deadline(ticks)`（默认预算来自
///   `ToolManifest.timeout_hint`，默认 2s）；纯 CPU guest 死循环最迟
///   在自身预算到期时被 epoch 确定性中断，不影响其它 Store；
/// - 外层取消经 `ctx.signal()` 传播：host function 内 `select!` 响应；
///   外层 drop Future 时 `spawn_blocking` 线程不可取消，故 host function
///   必须响应 signal（否则最长占用一个超时周期）。
///
/// # Errors
///
/// 返回 §1.2 的插件错误码（按 [`WasmRuntimeError`] 分级映射，见 §5.3）。
// 签名由设计文档 18_ §5.2 固定（engine/module/linker/shared/acl/tool/
// args/ctx/plugin_id/timeout 十参，与 WasmPlugin::execute 的调用形态对齐），
// 参数打包成 struct 反而破坏文档契约，故记录理由后豁免参数数量检查。
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(
  skip_all,
  fields(plugin_id = %plugin_id, tool_name = %tool,
         duration_ms = tracing::field::Empty,
         trap_reason = tracing::field::Empty)
)]
pub async fn run_tool(
  engine: &wasmtime::Engine,
  module: &Arc<wasmtime::Module>,
  linker: &Arc<wasmtime::Linker<HostState>>,
  shared: &Arc<crate::wasm::host::PluginShared>,
  acl: &PluginAcl,
  tool: &str,
  args: &serde_json::Value,
  ctx: &ra_tools::context::ToolContext,
  plugin_id: &str,
  timeout: Duration,
) -> ra_core::error::RiboResult<serde_json::Value> {
  // ===== 准备阶段：clone 进闭包（Send 边界在 spawn_blocking 入口） =====
  let started = std::time::Instant::now();
  let signal: CancellationToken = ctx.signal().clone();
  let engine = engine.clone();
  let module = module.clone();
  let linker = linker.clone();
  let shared = shared.clone();
  let acl = acl.clone();
  let args = args.clone();
  let tool = tool.to_string();
  let plugin_id = plugin_id.to_string();
  let ticks = epoch_ticks_for(timeout);
  let deadline_ms = timeout.as_millis() as u64;
  // 闭包内自持一份（外层副本用于 await 后的错误映射，避免 move 冲突）
  let tool_in_task = tool.clone();
  let plugin_id_out = plugin_id.clone();

  let handle = tokio::task::spawn_blocking(
    move || -> Result<serde_json::Value, WasmRuntimeError> {
      // Store: !Send —— Store::new + instantiate + 执行全部在本线程内完成
      let state = HostState::new(
        acl,
        signal,
        plugin_id.clone(),
        tool_in_task.clone(),
        &shared,
      );
      let mut store = wasmtime::Store::new(&engine, state);
      store.set_epoch_deadline(ticks); // 每 Store 独立 deadline，互不影响
      store.limiter(|s| &mut s.limits); // 配置化内存上限，超限 -> Timeout
      // fuel 与 epoch 并行的第二道确定性中断（进程开关，OnceLock 后固定）
      if shared.config.enable_fuel {
        store
          .set_fuel(shared.config.fuel_limit)
          .map_err(|e| WasmRuntimeError::Internal(format!("set fuel: {e}")))?;
      }
      // start 段（如有）因 epoch / 内存超限 trap 时同样按前缀分类，
      // 不得一律判 Internal（会误计入熔断，§5.2 新问题 1）
      let instance = linker
        .instantiate(&mut store, &module)
        .map_err(classify_wasmtime_error)?;
      // ABI v0 胶水：alloc → 写 args → 调 execute → 读回 → dealloc
      abi_v0_execute(&mut store, &instance, &tool_in_task, &args)
    },
  );

  let outcome = handle.await;
  // duration_ms 需手动 record（tracing::instrument 的 fields 只是占位）
  tracing::Span::current()
    .record("duration_ms", started.elapsed().as_millis() as u64);

  // ===== 错误映射阶段：WasmRuntimeError → 插件错误码（§5.3） =====
  match outcome {
    Ok(Ok(v)) => Ok(v),
    Ok(Err(WasmRuntimeError::Timeout(reason))) => {
      tracing::Span::current().record("trap_reason", reason.as_str());
      tracing::warn!(plugin = %plugin_id_out, trap_reason = %reason, "plugin execution timeout");
      Err(plugin_timeout(&plugin_id_out, &tool, deadline_ms, &reason))
    }
    Ok(Err(WasmRuntimeError::GuestError(reason))) => {
      tracing::Span::current().record("trap_reason", reason.as_str());
      // UTF-8 安全截断（4KB 上限），完整错误只落日志
      Err(plugin_exec_failed(
        &plugin_id_out,
        &tool,
        &truncate_error(&reason, 4096),
      ))
    }
    Ok(Err(WasmRuntimeError::Forbidden(cap))) => {
      tracing::Span::current()
        .record("trap_reason", format!("forbidden:{cap}"));
      tracing::warn!(plugin = %plugin_id_out, capability = %cap, "plugin capability denied");
      Err(plugin_forbidden(&plugin_id_out, &cap))
    }
    Ok(Err(WasmRuntimeError::Cancelled)) => {
      tracing::Span::current().record("trap_reason", "cancelled");
      tracing::warn!(plugin = %plugin_id_out, tool = %tool, "plugin execution cancelled");
      Err(ra_tools::error::cancelled(&tool)) // 复用 tool.cancelled（不计熔断）
    }
    Ok(Err(WasmRuntimeError::Internal(msg))) => {
      tracing::Span::current().record("trap_reason", msg.as_str());
      tracing::error!(plugin = %plugin_id_out, tool = %tool, reason = %msg, "plugin host internal error");
      Err(plugin_internal("run_tool", msg))
    }
    Err(e) => Err(plugin_internal("join error", e)),
  }
}

/// 调 guest 的 `alloc(size)` 分配一块缓冲区，返回首址。
///
/// **必须检查返回值非 0**（v0.8.1 P0-5）：ABI 约定 alloc 失败返回 0，
/// 不检查会写到 guest 线性内存 0 地址（潜在 UB）。返回 0 / 非 i32 属
/// **guest 侧 ABI 违约**（v0.8.4 P1-2）：归 `GuestError` →
/// `plugin.plugin_exec_failed`（可回灌 LLM），与 §6.4 `alloc_return` 的
/// 错误层严格对齐，**不得**归 `Internal`（会把「guest 内存不足」当宿主
/// 故障、误熔断整个工具面）。
fn guest_alloc(
  store: &mut wasmtime::Store<HostState>,
  alloc: &wasmtime::Func,
  n: usize,
) -> Result<i32, WasmRuntimeError> {
  let n = i32::try_from(n)
    .map_err(|_| WasmRuntimeError::Internal("alloc size overflow".into()))?;
  let mut out = [wasmtime::Val::I32(0)];
  alloc
    .call(&mut *store, &[wasmtime::Val::I32(n)], &mut out)
    .map_err(classify_guest_call_error)?;
  out[0].i32().filter(|p| *p != 0).ok_or_else(|| {
    WasmRuntimeError::GuestError("guest alloc returned 0 or non-i32".into())
  })
}

/// 已分配但尚未释放的 guest 输入缓冲区登记表（RAII 守卫）。
///
/// `execute` 在 alloc 之后、dealloc 之前若 trap（如 guest 内部
/// `unreachable`），`args_ptr` / `name_ptr` 会泄漏在 guest 线性内存里。
/// 本守卫在错误路径上对已登记指针逐一调 guest `dealloc`（v0.8.4 P2-5
/// 必修项；§13.1「中途 trap 不泄漏」防回归测试依赖它）。
pub(crate) struct GuestBufs {
  ptrs: Vec<(i32, i32)>,
}

impl GuestBufs {
  pub(crate) fn new() -> Self {
    Self { ptrs: Vec::new() }
  }

  pub(crate) fn track(&mut self, ptr: i32, len: i32) {
    self.ptrs.push((ptr, len));
  }

  /// 错误路径清理：释放全部已登记缓冲区（忽略单个 dealloc 的失败——
  /// 首要错误已在调用方持有；trap 后 Store 仍可用，dealloc 可执行）。
  pub(crate) fn release_all(
    &self,
    store: &mut wasmtime::Store<HostState>,
    dealloc: &wasmtime::Func,
  ) {
    for (ptr, len) in &self.ptrs {
      let _ = dealloc.call(
        &mut *store,
        &[wasmtime::Val::I32(*ptr), wasmtime::Val::I32(*len)],
        &mut [],
      );
    }
  }
}

/// ABI v0 胶水：alloc → 写 args → 调 execute → 读回 → dealloc
/// （v0.8.3 P2-6 最小可编译骨架 + v0.8.4 P2-5 RAII 守卫）。
///
/// 所有权（§8.2 三分表）：
///
/// - 输入缓冲区（args JSON / 工具名）：host 经 guest `alloc` 分配 →
///   host 写 → guest 读 → **host dealloc**（成功与失败路径均释放）；
/// - `execute` 的结果缓冲区：guest 自己 alloc → guest 写 → host 读 →
///   **host dealloc**；`ptr == 0` 表示「无结果」，**不得** dealloc；
/// - host function 返回值缓冲区：host 经 guest `alloc` → guest 读 →
///   guest dealloc（host 侧 `alloc_return` 不负责释放）。
fn abi_v0_execute(
  store: &mut wasmtime::Store<HostState>,
  instance: &wasmtime::Instance,
  tool: &str,
  args: &serde_json::Value,
) -> Result<serde_json::Value, WasmRuntimeError> {
  // ===== 阶段 1：解析 guest 导出（缺 memory / alloc / dealloc / execute 均为宿主内部错误） =====
  let mem = instance.get_memory(&mut *store, "memory").ok_or_else(|| {
    WasmRuntimeError::Internal("guest memory export missing".into())
  })?;
  let alloc = instance.get_func(&mut *store, "alloc").ok_or_else(|| {
    WasmRuntimeError::Internal("guest alloc export missing".into())
  })?;
  let dealloc = instance.get_func(&mut *store, "dealloc").ok_or_else(|| {
    WasmRuntimeError::Internal("guest dealloc export missing".into())
  })?;
  let execute = instance.get_func(&mut *store, "execute").ok_or_else(|| {
    WasmRuntimeError::Internal("guest execute export missing".into())
  })?;

  // ===== 阶段 2：args JSON 编码（编码失败 / 超限是业务级，归 GuestError，v0.8.4 P1-2） =====
  let args_bytes = serde_json::to_vec(args)
    .map_err(|e| WasmRuntimeError::GuestError(format!("args encode: {e}")))?;
  let args_len = i32::try_from(args_bytes.len())
    .map_err(|_| WasmRuntimeError::GuestError("args too large".into()))?;
  let name_bytes = tool.as_bytes();
  // 工具短名在注册期已经 `validate_tool_name` 校验（≤ 64 字节，
  // `TOOL_NAME_MAX_LEN`），本检查是**防御性**的：防未来校验被绕过 /
  // 上限放宽后遗忘此处。超限属「输入形状违约」→ `GuestError`
  // （`plugin.plugin_exec_failed`，可回灌），与相邻 `args_len` 的分类
  // 一致（v0.8.4 P1-2 跨层语义自洽纪律）；**不得**归 `Internal`
  // ——`plugin.plugin_internal` 计入熔断，会把形状问题当宿主故障、
  // 误熔断整个工具面。
  if name_bytes.len() > crate::adapter::TOOL_NAME_MAX_LEN {
    return Err(WasmRuntimeError::GuestError(format!(
      "tool name too large ({} > {})",
      name_bytes.len(),
      crate::adapter::TOOL_NAME_MAX_LEN
    )));
  }
  // 显式检查后 len ≤ 64，本转换不可达；映射保持 GuestError 与同类
  // 转换一致（防御 usize > i32::MAX 的极端平台差异）
  let name_len = i32::try_from(name_bytes.len())
    .map_err(|_| WasmRuntimeError::GuestError("tool name too large".into()))?;

  // ===== 阶段 3：分配并写入输入缓冲区（错误路径由守卫统一释放） =====
  let mut bufs = GuestBufs::new();
  let result = execute_inner(
    store, &mem, &alloc, &execute, name_bytes, name_len, args_bytes, args_len,
    &mut bufs,
  );
  match result {
    Ok((value, res_ptr, res_len, packed_block)) => {
      // 成功路径：依次释放输入缓冲区、打包块（如有）与（ptr != 0 的）结果载荷
      for (ptr, len) in bufs.ptrs.drain(..).chain(packed_block) {
        dealloc
          .call(
            &mut *store,
            &[wasmtime::Val::I32(ptr), wasmtime::Val::I32(len)],
            &mut [],
          )
          .map_err(classify_guest_call_error)?;
      }
      if res_ptr != 0 && res_len > 0 {
        dealloc
          .call(
            &mut *store,
            &[wasmtime::Val::I32(res_ptr), wasmtime::Val::I32(res_len)],
            &mut [],
          )
          .map_err(classify_guest_call_error)?;
      }
      Ok(value)
    }
    Err(e) => {
      // 错误路径（含 execute 中途 trap）：释放全部已分配输入缓冲区，防泄漏
      bufs.release_all(store, &dealloc);
      Err(e)
    }
  }
}

/// ABI v0 执行结果：`(结果 JSON, 载荷 ptr, 载荷 len, 打包块)`；
/// `ptr == 0` 表示无结果（按 ABI v0 返回 `Null`）；打包块为
/// `Some((block_ptr, 8))` 时（单 i32 返回形态）需一并释放。
type ExecuteOutcome = (serde_json::Value, i32, i32, Option<(i32, i32)>);

/// ABI v0 执行主体（由 [`abi_v0_execute`] 包裹以获得 RAII 清理）。
///
/// 返回载荷与打包块缓冲区的释放责任在调用方（成功路径释放、
/// 错误路径经 `GuestBufs` RAII 守卫释放）。
#[allow(clippy::too_many_arguments)]
fn execute_inner(
  store: &mut wasmtime::Store<HostState>,
  mem: &wasmtime::Memory,
  alloc: &wasmtime::Func,
  execute: &wasmtime::Func,
  name_bytes: &[u8],
  name_len: i32,
  args_bytes: Vec<u8>,
  args_len: i32,
  bufs: &mut GuestBufs,
) -> Result<ExecuteOutcome, WasmRuntimeError> {
  // 分配并写入 args JSON
  let args_ptr = guest_alloc(store, alloc, args_bytes.len())?;
  bufs.track(args_ptr, args_len);
  mem
    .write(&mut *store, args_ptr as usize, &args_bytes)
    .map_err(|e| WasmRuntimeError::Internal(format!("write args: {e}")))?;

  // 分配并写入工具名
  let name_ptr = guest_alloc(store, alloc, name_bytes.len())?;
  bufs.track(name_ptr, name_len);
  mem
    .write(&mut *store, name_ptr as usize, name_bytes)
    .map_err(|e| WasmRuntimeError::Internal(format!("write tool name: {e}")))?;

  // ===== 调 guest execute(name_ptr, name_len, args_ptr, args_len) =====
  // 结果形态二选一（wasmtime 50 / rustc 稳定性约束实测校准）：
  // - **多值 ABI**（手写 WAT guest）：`(res_ptr, res_len)` 双 i32 返回；
  // - **单 i32 返回**（Rust PDK guest，rustc 不支持 extern "C" 多值返回）：
  //   打包指针 → 指向 8 字节块 `{ptr: i32 LE, len: i32 LE}`，块与载荷均
  //   由 guest alloc 分配，host 读完后需分别 dealloc。
  let n_results = execute.ty(&*store).results().len();
  let mut out = vec![wasmtime::Val::I32(0); n_results];
  execute
    .call(
      &mut *store,
      &[
        wasmtime::Val::I32(name_ptr),
        wasmtime::Val::I32(name_len),
        wasmtime::Val::I32(args_ptr),
        wasmtime::Val::I32(args_len),
      ],
      &mut out,
    )
    .map_err(classify_guest_call_error)?;

  // ===== 读回结果（ptr == 0 → 无结果，按 ABI v0 返回 Null） =====
  let (res_ptr, res_len, packed_block) = match n_results {
    2 => (out[0].i32().unwrap_or(0), out[1].i32().unwrap_or(0), None),
    1 => {
      let block = out[0].i32().unwrap_or(0);
      if block == 0 {
        (0, 0, None)
      } else {
        let mut b = [0u8; 8];
        mem.read(&mut *store, block as usize, &mut b).map_err(|e| {
          WasmRuntimeError::Internal(format!("read packed result: {e}"))
        })?;
        let ptr = i32::from_le_bytes(b[0..4].try_into().expect("4 bytes"));
        let len = i32::from_le_bytes(b[4..8].try_into().expect("4 bytes"));
        (ptr, len, Some((block, 8)))
      }
    }
    n => {
      return Err(WasmRuntimeError::Internal(format!(
        "guest execute must return 1 or 2 results, got {n}"
      )));
    }
  };
  let result = if res_ptr == 0 || res_len <= 0 {
    serde_json::Value::Null
  } else {
    let mut buf = vec![0u8; res_len as usize];
    mem
      .read(&mut *store, res_ptr as usize, &mut buf)
      .map_err(|e| WasmRuntimeError::Internal(format!("read result: {e}")))?;
    // guest 返回的非法 JSON 属业务级错误（可回灌 LLM）
    serde_json::from_slice(&buf).map_err(|e| {
      WasmRuntimeError::GuestError(format!("bad result json: {e}"))
    })?
  };
  Ok((result, res_ptr, res_len, packed_block))
}

#[cfg(test)]
mod tests {
  use super::*;

  // ===== 编译期契约断言（§0.4 纪律 2 / 决策 3 / 决策 4） =====

  /// `HostState` 必须 `Send + 'static`（作为 Store data 进入
  /// spawn_blocking 闭包的前提）。
  #[test]
  fn host_state_is_send_and_static() {
    fn assert_send_static<T: Send + 'static>() {}
    assert_send_static::<HostState>();
  }

  /// `HostStateSnapshot` 必须 `Clone + Debug + Send + Sync`（owned 快照
  /// 在 host function 闭包内使用）。
  #[test]
  fn host_state_snapshot_derives() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<HostStateSnapshot>();
    fn assert_clone_debug<T: Clone + core::fmt::Debug>() {}
    assert_clone_debug::<HostStateSnapshot>();
  }

  /// `Linker<HostState>` 必须可跨线程共享（决策 3）：`Arc<Linker>` 跨
  /// execute 复用的前提。若失败退回每次执行构建并标记 Step C。
  #[test]
  fn linker_is_sync() {
    fn assert_sync<S: Sync>() {}
    fn f<T: Send>() {
      assert_sync::<wasmtime::Linker<T>>();
    }
    f::<HostState>();
  }

  /// `StoreLimits` 可用（决策 4）：`runtime` feature 已开启的证明。
  #[test]
  fn store_limits_available() {
    let _ = wasmtime::StoreLimitsBuilder::new();
  }

  // ===== epoch_ticks_for（§三.3 ceil） =====

  /// 场景：tick 换算向上取整——15ms → 2 ticks（20ms ≥ 15ms），
  /// 恰好整除时不多算（10ms → 1 tick）。
  #[test]
  fn epoch_ticks_round_up() {
    assert_eq!(epoch_ticks_for(Duration::from_millis(15)), 2);
    assert_eq!(epoch_ticks_for(Duration::from_millis(10)), 1);
    assert_eq!(epoch_ticks_for(Duration::from_millis(1)), 1);
    assert_eq!(epoch_ticks_for(Duration::from_secs(2)), 200);
    assert_eq!(epoch_ticks_for(Duration::from_millis(0)), 1);
  }

  // ===== trap 载荷剥离与错误分类（§5.2 v0.8.3 三段式 + §13.1 防回归） =====

  /// 场景：`trap_payload` 取第一个 `"wasm trap: "` 之后的载荷；
  /// 无该标记时返回 None。
  #[test]
  fn trap_payload_extracts_first_marker() {
    assert_eq!(
      trap_payload("wasm trap: forbidden:storage"),
      Some("forbidden:storage")
    );
    assert_eq!(
      trap_payload(
        "error while executing at wasm://...\nCaused by:\n  wasm trap: internal:x"
      ),
      Some("internal:x")
    );
    // payload 自身含 "wasm trap: " 时取第一个（v0.8.4 P3-1）
    assert_eq!(
      trap_payload("wasm trap: a wasm trap: b"),
      Some("a wasm trap: b")
    );
    assert_eq!(trap_payload("no marker here"), None);
  }

  /// 场景：结构化前缀精确匹配——`forbidden:` 只取纯能力名；
  /// `cancelled` 必须完全相等；`guest_error:` / `internal:` / `storage_*` 各归其位。
  #[test]
  fn structured_prefixes_are_classified_precisely() {
    let f = |m: &str| classify_structured(m).unwrap();
    assert!(
      matches!(f("wasm trap: forbidden:storage"), WasmRuntimeError::Forbidden(c) if c == "storage")
    );
    assert!(
      matches!(f("wasm trap: forbidden:url_policy"), WasmRuntimeError::Forbidden(c) if c == "url_policy")
    );
    assert!(matches!(
      f("wasm trap: cancelled"),
      WasmRuntimeError::Cancelled
    ));
    assert!(
      matches!(f("wasm trap: guest_error: bad key"), WasmRuntimeError::GuestError(s) if s == "bad key")
    );
    assert!(
      matches!(f("wasm trap: internal:linker"), WasmRuntimeError::Internal(s) if s == "linker")
    );
    assert!(matches!(
      f("wasm trap: storage_full"),
      WasmRuntimeError::GuestError(_)
    ));
    assert!(matches!(
      f("wasm trap: storage_bad_key"),
      WasmRuntimeError::GuestError(_)
    ));
    assert!(matches!(
      f("wasm trap: storage_internal:poison"),
      WasmRuntimeError::Internal(_)
    ));
    // guest 业务文本里的 "cancelled" 不带结构化前缀 → 不误判（§13.1 P1-2 回归）
    assert!(
      classify_structured("wasm trap: operation cancelled by upstream")
        .is_none()
    );
  }

  /// 场景：wasmtime 自身措辞——epoch 到期与内存超限归 Timeout；
  /// `guest memory export missing`（宿主自造 internal: 前缀）归 Internal
  /// 而非 Timeout（§13.1 P1-1 回归：禁止裸 contains("memory")）。
  #[test]
  fn resource_messages_and_internal_prefix_are_classified() {
    let inst = |e: wasmtime::Error| classify_wasmtime_error(e);
    // 宿主自造的 internal: 前缀必须最先判，不被 "memory" 字样击穿
    let err = inst(wasmtime::Error::msg(
      "wasm trap: internal:guest memory export missing",
    ));
    assert!(matches!(err, WasmRuntimeError::Internal(_)), "got {err:?}");

    // epoch / 内存超限措辞候选集合（wasmtime 50 实测措辞 + 旧版候选）→ Timeout
    for msg in [
      "wasm trap: interrupt", // wasmtime 50：epoch interruption
      "epoch deadline exceeded during yield", // 旧版措辞（保留兼容）
      "memory minimum size of 1024 pages exceeds memory limits", // wasmtime 50：StoreLimits
      "MemoryError: out of memory", // 旧版措辞（保留兼容）
    ] {
      let err = inst(wasmtime::Error::msg(msg));
      assert!(
        matches!(err, WasmRuntimeError::Timeout(_)),
        "msg `{msg}` got {err:?}"
      );
    }

    // 未识别 → Internal（保守默认）
    let err = inst(wasmtime::Error::msg("some unknown failure"));
    assert!(matches!(err, WasmRuntimeError::Internal(_)));
  }

  /// 场景：guest 调用期兜底分支为 GuestError（业务级，可回灌 LLM）——
  /// `unreachable` 等 guest 自身 trap 归 `plugin.plugin_exec_failed`
  /// 而非 `plugin.plugin_internal`（§5.3 / §7「trap → exec_failed」）。
  #[test]
  fn guest_call_trap_falls_back_to_guest_error() {
    let err =
      classify_guest_call_error(wasmtime::Error::msg("wasm trap: unreachable"));
    assert!(
      matches!(err, WasmRuntimeError::GuestError(_)),
      "got {err:?}"
    );

    // 但宿主自造的 internal: 前缀在调用期同样优先
    let err = classify_guest_call_error(wasmtime::Error::msg(
      "wasm trap: internal:write args",
    ));
    assert!(matches!(err, WasmRuntimeError::Internal(_)), "got {err:?}");
  }

  /// 场景：`PluginRuntime::global()` 返回进程唯一实例（两次调用同一指针），
  /// 且 ticker 线程存在（进程级后台线程，测试容忍其持续运行）。
  #[test]
  fn plugin_runtime_global_is_singleton() {
    let r1 = PluginRuntime::global().expect("runtime init");
    let r2 = PluginRuntime::global().expect("runtime init again");
    assert!(
      std::ptr::eq(r1, r2),
      "global() must return the same instance"
    );
  }

  /// 场景：结构化 Trap 优先于字符串兜底——guest `unreachable` 产生的
  /// error 链含 `Trap::UnreachableCodeReached`，应直接归 GuestError，
  /// 不依赖 `"wasm trap:"` 文本解析。
  #[test]
  fn structured_trap_classification_precedes_strings() {
    let cfg = PluginRuntimeConfig::default();
    let engine = build_engine(&cfg).expect("engine");
    let wasm = wat::parse_str(r#"(module (func (export "boom") unreachable))"#)
      .expect("valid wat");
    let module = wasmtime::Module::new(&engine, wasm).expect("module");
    let mut store = wasmtime::Store::new(&engine, ());
    // 本测试自建 Engine，但进程 ticker 仍会推进全局 epoch：默认 deadline
    // 为 0 会先于目标 trap 触发 Interrupt，需显式关闭（同 GuestBufs 测试）
    store.set_epoch_deadline(u64::MAX);
    let instance =
      wasmtime::Instance::new(&mut store, &module, &[]).expect("instance");
    let boom = instance
      .get_typed_func::<(), ()>(&mut store, "boom")
      .expect("boom export");
    let err = boom
      .call(&mut store, ())
      .expect_err("unreachable must trap");

    let structured = classify_trap(&err).expect("structured trap in chain");
    assert!(
      matches!(structured, WasmRuntimeError::GuestError(ref r) if r.contains("unreachable")),
      "got {structured:?}"
    );
    assert!(matches!(
      classify_guest_call_error(err),
      WasmRuntimeError::GuestError(_)
    ));
    // 纯字符串错误（无 Trap 载荷）仍走兜底，行为不变
    assert!(matches!(
      classify_guest_call_error(wasmtime::Error::msg("wasm trap: unreachable")),
      WasmRuntimeError::GuestError(_)
    ));
  }

  /// 场景：fuel 耗尽是结构化 `Trap::OutOfFuel`，归 Timeout（资源限制），
  /// 与 epoch `Trap::Interrupt` 同语义；不因消息文本而误判。
  #[test]
  fn fuel_exhaustion_classifies_as_timeout() {
    let cfg = PluginRuntimeConfig {
      enable_fuel: true,
      ..PluginRuntimeConfig::default()
    };
    let engine = build_engine(&cfg).expect("fuel engine");
    let wasm = wat::parse_str(r#"(module (func (export "spin") (loop br 0)))"#)
      .expect("valid wat");
    let module = wasmtime::Module::new(&engine, wasm).expect("module");
    let mut store = wasmtime::Store::new(&engine, ());
    store.set_epoch_deadline(u64::MAX); // 隔离 ticker 干扰，只验 fuel trap
    store.set_fuel(64).expect("set fuel");
    let instance =
      wasmtime::Instance::new(&mut store, &module, &[]).expect("instance");
    let spin = instance
      .get_typed_func::<(), ()>(&mut store, "spin")
      .expect("spin export");
    let err = spin.call(&mut store, ()).expect_err("fuel must run out");

    let classified = classify_trap(&err).expect("structured fuel trap");
    assert!(
      matches!(classified, WasmRuntimeError::Timeout(ref r) if r.contains("fuel")),
      "got {classified:?}"
    );
    assert!(matches!(
      classify_wasmtime_error(err),
      WasmRuntimeError::Timeout(_)
    ));
  }

  /// 场景：ModuleCache 容量上限 + LRU 淘汰——容量 2 下访问 m1 后
  /// m2 成为最久未使用，写入 m3 淘汰 m2；再次请求 m2 触发重编译。
  #[test]
  fn module_cache_evicts_least_recently_used() {
    let engine = build_engine(&PluginRuntimeConfig::default()).expect("engine");
    let cache = ModuleCache::new(2);
    let compile = |pages: u8| {
      let wasm = wat::parse_str(format!(
        r#"(module (memory (export "memory") {pages}))"#
      ))
      .expect("valid wat");
      cache.get_or_compile(&engine, &wasm).expect("compile")
    };
    let m1 = compile(1);
    let m2 = compile(2);
    // touch m1 → m2 成为 LRU
    let m1_again = compile(1);
    assert!(Arc::ptr_eq(&m1, &m1_again));
    let _m3 = compile(3);
    assert_eq!(cache.len(), 2, "must never exceed configured capacity");
    let m2_again = compile(2);
    assert!(
      !Arc::ptr_eq(&m2, &m2_again),
      "LRU entry must have been evicted and recompiled"
    );
  }

  /// 场景：ModuleCache 内容哈希命中——同一字节命中同一 `Arc<Module>`，
  /// 内容变化（不同模块）则各自编译（§13.1 P1-2）。
  #[test]
  fn module_cache_hits_by_content_hash() {
    let runtime = PluginRuntime::global().expect("runtime init");
    let wasm1 =
      wat::parse_str(r#"(module (memory (export "memory") 1))"#).unwrap();
    let m1 = runtime
      .module_cache
      .get_or_compile(&runtime.engine, &wasm1)
      .expect("compile 1");
    let m1_again = runtime
      .module_cache
      .get_or_compile(&runtime.engine, &wasm1)
      .expect("compile 1 again");
    assert!(
      Arc::ptr_eq(&m1, &m1_again),
      "same bytes must hit the same Module"
    );

    // 内容不同 → 缓存未命中 → 新 Module
    let wasm2 =
      wat::parse_str(r#"(module (memory (export "memory") 2))"#).unwrap();
    let m2 = runtime
      .module_cache
      .get_or_compile(&runtime.engine, &wasm2)
      .expect("compile 2");
    assert!(
      !Arc::ptr_eq(&m1, &m2),
      "different bytes must not share Module"
    );
  }

  /// 场景：持锁线程 panic 致 module cache 的 Mutex 进入 poison 状态后
  /// 调用 `get_or_compile`。
  ///
  /// 预期：**返回 Err 而非 panic**。panic 在 spawn_blocking / 宿主线程
  /// 中会升级为进程级故障（甚至 double panic → abort），阻断优雅关闭；
  /// 错误传播则可由上层 `plugin_internal("module compile", e)` 正常处理。
  /// 同时 `Debug` / `len` 不得 panic（panic 展开期间调用 Debug 会 abort）。
  #[test]
  fn poisoned_module_cache_propagates_error_without_panicking() {
    let engine = build_engine(&PluginRuntimeConfig::default()).expect("engine");
    let cache = ModuleCache::new(2);
    // 制造 poison：在 scoped 线程中持锁 panic，catch_unwind 只在线程内
    // 吸收 panic，Mutex 的 poison 标记仍会被设置
    std::thread::scope(|scope| {
      scope.spawn(|| {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
          let _guard = cache.entries.lock().expect("lock");
          panic!("simulate panic inside critical section");
        }));
      });
    });
    assert!(
      cache.entries.is_poisoned(),
      "setup must poison the cache mutex"
    );

    // 这些诊断入口在 poison 状态下也不得 panic
    let _ = format!("{cache:?}");
    let _ = cache.len();

    let wasm =
      wat::parse_str(r#"(module (memory (export "memory") 1))"#).expect("wat");
    let err = cache
      .get_or_compile(&engine, &wasm)
      .expect_err("poison must propagate as Err, not panic");
    assert!(
      err.to_string().contains("poison"),
      "error should describe the poisoned mutex, got: {err}"
    );
  }
}
