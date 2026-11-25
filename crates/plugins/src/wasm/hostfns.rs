//! Host Functions：log / storage_get / storage_put / http_proxy 与
//! `Linker` 构建。
//!
//! 能力按 ACL 注入（guest 默认无 IO）：`log` 走限流、`storage_*` 走
//! 插件级进程内存储、`http_proxy` 走受控代理（UrlPolicy + 禁重定向 +
//! 响应体上限）。所有 guest 可控长度在**读取点先判后分配**
//! （`StoreLimits` 只约束 guest 线性内存，管不到 host 堆上的 Vec）。
//!
//! 错误链契约：host function 内部错误 → **结构化前缀 trap**
//! （`internal:` / `guest_error:` / `forbidden:{cap}` / `cancelled` /
//! `storage_*`）→ `run_tool` 的 [`classify_wasmtime_error`](crate::wasm::runtime)
//! 按前缀分类。ACL 拒绝**必须直接构造** `WasmError::msg("forbidden:{cap}")`
//! （规范能力名），绝不走 [`ribo_to_error`]。

use std::sync::Arc;
use std::sync::{
  OnceLock,
  atomic::{AtomicU64, Ordering},
};
use std::time::Duration;

use ra_core::error::{RiboError, RiboResult};
use ra_tools::context::ToolContext;
use wasmtime::{Caller, Error as WasmError};

// wasmtime 50 API 校准（依赖锁定时实测）
// `wasmtime::Trap` 在 50.x 已是 #[non_exhaustive] 的预定义 enum
// （无 `Trap::new`），host function 的错误类型统一为 `wasmtime::Error`
// （`WasmRet for Result<T>` 以 wasmtime::Result 为准）。自定义结构化
// 前缀消息经 `WasmError::msg(...)` 承载——host function 返回的
// Err 会**原样**从 `Func::call` 透出（无 "wasm trap: " 前缀），
// `run_tool` 的分类器同时兼容「裸前缀」与「guest trap 包裹」两种形态。

use crate::config::PluginRuntimeConfig;
use crate::error::{
  PLUGIN_INTERNAL, plugin_internal, plugin_invalid_arguments,
};
use crate::wasm::runtime::{HostState, HostStateSnapshot};
use crate::wasm::storage::{MAX_KEY_LEN, MAX_VALUE_LEN, StorageError};

// guest 可控输入长度上限（DoS 边界）
// 四条纪律：① 每个 guest 可控长度参数都必须有上限；② 上限判定必须在
// host 侧；③ 先判后分配；④ 越界 → plugin_invalid_arguments →
// guest_error: → plugin.plugin_exec_failed（可回灌的业务错误）。
// storage 语义的 MAX_KEY_LEN / MAX_VALUE_LEN 定义在 storage.rs，
// 本文件定义 HTTP / 日志相关常量；ABI 表是四处上限的契约汇总。

/// http 请求 JSON 长度上限。
pub const MAX_REQ_LEN: usize = 64 * 1024;
/// 日志文本长度上限。
pub const MAX_LOG_LEN: usize = 16 * 1024;

/// 本 crate 私有的单调毫秒函数（限流窗口基准）。
///
/// **不要用 `ra_core::utils::time::now_ms()`**：该函数返回 i64 UTC 时间戳
/// （wall-clock 可能被 NTP / 手动校时回拨，导致限流窗口错乱）；
/// 限流窗口必须用单调时钟。毫秒精度仅作窗口起点，不需纳秒。
pub(crate) fn now_ms() -> u64 {
  static BASE: OnceLock<std::time::Instant> = OnceLock::new();
  BASE
    .get_or_init(std::time::Instant::now)
    .elapsed()
    .as_millis() as u64
}

/// 插件级日志限流器（Send + Sync，作为 `Arc` 放进 HostState）。
///
/// 每个 [`Storage`](crate::wasm::storage::Storage) / 限流器即单插件实例
/// （挂 `Arc<PluginShared>`），内部无需按 plugin_id 分表。
///
/// 实现要点：
///
/// ① 窗口滚动用 CAS（`try_update`，原 `fetch_update`），不用「load → 比较 → store」；
/// ② 计数用 `fetch_add`；**窗口一旦滚动，必须同步把 `count` 归零**
///    ——否则第二窗口起 `count` 永远 ≥ burst，限流永久失效；
/// ③ burst 内直接放行，超出后按 `rate_per_sec` 采样放行；
/// ④ 超配额**不返回错误**（调用方直接丢弃，避免插件因日志被限而报错）；
/// ⑤ 允许丢弃一个窗口的余量——限流的目的是防洪水而非精确 QoS。
#[derive(Debug)]
pub struct LogRateLimiter {
  count: AtomicU64,
  window_start: AtomicU64,
  window: Duration,
  burst: u64,
  rate_per_sec: u64,
}

