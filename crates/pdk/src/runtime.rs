//! guest 侧 ABI 运行时：线性内存分配器与 ABI v0 导出实现
//! （设计文档 18_ §11）。
//!
//! 仅 `#[ribo_plugin]` 生成的 `#[unsafe(no_mangle)]` 导出与本模块交互：
//!
//! - `alloc` / `dealloc`：**基于 `Vec` 的分配器**——分配走 guest 的
//!   标准堆（dlmalloc），与 guest 自身的 `serde_json` / `String` 共用
//!   同一分配器，不存在区域重叠。**旧方案（自管理 bump + 固定起点
//!   1MB）存在真实缺陷**：rustc wasm 目标的静态数据之后即为 dlmalloc
//!   堆，堆增长会越过 1MB，与 bump 区间互相踩踏（host 写入的 ABI
//!   缓冲覆盖 guest 堆数据 / 反向覆盖），已在 plugin-demo e2e 中实测
//!   复现（`Value` 索引返回 null、bool `to_string` 产生乱码）。
//! - `list_tools` / `execute`：**单 i32 打包返回**——指向 8 字节块
//!   `{ptr: i32 LE, len: i32 LE}`，载荷紧随独立分配。
//!
//! # 内存所有权（§8.2 三分表）
//!
//! - host 经 `alloc` 申请的缓冲（args / 帧化返回），由**使用方**
//!   （host 或 PDK 封装）以相同 `(ptr, len)` 调 `dealloc` 释放；
//! - `dealloc` 以 `Vec::from_raw_parts(ptr, 0, len)` 重建并 drop，
//!   `len` 必须等于分配时的 `size`（所有调用路径均满足——host 的
//!   `alloc_return` 与本模块的 `write_packed` 均按载荷精确长度分配）；
//! - 失配的 `dealloc` 只会泄漏（Stateless Store 下随实例回收），
//!   不会踩内存。

/// ABI `alloc(size)`：按 `size` 分配一字节对齐缓冲，失败返回 0。
///
/// 用 `Vec::with_capacity(size)` 分配后 `forget`，指针交由 ABI 调用方
/// 使用；释放责任见模块文档（§8.2）。`size <= 0` 返回 0（宿主侧视为
/// guest 分配失败，v0.8.1 P0-5）。
#[unsafe(no_mangle)]
pub extern "C" fn alloc(size: i32) -> i32 {
  if size <= 0 {
    return 0;
  }
  let mut buf: Vec<u8> = Vec::with_capacity(size as usize);
  let ptr = buf.as_mut_ptr();
  core::mem::forget(buf);
  ptr as i32
}

/// ABI `dealloc(ptr, len)`：释放 `alloc(size = len)` 得到的缓冲。
///
/// `ptr` / `len` 与分配时不一致时静默忽略（防御性；正常流程由
/// ABI 契约保证配对）。`len <= 0` 或 `ptr <= 0` 直接返回。
#[unsafe(no_mangle)]
pub extern "C" fn dealloc(ptr: i32, len: i32) {
  if ptr <= 0 || len <= 0 {
    return;
  }
  // SAFETY：ptr 来自本模块 alloc（capacity == len），len = 0 的 Vec
  // drop 时按 capacity 释放，与分配大小一致
  unsafe {
    drop(Vec::from_raw_parts(ptr as *mut u8, 0, len as usize));
  }
}

/// ABI `list_tools`：构建 manifest JSON 并以打包指针返回。
pub fn list_tools(plugin_id: &'static str, tools: &[crate::ToolEntry]) -> i32 {
  let bytes = crate::build_manifest(plugin_id, tools)
    .to_string()
    .into_bytes();
  write_packed(&bytes)
}

/// ABI `execute`：读工具名与 args → 解包信封 → 派发 → 序列化结果 →
/// 打包返回。工具 `Err(String)` 生成 `{"ok":false,"error":...}`
/// （4KB 截断），宿主据此回灌业务错误（§11.1 契约要点 ④）。
pub fn execute(
  _plugin_id: &'static str,
  tools: &[crate::ToolEntry],
  name_ptr: i32,
  name_len: i32,
  args_ptr: i32,
  args_len: i32,
) -> i32 {
  let outcome = (|| -> Result<serde_json::Value, String> {
    let name = read_bytes(name_ptr, name_len)?;
    let name = String::from_utf8(name)
      .map_err(|_| "tool name is not utf8".to_string())?;
    let raw = read_bytes(args_ptr, args_len)?;
    let parsed: serde_json::Value = serde_json::from_slice(&raw)
      .map_err(|e| format!("bad args json: {e}"))?;
    let (cfg, args) = crate::unwrap_envelope(parsed);
    crate::dispatch(tools, &name, cfg, args)
  })();
  let payload = match outcome {
    Ok(v) => v.to_string().into_bytes(),
    Err(e) => serde_json::json!({
      "ok": false,
      "error": crate::truncate_error(&e, crate::ERROR_TRUNCATE_BYTES),
    })
    .to_string()
    .into_bytes(),
  };
  write_packed(&payload)
}

/// 把载荷写入线性内存，返回指向 8 字节打包块 `{ptr, len}` 的指针。
fn write_packed(payload: &[u8]) -> i32 {
  let payload_ptr = alloc(payload.len() as i32);
  if payload_ptr == 0 {
    return 0;
  }
  write_bytes(payload_ptr, payload);
  let mut block = [0u8; 8];
  block[0..4].copy_from_slice(&(payload_ptr).to_le_bytes());
  block[4..8].copy_from_slice(&(payload.len() as i32).to_le_bytes());
  let block_ptr = alloc(8);
  if block_ptr == 0 {
    return 0;
  }
  write_bytes(block_ptr, &block);
  block_ptr
}

/// 从线性内存读 `len` 字节（越界由 wasm 引擎 trap，属宿主可见错误）。
fn read_bytes(ptr: i32, len: i32) -> Result<Vec<u8>, String> {
  if ptr <= 0 || len < 0 {
    return Err(format!("bad guest memory range (ptr={ptr}, len={len})"));
  }
  // SAFETY：ptr/len 来自宿主写入的 ABI 参数，线性内存范围校验由 wasm 引擎保证
  Ok(unsafe {
    core::slice::from_raw_parts(ptr as *const u8, len as usize).to_vec()
  })
}

/// 向线性内存写入字节。
fn write_bytes(ptr: i32, data: &[u8]) {
  // SAFETY：ptr 来自本模块的分配器，容量由 alloc 的 size 保证
  unsafe {
    core::ptr::copy_nonoverlapping(data.as_ptr(), ptr as *mut u8, data.len());
  }
}
