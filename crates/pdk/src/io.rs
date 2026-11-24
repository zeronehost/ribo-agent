//! guest 侧 IO 封装：host import 声明与帧化返回解码。
//!
//! 仅 `wasm32` 目标可用（host import 经 `wasm_import_module` 链接）。
//! 四个 import 与宿主侧 `build_linker`一一对应：
//!
//! | import | guest 侧签名 | 返回语义 |
//! | :-- | :-- | :-- |
//! | `ribo.log` | `(ptr, len) -> i32` | `0` = 成功且无结果 |
//! | `ribo.storage_get` | `(key_ptr, key_len) -> i32` | `ptr`；`0` = key 不存在 |
//! | `ribo.storage_put` | `(key_ptr, key_len, val_ptr, val_len) -> i32` | `0` = 成功且无结果 |
//! | `ribo.http_proxy` | `(ptr, len) -> i32` | `ptr` = 帧化响应缓冲区 |
//!
//! 返回携带载荷的 import（`storage_get` / `http_proxy`）采用**帧化返回**：
//! host 把 `[len: u32 LE][payload]` 写入一块 guest 缓冲区并返回首址，
//! 本模块读首 4 字节取长度后读载荷，用完 `dealloc` 整块（`4 + len`）。
//!
//! **长度上限协作**：本模块在发请求**前**检查
//! key/value/请求/日志长度（与宿主侧同值），给插件作者清晰的错误文案；
//! host 侧检查是**兜底**（防恶意 guest）。host 拒绝（ACL / 策略 /
//! 超限）经 **trap** 中止实例执行，错误在宿主侧分类回灌，guest 侧
//! `Err(String)` 仅覆盖本地预检失败。
//!
//! **`ptr == 0` 哨兵路径不 dealloc**（ABI 所有权）。

use crate::{MAX_KEY_LEN, MAX_LOG_LEN, MAX_REQ_LEN, MAX_VALUE_LEN};

/// host import 声明（与 `build_linker` 的模块名/函数名逐项一致）。
///
/// 放在私有 `imp` 模块内，避免 extern 符号与同名包装函数冲突；
/// 缺失的 import 由宿主 Linker 实例化期报错（ABI 一致性由宿主
/// CI 比对守护）。
mod imp {
  #[link(wasm_import_module = "ribo")]
  unsafe extern "C" {
    pub fn log(ptr: i32, len: i32) -> i32;
    pub fn storage_get(key_ptr: i32, key_len: i32) -> i32;
    pub fn storage_put(
      key_ptr: i32,
      key_len: i32,
      val_ptr: i32,
      val_len: i32,
    ) -> i32;
    pub fn http_proxy(ptr: i32, len: i32) -> i32;
  }
}

/// 插件级进程内存储（不持久化，配额由 host 配置）。
///
/// # 契约
///
/// - `get` 返回 `Ok(None)` = key 不存在（正常态，非错误）；
/// - key ≤ 256B、value ≤ 256KB（与宿主侧同值，发请求前本地预检）；
/// - host 侧拒绝（ACL / 配额）经 trap 中止执行，错误在宿主分类回灌。
pub mod storage {
  use super::*;

  /// 读取 key 对应的 value；`Ok(None)` = key 不存在。
  ///
  /// # Errors
  ///
  /// 本地预检失败（key 超长）返回 `Err`，错误文案含上限值
  /// （便于插件作者定位）；host 侧拒绝走 trap，不经过本返回值。
  pub fn get(key: &str) -> Result<Option<Vec<u8>>, String> {
    check_len("storage key", key.len(), MAX_KEY_LEN)?;
    let key_ptr = crate::runtime::alloc(key.len() as i32);
    if key_ptr == 0 {
      return Err("guest alloc failed for key".to_string());
    }
    // SAFETY：key_ptr 来自本 crate bump 分配器，容量即 key.len()
    unsafe { write_bytes(key_ptr, key.as_bytes()) };
    // SAFETY：extern 声明与宿主 build_linker 签名一致
    let ret = unsafe { imp::storage_get(key_ptr, key.len() as i32) };
    if ret == 0 {
      // 0 = key 不存在哨兵：不 dealloc、不读内存（ABI 所有权）
      return Ok(None);
    }
    read_framed(ret).map(Some)
  }