impl LogRateLimiter {
  /// 以进程配置构造（burst / rate 注入，不再硬编码 200 条 / 50 per s）。
  pub fn new(cfg: &PluginRuntimeConfig) -> Self {
    Self {
      count: AtomicU64::new(0),
      window_start: AtomicU64::new(now_ms()),
      window: Duration::from_secs(1),
      burst: cfg.log_burst,
      rate_per_sec: cfg.log_rate_per_sec,
    }
  }

  /// 判定本条日志是否放行：窗口内计数 < burst 直接放行；超出后按
  /// `rate_per_sec` 采样。窗口滚动成功者负责把 count 清零：**滚动必须重置计数**，否则第二窗口起限流永久失效。
  pub fn allow(&self) -> bool {
    let window_ms = self.window.as_millis() as u64;
    let now = now_ms();
    // ① 窗口滚动（CAS：仅当窗口确实过期才前进）；只有滚动成功的线程
    //    负责把 count 清零（两步间竞态可接受，限流非热路径）
    let rolled = self
      .window_start
      .try_update(Ordering::Relaxed, Ordering::Relaxed, |start| {
        if now.saturating_sub(start) >= window_ms {
          Some(now)
        } else {
          None
        }
      })
      .is_ok();
    if rolled {
      self.count.store(0, Ordering::Relaxed);
    }
    // ② 计数自增
    let count = self.count.fetch_add(1, Ordering::Relaxed);
    // ③ burst 内直接放行
    if count < self.burst {
      return true;
    }
    // ④ 超出 burst 后按 rate_per_sec 采样放行
    self.rate_per_sec > 0 && count.is_multiple_of(self.rate_per_sec)
  }
}

// http-proxy（sync/async 桥接 + SSRF 三层的前两层）

/// host function 内部私有错误（对外经 trap 前缀表映射，不引入第二套
/// 对外错误类型）。`Display` 供 `guest_error: {e}` 前缀拼接使用。
#[derive(Debug, thiserror::Error)]
pub(crate) enum HttpProxyError {
  /// ACL 未授权 → `WasmRuntimeError::Forbidden({capability})`。
  #[error("denied capability {0}")]
  Forbidden(String),
  /// UrlPolicy 拒绝 → `Forbidden("url_policy")`。
  #[error("url policy rejected: {0}")]
  Policy(String),
  /// URL 解析失败 → GuestError（业务级）。
  #[error("bad url: {0}")]
  BadUrl(String),
  /// HTTP 方法非法 → GuestError。
  #[error("bad method: {0}")]
  BadMethod(String),
  /// 请求执行失败 → GuestError。
  #[error("request failed: {0}")]
  Request(String),
  /// 取消 → `tool.cancelled`。
  #[error("cancelled")]
  Cancelled,
  /// 响应体超 [`MAX_RESP_BYTES`] → GuestError（可回灌 LLM 改小请求）。
  #[error("response too large ({0} bytes)")]
  ResponseTooLarge(usize),
}

/// http 响应体大小上限（4MB，DoS 边界）。
///
/// `StoreLimits` 只约束 guest 线性内存，不约束 host 堆上的 Vec——
/// 异常服务端返回超大响应即可打爆 host。**即使只面向受控 URL，
/// 内网里的异常服务同样能打垮 host**，故该上限必须存在。
pub const MAX_RESP_BYTES: usize = 4 * 1024 * 1024;

// `host_http_get` 内 `?` 需要 url::ParseError / reqwest::Error
// 的 From 实现，缺失编译不过。reqwest 0.13 不再 re-export `http` 模块，
// `From<reqwest::http::method::InvalidMethod>` 无法成立——
// `Method::from_bytes` 的错误在调用点以 `map_err` 就地映射为
// `HttpProxyError::BadMethod`（不引入 http 直接依赖）。
impl From<url::ParseError> for HttpProxyError {
  fn from(e: url::ParseError) -> Self {
    HttpProxyError::BadUrl(e.to_string())
  }
}

impl From<reqwest::Error> for HttpProxyError {
  fn from(e: reqwest::Error) -> Self {
    HttpProxyError::Request(e.to_string())
  }
}

/// 受限的出站 HTTP 请求（sync host function 内经 `block_on` 桥接 async）。
///
/// 先同步做 ACL + UrlPolicy 校验，再 `block_on` 发送请求
/// 与取消竞争（`tokio::select!`），响应体分块读取并累计上限
/// （`Content-Length` 不可信，只信预检等于没设防）。
///
/// **调用约束**：本函数依赖
/// `tokio::runtime::Handle::current()`——仅在 `run_tool` 的
/// `spawn_blocking` 路径（runtime 上下文已注入）或 `#[tokio::test]`
/// 内调用；无 runtime 时 `Handle::current()` 会 panic。
pub(crate) fn host_http_get(
  state: &HostStateSnapshot,
  url: &str,
  method: &str,
  body: Option<Vec<u8>>,
) -> Result<Vec<u8>, HttpProxyError> {
  // ACL + SSRF 校验（同步）
  if !state.acl.http {
    return Err(HttpProxyError::Forbidden("http".into()));
  }
  let parsed = reqwest::Url::parse(url)?;
  state
    .acl
    .url_policy
    .as_ref()
    .ok_or(HttpProxyError::Policy("url policy missing".into()))?
    .check_url(&parsed)
    .map_err(|e| HttpProxyError::Policy(e.to_string()))?;

  // sync host function 内桥接 async
  tokio::runtime::Handle::current().block_on(async {
    // Method::from_bytes 的错误类型（InvalidMethod）不经 From 自动转换：
    // 就地 map_err（reqwest 0.13 不再 re-export `http` 模块）
    let method = reqwest::Method::from_bytes(method.as_bytes())
      .map_err(|e| HttpProxyError::BadMethod(e.to_string()))?;
    let base = state.client.request(method, parsed.clone());
    let req = if let Some(b) = body {
      base.body(b)
    } else {
      base
    };

    tokio::select! {
      res = req.send() => {
        let mut res = res?;
        // ① Content-Length 预检（不可信，仅用于快速失败）
        if let Some(len) = res.content_length().filter(|&l| l > MAX_RESP_BYTES as u64) {
          return Err(HttpProxyError::ResponseTooLarge(len as usize));
        }
        // ② 分块读取 + 累计（防 Content-Length 撒谎 / chunked / 无长度头）
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = res
          .chunk()
          .await
          .map_err(|e| HttpProxyError::Request(e.to_string()))?
        {
          if buf.len() + chunk.len() > MAX_RESP_BYTES {
            return Err(HttpProxyError::ResponseTooLarge(MAX_RESP_BYTES));
          }
          buf.extend_from_slice(&chunk);
        }
        Ok(buf)
      }
      _ = state.signal.cancelled() => Err(HttpProxyError::Cancelled),
    }
  })
}

/// 构建受限 HTTP 客户端：禁用重定向（SSRF 第 2 层）+ DNS 解析结果
/// IP 复检（SSRF 第 3 层）。
///
/// # Errors
///
/// 客户端构建失败 → `plugin.plugin_internal`（不引入第二套对外错误类型）。
pub fn build_http_client() -> RiboResult<reqwest::Client> {
  // 生产路径：系统解析外包 DNS 守卫（SSRF 第 3 层）
  build_http_client_with_resolver(Arc::new(DnsGuardResolver::new(Arc::new(
    SystemResolver,
  ))))
}

/// 显式注入解析器的构造：生产路径走 [`SystemResolver`]；测试用静态
/// 解析表复现「白名单域名解析到私网 IP」的 DNS rebinding 场景。
///
/// # Errors
///
/// 同 [`build_http_client`]。
pub(crate) fn build_http_client_with_resolver(
  resolver: Arc<dyn reqwest::dns::Resolve>,
) -> RiboResult<reqwest::Client> {
  reqwest::Client::builder()
    .redirect(reqwest::redirect::Policy::none())
    // 强制直连（不走系统代理）：SSRF 第 3 层依赖本地解析并复检 IP——
    // 走代理时解析发生在代理侧，DNS 守卫与保留段拦截全部被旁路。
    // 插件通道的出站策略完全由宿主白名单承担，不继承进程级代理。
    .no_proxy()
    .dns_resolver(resolver)
    .build()
    .map_err(|e| plugin_internal("http client build", e))
}