  /// 写入 key/value；成功返回 `Ok(())`。
  ///
  /// # Errors
  ///
  /// 本地预检失败（key/value 超长）或 guest 分配失败返回 `Err`；
  /// host 侧拒绝（超配额 / ACL）走 trap。
  pub fn put(key: &str, value: &[u8]) -> Result<(), String> {
    check_len("storage key", key.len(), MAX_KEY_LEN)?;
    check_len("storage value", value.len(), MAX_VALUE_LEN)?;
    let key_ptr = crate::runtime::alloc(key.len() as i32);
    let val_ptr = crate::runtime::alloc(value.len() as i32);
    if key_ptr == 0 || (val_ptr == 0 && !value.is_empty()) {
      return Err("guest alloc failed for key/value".to_string());
    }
    // SAFETY：两个指针均来自 bump 分配器，容量与写入长度一致
    unsafe {
      write_bytes(key_ptr, key.as_bytes());
      if !value.is_empty() {
        write_bytes(val_ptr, value);
      }
    }
    // SAFETY：extern 声明与宿主 build_linker 的 4 参签名一致
    let ret = unsafe {
      imp::storage_put(key_ptr, key.len() as i32, val_ptr, value.len() as i32)
    };
    if ret == 0 {
      Ok(()) // 0 = 成功且无结果
    } else {
      Err("storage_put failed".to_string())
    }
  }
}

/// 经 host 代理发起 GET 请求（`ribo.http_proxy`，受控代理）。
///
/// guest 无直接网络访问；URL / 方法 / body 打包为请求 JSON
/// （≤ 64KB）送宿主校验（ACL + UrlPolicy + 响应体上限）。
///
/// # Errors
///
/// 本地预检失败（请求 JSON 超限）或响应解码失败返回 `Err`；
/// host 侧拒绝（ACL / 策略 / 超限 / 取消）经 trap 中止执行。
pub fn http_get(url: &str) -> Result<String, String> {
  let req = serde_json::json!({ "url": url, "method": "GET" });
  let bytes = req.to_string().into_bytes();
  check_len("http request", bytes.len(), MAX_REQ_LEN)?;
  let ptr = crate::runtime::alloc(bytes.len() as i32);
  if ptr == 0 {
    return Err("guest alloc failed for http request".to_string());
  }
  // SAFETY：ptr 来自 bump 分配器，容量即 bytes.len()
  unsafe { write_bytes(ptr, &bytes) };
  // SAFETY：extern 声明与宿主 build_linker 签名一致
  let ret = unsafe { imp::http_proxy(ptr, bytes.len() as i32) };
  if ret == 0 {
    return Err("http_proxy returned null".to_string());
  }
  let body = read_framed(ret)?;
  Ok(String::from_utf8_lossy(&body).into_owned())
}

/// 输出日志到宿主（`ribo.log`，限流）。
///
/// 超配额由宿主直接丢弃（不报错）；文本 ≤ 16KB。本地预检失败同样
/// 静默丢弃（与限流同语义，防噪声放大）。
pub fn log(message: &str) {
  if check_len("log payload", message.len(), MAX_LOG_LEN).is_err() {
    return;
  }
  let ptr = crate::runtime::alloc(message.len() as i32);
  if ptr == 0 {
    return;
  }
  // SAFETY：ptr 来自 bump 分配器，容量即 message.len()
  unsafe {
    write_bytes(ptr, message.as_bytes());
    // SAFETY：extern 声明与宿主 build_linker 签名一致
    let _ = imp::log(ptr, message.len() as i32); // 0 = 成功且无结果
  }
}

// ===== 内部辅助 =====

/// 长度预检：超过上限返回带具体数值的 `Err`（协作式报错）。
fn check_len(what: &str, len: usize, max: usize) -> Result<(), String> {
  if len > max {
    Err(format!("{what} exceeds limit ({len} > {max})"))
  } else {
    Ok(())
  }
}

/// 解码帧化返回：`ptr` 指向 `[len: u32 LE][payload]`，读载荷后
/// `dealloc` 整块（`4 + len`，所有权三分表：host 申请、guest 释放）。
///
/// # Errors
///
/// `ptr` 非法 / 长度头超界 → `Err`（防御性：正常流程由宿主保证）。
fn read_framed(ptr: i32) -> Result<Vec<u8>, String> {
  if ptr <= 0 {
    return Err(format!("bad framed ptr {ptr}"));
  }
  // SAFETY：帧头 4 字节由宿主 alloc_return 写入（ptr 有效性已保证）
  let header = unsafe { core::slice::from_raw_parts(ptr as *const u8, 4) };
  let len =
    u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
  // SAFETY：载荷区紧随帧头（宿主 alloc_return 的写入布局）
  let payload =
    unsafe { core::slice::from_raw_parts((ptr + 4) as *const u8, len) }
      .to_vec();
  // 释放整块（帧头 + 载荷）：host 申请、guest 释放（三分表）；
  // bump 分配器下为 no-op（Stateless Store 模型）
  crate::runtime::dealloc(ptr, (4 + len) as i32);
  Ok(payload)
}

/// 向线性内存写入字节（与 `runtime::write_bytes` 同语义，私有复用）。
///
/// # Safety
///
/// `ptr` 必须来自本 crate bump 分配器且容量 ≥ `data.len()`。
unsafe fn write_bytes(ptr: i32, data: &[u8]) {
  // SAFETY：调用方保证 ptr 有效
  unsafe {
    core::ptr::copy_nonoverlapping(data.as_ptr(), ptr as *mut u8, data.len());
  }
}