/// 系统默认解析（getaddrinfo），与 reqwest 无自定义 resolver 时的行为
/// 等价——包一层以便 [`DnsGuardResolver`] 统一持有 `Arc<dyn Resolve>`。
struct SystemResolver;

impl reqwest::dns::Resolve for SystemResolver {
  fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
    let host = name.as_str().to_string();
    Box::pin(async move {
      use std::net::ToSocketAddrs;
      // port 0 占位：reqwest 以连接目标真实端口替换地址端口，仅取 IP
      let addrs: Vec<std::net::SocketAddr> =
        (host.as_str(), 0).to_socket_addrs()?.collect();
      Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
    })
  }
}

/// SSRF 第 3 层：DNS 解析结果 IP 复检守卫。
///
/// [`UrlPolicy`] 检查的是 URL 字符串的 host，防不了「白名单域名解析到
/// 私网 IP」的 DNS rebinding——本守卫在解析结果上复用
/// [`crate::acl::is_blocked_ip`]，命中保留段即拒绝连接（错误进入
/// reqwest 的 transport 路径 → `plugin.plugin_exec_failed`）。
pub(crate) struct DnsGuardResolver {
  inner: Arc<dyn reqwest::dns::Resolve>,
}

impl DnsGuardResolver {
  /// 以显式内层解析器构造守卫（生产 = 系统解析；测试 = 静态解析表）。
  pub(crate) fn new(inner: Arc<dyn reqwest::dns::Resolve>) -> Self {
    Self { inner }
  }
}

/// 解析结果 IP 复检（纯函数，便于单测）：命中保留段即拒绝，错误文案
/// 含 `blocked address` 与具体 IP（供诊断与断言）。
pub(crate) fn ensure_addrs_allowed(
  addrs: impl Iterator<Item = std::net::SocketAddr>,
) -> std::io::Result<Vec<std::net::SocketAddr>> {
  let mut allowed = Vec::new();
  for addr in addrs {
    if crate::acl::is_blocked_ip(addr.ip()) {
      return Err(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        format!("dns resolved to blocked address: {}", addr.ip()),
      ));
    }
    allowed.push(addr);
  }
  Ok(allowed)
}

impl reqwest::dns::Resolve for DnsGuardResolver {
  fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
    let inner = Arc::clone(&self.inner);
    Box::pin(async move {
      let addrs = inner.resolve(name).await?;
      let allowed = ensure_addrs_allowed(addrs)?;
      Ok(Box::new(allowed.into_iter()) as reqwest::dns::Addrs)
    })
  }
}

// guest 内存读写辅助

/// 把 host function 内部的 `RiboError` 转成**带结构化前缀**的 Trap。
///
/// **硬约束**：本函数**绝不产生 `forbidden:` 前缀**——
/// 所有 ACL 拒绝路径必须直接构造 `WasmError::msg("forbidden:{capability}")`
/// （规范能力名）。本函数只产 `internal:{stage}`（计入熔断）与
/// `guest_error:{reason}`（其余一律按业务错误收敛，含
/// `plugin.plugin_invalid_arguments` → `plugin.plugin_exec_failed`）。
/// 禁止 `WasmError::msg(e.to_string())`——`RiboError` 的 Display 无结构化
/// 前缀，会落入未识别分支被误判 `Internal` 并误计熔断。
fn ribo_to_error(e: &RiboError) -> WasmError {
  if e.code() == PLUGIN_INTERNAL {
    WasmError::msg(format!("internal:{}", e.message()))
  } else {
    WasmError::msg(format!("guest_error: {e}"))
  }
}

/// 从 guest 线性内存按 (ptr, len) 读原始字节（越界由 wasmtime 转 Trap）。
///
/// guest 必须显式 `(export "memory" (memory 0))`，否则
/// `caller.get_export("memory")` 返回 None → `plugin.plugin_internal`。
///
/// **签名必须收 `&mut Caller`**：wasmtime 的
/// `Caller::get_export` 需 `&mut self`；`&Caller` 编译不过（E0596）。
fn read_guest_bytes(
  caller: &mut Caller<HostState>,
  ptr: i32,
  len: i32,
) -> RiboResult<Vec<u8>> {
  let ptr =
    usize::try_from(ptr).map_err(|_| plugin_invalid_arguments("ptr<0"))?;
  let len =
    usize::try_from(len).map_err(|_| plugin_invalid_arguments("len<0"))?;
  let mem = caller
    .get_export("memory")
    .and_then(|e| e.into_memory())
    .ok_or_else(|| plugin_internal("guest memory export missing", ""))?;
  // 长度在读取点已由 read_guest_bytes_limited 限制；此处直接分配
  let mut buf = vec![0u8; len];
  mem
    .read(&*caller, ptr, &mut buf)
    .map_err(|e| plugin_internal("read guest", e))?;
  Ok(buf)
}

/// **带长度上限**的 guest 内存读取（先判后分配，DoS 边界）。
///
/// `read_guest_bytes` 里的 `vec![0u8; len]` 发生在 host 堆上——恶意
/// guest 传巨大 `len` 即可在不触碰 guest 沙箱内存上限的前提下打爆
/// host。故长度判定必须在分配**之前**、host function 的读取点完成。
/// 越界是「guest 传参违规」→ `plugin_invalid_arguments` →
/// `guest_error:` → `plugin.plugin_exec_failed`（可回灌 LLM 缩小入参）。
fn read_guest_bytes_limited(
  caller: &mut Caller<HostState>,
  ptr: i32,
  len: i32,
  max: usize,
  what: &'static str,
) -> RiboResult<Vec<u8>> {
  let len_u =
    usize::try_from(len).map_err(|_| plugin_invalid_arguments("len<0"))?;
  if len_u > max {
    return Err(plugin_invalid_arguments(&format!(
      "{what} exceeds limit ({len_u} > {max})"
    )));
  }
  read_guest_bytes(caller, ptr, len)
}

/// 经 guest 导出的 `alloc(len)` 申请缓冲区并写入 bytes，返回 (ptr, len)。
///
/// **边界契约**：本函数不接受空 bytes——`alloc(0)` 的
/// 行为在不同引擎/版本下不一致，需要表达「没有结果」的场景一律走
/// `Ok(0)` 哨兵（返回语义表），绝不把 `&[]` 传进来。
/// **释放责任**：本函数只负责「申请 + 写入 + 返回首址」，不负责释放——
/// 返回值缓冲区由 guest 读完后 `dealloc`（三分表）。
/// **alloc 返回 0 / 非 i32 视为 guest 分配失败**（先判 0
/// 再 write，否则写到 guest 线性内存 0 地址属潜在 UB）。
fn alloc_return(
  caller: &mut Caller<HostState>,
  bytes: &[u8],
) -> RiboResult<(i32, i32)> {
  if bytes.is_empty() {
    return Err(plugin_invalid_arguments(
      "alloc_return called with empty payload",
    ));
  }
  let len = i32::try_from(bytes.len())
    .map_err(|_| plugin_invalid_arguments("len>2GB"))?;
  let alloc = caller
    .get_export("alloc")
    .and_then(|e| e.into_func())
    .ok_or_else(|| plugin_internal("guest alloc export missing", ""))?;
  let mut out = [wasmtime::Val::I32(0)];
  alloc
    .call(&mut *caller, &[wasmtime::Val::I32(len)], &mut out)
    .map_err(|e| plugin_internal("alloc call", e))?;
  let ptr = out[0].i32().filter(|p| *p != 0).ok_or_else(|| {
    plugin_invalid_arguments("guest alloc returned 0 or non-i32")
  })?;
  let mem = caller
    .get_export("memory")
    .and_then(|e| e.into_memory())
    .ok_or_else(|| plugin_internal("guest memory export missing", ""))?;
  mem
    .write(&mut *caller, ptr as usize, bytes)
    .map_err(|e| plugin_internal("write guest", e))?;
  Ok((ptr, len))
}

/// `http_proxy` 的请求 JSON（guest 经 `ribo.http_proxy(ptr,len)` 传入）。
///
/// 文本 body 直接走 UTF-8；二进制 body 再上 base64。
#[derive(serde::Deserialize)]
struct HttpRequest {
  url: String,
  method: String,
  #[serde(default)]
  body: Option<String>,
}

/// 构建 `Linker`：注册 4 个 host import，与 PDK 声明一一对应。
///
/// 返回值语义按 「host import 返回语义表」：
///
/// | import | 入参 | 返回 |
/// | :-- | :-- | :-- |
/// | `ribo.log` | `(ptr, len)` ≤ 16KB | `0` = 成功且无结果 |
/// | `ribo.storage_get` | `(key_ptr, key_len)` ≤ 256B | `ptr`；**`0` = key 不存在** |
/// | `ribo.storage_put` | `(key_ptr, key_len, val_ptr, val_len)` | `0` = 成功且无结果 |
/// | `ribo.http_proxy` | `(ptr, len)` ≤ 64KB | `ptr` = 响应缓冲区首址 |
///
/// 由 `WasmPluginHost::new()` 调用一次并以 `Arc` 缓存复用；
/// `Linker::instantiate(&self, ...)` 为只读方法，可跨并发 Store 调用。
///
/// **四个闭包参数一律保留 `mut caller`**（冲突裁决）：
/// `read_guest_bytes_limited` 需 `&mut Caller`（`get_export` 要
/// `&mut self`）；若 clippy 报 unused_mut，先确认该闭包确实未触达
/// `&mut caller` 再删。
///
/// # Errors
///
/// import 签名注册失败 → `plugin.plugin_internal`（不引 anyhow）。
pub fn build_linker(
  engine: &wasmtime::Engine,
) -> RiboResult<wasmtime::Linker<HostState>> {
  let mut linker = wasmtime::Linker::new(engine);

  // —— `log`：读 (ptr,len) → UTF-8 → 限流 → tracing::debug!（英文） ——
  linker.func_wrap(
    "ribo",
    "log",
    |mut caller: Caller<HostState>, ptr: i32, len: i32| -> wasmtime::Result<i32> {
      let snap = caller.data().snapshot(); // owned 快照，借用即结束
      if !snap.acl.log {
        // ACL 拒绝必须直接构造规范能力名 trap（硬约束）
        return Err(WasmError::msg("forbidden:log"));
      }
      // 先判后分配：日志文本 ≤ 16KB
      let bytes = read_guest_bytes_limited(&mut caller, ptr, len, MAX_LOG_LEN, "log payload")
        .map_err(|e| ribo_to_error(&e))?;
      let msg = String::from_utf8(bytes)
        .map_err(|_| WasmError::msg("guest_error: log invalid utf8"))?;
      // 限流：超配额直接丢弃，不报错（避免放大噪声）
      if snap.log_limiter.allow() {
        tracing::debug!(plugin = %snap.plugin_id, tool = %snap.tool_name, "{msg}");
      }
      Ok(0) // 0 = 成功且无结果（guest 不得把 0 当结果指针读）
    },
  )
  .map_err(|e| plugin_internal("linker:log", e))?;

  // —— `storage_get`：读 key → 查 Arc<Storage> → 三态返回 ——
  linker.func_wrap(
    "ribo",
    "storage_get",
    |mut caller: Caller<HostState>, key_ptr: i32, key_len: i32| -> wasmtime::Result<i32> {
      let snap = caller.data().snapshot();
      // 先判后分配：key ≤ 256B
      let key = read_guest_bytes_limited(&mut caller, key_ptr, key_len, MAX_KEY_LEN, "storage key")
        .and_then(|b| {
          String::from_utf8(b).map_err(|_| plugin_invalid_arguments("bad key utf8"))
        })
        .map_err(|e| ribo_to_error(&e))?;
      let value = match snap.storage.get(&key) {
        Ok(Some(v)) => v,
        // key 不存在是正常态：直接返回 0 哨兵（不走 alloc_return——
        // 其已把 alloc 返回 0 视为 guest 分配失败并拒绝写入）
        Ok(None) => return Ok(0),
        Err(StorageError::Denied) => return Err(WasmError::msg("forbidden:storage")),
        // Full 对 get 而言是防御性分支（明确 Full 仅 put 触发），
        // 保留并映射到正确的 storage_full（不与 BadKey 混淆）
        Err(StorageError::Full) => return Err(WasmError::msg("storage_full")),
        Err(StorageError::BadKey(reason)) => {
          tracing::debug!(plugin = %snap.plugin_id, reason = %reason, "storage key rejected");
          return Err(WasmError::msg("storage_bad_key"));
        }
        Err(StorageError::Internal(s)) => {
          return Err(WasmError::msg(format!("storage_internal:{s}")));
        }
      };
      // caller 的不可变借用已随 snap 结束，此处 &mut 安全
      // 帧化返回（PDK 跨层契约）：host function 的 import 只能返回单
      // i32（rustc 不支持多值返回），无法同时携带 ptr 与 len——故写入
      // `[len: u32 LE][value]` 帧化块，guest 读首 4 字节取长度后读载荷，
      // 用完 dealloc 整块。
      let mut framed = Vec::with_capacity(4 + value.len());
      framed.extend_from_slice(&(value.len() as u32).to_le_bytes());
      framed.extend_from_slice(&value);
      match alloc_return(&mut caller, &framed) {
        Ok((p, _)) => Ok(p),
        Err(e) => Err(ribo_to_error(&e)),
      }
    },
  )
  .map_err(|e| plugin_internal("linker:storage_get", e))?;

  // —— `storage_put`：写 key/value ——
  linker
    .func_wrap(
      "ribo",
      "storage_put",
      |mut caller: Caller<HostState>,
       key_ptr: i32,
       key_len: i32,
       val_ptr: i32,
       val_len: i32|
       -> wasmtime::Result<i32> {
        let snap = caller.data().snapshot();
        // 先判后分配：key ≤ 256B、value ≤ 256KB
        let key = read_guest_bytes_limited(
          &mut caller,
          key_ptr,
          key_len,
          MAX_KEY_LEN,
          "storage key",
        )
        .and_then(|b| {
          String::from_utf8(b)
            .map_err(|_| plugin_invalid_arguments("bad key utf8"))
        })
        .map_err(|e| ribo_to_error(&e))?;
        let value = read_guest_bytes_limited(
          &mut caller,
          val_ptr,
          val_len,
          MAX_VALUE_LEN,
          "storage value",
        )
        .map_err(|e| ribo_to_error(&e))?;
        match snap.storage.put(&key, &value) {
          Ok(()) => Ok(0), // 0 = 成功且无结果
          Err(StorageError::Full) => Err(WasmError::msg("storage_full")),
          Err(StorageError::Denied) => Err(WasmError::msg("forbidden:storage")),
          Err(StorageError::BadKey(_)) => {
            Err(WasmError::msg("storage_bad_key"))
          }
          Err(StorageError::Internal(s)) => {
            Err(WasmError::msg(format!("storage_internal:{s}")))
          }
        }
      },
    )
    .map_err(|e| plugin_internal("linker:storage_put", e))?;

  // —— `http_proxy`：复用 host_http_get（ACL + SSRF + 响应体上限 + block_on 桥接） ——
  linker
    .func_wrap(
      "ribo",
      "http_proxy",
      |mut caller: Caller<HostState>,
       ptr: i32,
       len: i32|
       -> wasmtime::Result<i32> {
        let snap = caller.data().snapshot(); // owned 快照
        // 先判后分配：请求 JSON ≤ 64KB
        let req_bytes = read_guest_bytes_limited(
          &mut caller,
          ptr,
          len,
          MAX_REQ_LEN,
          "http request",
        )
        .map_err(|e| ribo_to_error(&e))?;
        let req: HttpRequest = serde_json::from_slice(&req_bytes)
          .map_err(|_| WasmError::msg("guest_error: bad http request json"))?;
        let body = req.body.as_deref().map(|s| s.as_bytes().to_vec());
        // 用快照（owned）调用，不持 caller 的不可变借用
        let resp = match host_http_get(&snap, &req.url, &req.method, body) {
          Ok(b) => b,
          Err(HttpProxyError::Forbidden(c)) => {
            return Err(WasmError::msg(format!("forbidden:{c}")));
          }
          Err(HttpProxyError::Policy(_)) => {
            return Err(WasmError::msg("forbidden:url_policy"));
          }
          Err(HttpProxyError::Cancelled) => {
            return Err(WasmError::msg("cancelled"));
          }
          // 响应超限 = 业务错误（guest 可改小请求重试）
          Err(HttpProxyError::ResponseTooLarge(n)) => {
            return Err(WasmError::msg(format!(
              "guest_error: response too large ({n} bytes)"
            )));
          }
          Err(e) => return Err(WasmError::msg(format!("guest_error: {e}"))),
        };
        // 此处 &mut caller 与上面的 &snap 无重叠 → 编译通过
        // 帧化返回（PDK 跨层契约，同 storage_get）：单 i32 import 返回无法
        // 携带长度，写入 `[len: u32 LE][body]`，guest 读首 4 字节取长度。
        let mut framed = Vec::with_capacity(4 + resp.len());
        framed.extend_from_slice(&(resp.len() as u32).to_le_bytes());
        framed.extend_from_slice(&resp);
        match alloc_return(&mut caller, &framed) {
          Ok((p, _)) => Ok(p),
          Err(e) => Err(ribo_to_error(&e)),
        }
      },
    )
    .map_err(|e| plugin_internal("linker:http_proxy", e))?;

  Ok(linker)
}

/// 借 [`ToolContext`] 声明 hostfns 对取消语义的依赖（占位说明）。
///
/// 实际取消传播经 `HostState.signal`（`ctx.signal().clone()` 传入），
/// 本常量仅供文档锚定，无运行时语义。
#[allow(dead_code)]
const _CANCEL_VIA_TOOL_CONTEXT: fn(&ToolContext) = |ctx| {
  let _ = ctx.signal().clone();
};

#[cfg(test)]
mod tests {
  use std::time::Duration;

  use super::*;
  use crate::config::PluginRuntimeConfig;

  /// 场景：限流器窗口语义——burst 内全放行；burst 后按 rate 采样；
  /// **窗口滚动后 count 必须重置、burst 重新可用**（防回归：
  /// 旧版只滚动 window_start 不重置 count，第二窗口起限流永久失效）。
  #[test]
  fn rate_limiter_resets_count_on_window_roll() {
    let cfg = PluginRuntimeConfig {
      log_burst: 3,
      log_rate_per_sec: 0, // 超出 burst 后全丢弃，语义更可断言
      ..PluginRuntimeConfig::default()
    };
    let limiter = LogRateLimiter::new(&cfg);

    // burst 3 内全放行
    assert!(limiter.allow());
    assert!(limiter.allow());
    assert!(limiter.allow());
    // 第 4 条起：rate=0 → 全部丢弃
    assert!(!limiter.allow());

    // 缩小窗口并等真实时间推进超过窗口长度 → 自然触发滚动
    // （now_ms() 起点接近 0，无法向历史回拨，故缩短窗口而非回拨时钟）
    let mut limiter2 = LogRateLimiter::new(&cfg);
    limiter2.window = Duration::from_millis(1);
    assert!(limiter2.allow());
    assert!(limiter2.allow());
    assert!(limiter2.allow());
    assert!(!limiter2.allow());
    std::thread::sleep(Duration::from_millis(5));

    // 窗口滚动后 count 已重置：burst 重新可用（这条不测，bug 藏身）
    assert!(
      limiter2.allow(),
      "burst must be available again after window roll"
    );
    assert!(limiter2.allow());
    assert!(limiter2.allow());
    assert!(!limiter2.allow(), "new window must enforce burst again");
    let _ = limiter; // 原 limiter 仅验证 burst 基线
  }

  /// 场景：`ribo_to_error` 的二分映射——Internal → `internal:`，
  /// 其余（含 invalid_arguments）→ `guest_error:`；**绝不产 forbidden:**。
  #[test]
  fn ribo_to_error_mapping() {
    let internal = plugin_internal("engine init failed", "boom");
    let trap = ribo_to_error(&internal);
    assert!(trap.to_string().starts_with("internal:"), "got {}", trap);

    let guest = plugin_invalid_arguments("ptr<0");
    let trap = ribo_to_error(&guest);
    assert!(
      trap.to_string().starts_with("guest_error: "),
      "got {}",
      trap
    );

    // forbidden 不得经过本函数：直接构造 WasmError::msg("forbidden:{cap}")
    // ——此处仅断言本函数不会因 PLUGIN_FORBIDDEN 产生 forbidden: 前缀
    let forbidden = crate::error::plugin_forbidden("a", "storage");
    let trap = ribo_to_error(&forbidden);
    assert!(
      trap.to_string().starts_with("guest_error: "),
      "got {}",
      trap
    );
  }

  /// 场景：`now_ms()` 单调递增（限流窗口基准不回拨）。
  #[test]
  fn now_ms_is_monotonic() {
    let t1 = now_ms();
    std::thread::sleep(std::time::Duration::from_millis(2));
    let t2 = now_ms();
    assert!(t2 >= t1);
  }
}
