//! ABI v0：JSON over linear memory 的 manifest 解析与 `list_tools` 调用
//! （设计文档 18_ §8）。
//!
//! guest 导出符号（§8.1，均为必选）：
//!
//! | 符号 | 签名 |
//! | :-- | :-- |
//! | `memory` | `(export "memory" (memory 0))`——**必须显式导出** |
//! | `alloc` | `(size: i32) -> ptr: i32`（失败返回 0） |
//! | `dealloc` | `(ptr: i32, size: i32)` |
//! | `list_tools` | `() -> (ptr, len)`，manifest JSON，加载期一次 |
//! | `execute` | `(name_ptr, name_len, args_ptr, args_len) -> (ptr, len)` |
//!
//! 配置注入走 **args 信封**（v0.6 P0-1，§9）：config 经
//! `{"config": cfg, "args": args}` 随业务参数一并写入 guest 内存，
//! 与 Stateless Store 完全一致（加载期写 globals 会在实例重置后丢失）。

use std::sync::Arc;
use std::time::Duration;

use ra_core::error::RiboResult;
use tokio_util::sync::CancellationToken;

use crate::acl::PluginAcl;
use crate::error::{plugin_exec_failed, plugin_internal, plugin_invalid_name};
use crate::traits::ToolManifest;
use crate::wasm::host::PluginShared;
use crate::wasm::runtime::{
  HostState, classify_guest_call_error, classify_wasmtime_error,
  epoch_ticks_for,
};

/// `list_tools` 返回的插件清单（v0.4 P0-4）。
///
/// 顶层含 `plugin_id`（即 `Plugin::id()` 与工具名前缀的来源，不从
/// 文件名 / 哈希推导）；`ToolManifest` 本身不含 `plugin_id`。
pub struct PluginManifest {
  /// 插件 ID（须经 `validate_plugin_id` 命名约束校验后才作为前缀）。
  pub plugin_id: String,
  /// 工具清单（`timeout_hint_ms` 已转为 `Duration` 并钳制到 60s）。
  pub tools: Vec<ToolManifest>,
}

/// manifest JSON 的中间反序列化结构（方案 A，§8.1 v0.8.3 P2-10）。
///
/// `ToolManifest.timeout_hint` 是 `Option<Duration>`，JSON 里是
/// `timeout_hint_ms: Option<u64>`——serde 不会自动转换，必须显式 `From`。
#[derive(serde::Deserialize)]
struct RawToolManifest {
  name: String,
  description: String,
  parameters: serde_json::Value,
  #[serde(default)]
  timeout_hint_ms: Option<u64>,
  #[serde(default)]
  idempotent: bool,
  #[serde(default)]
  memory_limit_bytes: Option<usize>,
}

impl From<RawToolManifest> for ToolManifest {
  fn from(r: RawToolManifest) -> Self {
    ToolManifest {
      name: r.name,
      description: r.description,
      parameters: r.parameters,
      // 60s 钳制：guest 自述超时不可信（§8.1 信任链），防止自述 10 小时
      timeout_hint: r
        .timeout_hint_ms
        .map(|ms| Duration::from_millis(ms.min(60_000))),
      idempotent: r.idempotent,
      memory_limit_bytes: r.memory_limit_bytes,
    }
  }
}

/// manifest JSON 的顶层中间结构。
#[derive(serde::Deserialize)]
struct RawPluginManifest {
  plugin_id: String,
  tools: Vec<RawToolManifest>,
}

/// 解析 `list_tools()` 返回的 manifest JSON（§8.1 v0.4 / v0.8.3）。
///
/// # Errors
///
/// - JSON 结构非法 / `plugin_id` 缺失 → `plugin.plugin_exec_failed`
///   （guest 自述问题，业务级）；
/// - 单工具超时自述超 60s → 钳制为 60s（不报错）。
pub fn parse_manifest(json: &[u8]) -> RiboResult<PluginManifest> {
  let raw: RawPluginManifest = serde_json::from_slice(json).map_err(|e| {
    plugin_exec_failed(
      "<unknown>",
      "list_tools",
      &format!("bad manifest json: {e}"),
    )
  })?;
  Ok(PluginManifest {
    plugin_id: raw.plugin_id,
    tools: raw.tools.into_iter().map(ToolManifest::from).collect(),
  })
}

/// 调 guest 的 `list_tools()` 导出，返回 manifest JSON 字节
/// （经 `Linker` 实例化——host import 在此注入，供 `WasmPluginHost::load`）。
///
/// 加载期**仅调用一次**（结果随后被缓存到 `WasmPlugin.manifests`，
/// §4.2）。执行预算固定 2s（epoch deadline + 内存 limiter），防止
/// 恶意 guest 在加载期死循环 / 巨量分配。
///
/// # Errors
///
/// - 导出缺失 / 实例化失败 → 按 §5.3 映射（缺 memory 等宿主内部错 →
///   `plugin.plugin_internal`；start 段 trap → `classify_wasmtime_error`）；
/// - 读回 / dealloc 失败 → 对应错误码。
pub(crate) fn call_list_tools(
  engine: &wasmtime::Engine,
  linker: &wasmtime::Linker<HostState>,
  module: &wasmtime::Module,
  shared: &Arc<PluginShared>,
  acl: &PluginAcl,
) -> RiboResult<Vec<u8>> {
  // ===== 阶段 1：新建一次性 Store =====
  let state = HostState::new(
    acl.clone(),
    CancellationToken::new(),
    String::new(),
    "list_tools".to_string(),
    shared,
  );
  let mut store = wasmtime::Store::new(engine, state);
  store.set_epoch_deadline(epoch_ticks_for(Duration::from_secs(2)));
  store.limiter(|s| &mut s.limits);

  // ===== 阶段 2：实例化（start 段 trap 按前缀分类，不得一律 Internal） =====
  let instance = linker.instantiate(&mut store, module).map_err(|e| {
    // classify → RiboError 的最小映射：复用 run_tool 的语义
    let classified = classify_wasmtime_error(e);
    match classified {
      crate::wasm::runtime::WasmRuntimeError::Timeout(reason) => {
        plugin_exec_failed(
          "<unknown>",
          "list_tools",
          &format!("timeout: {reason}"),
        )
      }
      crate::wasm::runtime::WasmRuntimeError::GuestError(reason) => {
        plugin_exec_failed("<unknown>", "list_tools", &reason)
      }
      crate::wasm::runtime::WasmRuntimeError::Forbidden(cap) => {
        plugin_invalid_name(
          "plugin_id",
          "<unknown>",
          format!("forbidden: {cap}"),
        )
      }
      crate::wasm::runtime::WasmRuntimeError::Cancelled => plugin_exec_failed(
        "<unknown>",
        "list_tools",
        "cancelled during list_tools",
      ),
      crate::wasm::runtime::WasmRuntimeError::Internal(msg) => {
        plugin_internal("list_tools instantiate", msg)
      }
    }
  })?;

  // ===== 阶段 3：调用 list_tools() =====
  // 结果形态二选一（与 abi_v0_execute 相同的实测校准）：多值 ABI 返回
  // `(ptr, len)` 双 i32；单 i32 返回（Rust PDK）为打包指针，指向
  // 8 字节块 `{ptr: i32 LE, len: i32 LE}`。
  let func = instance
    .get_func(&mut store, "list_tools")
    .ok_or_else(|| plugin_internal("guest list_tools export missing", ""))?;
  let n_results = func.ty(&store).results().len();
  let mut out = vec![wasmtime::Val::I32(0); n_results];
  func
    .call(&mut store, &[], &mut out)
    .map_err(|e| map_guest_call(e, "list_tools"))?;
  let (ptr, len, packed_block) = match n_results {
    2 => (out[0].i32().unwrap_or(0), out[1].i32().unwrap_or(0), None),
    1 => {
      let block = out[0].i32().unwrap_or(0);
      if block == 0 {
        return Err(plugin_exec_failed(
          "<unknown>",
          "list_tools",
          "guest returned null packed pointer",
        ));
      }
      let mem = instance
        .get_memory(&mut store, "memory")
        .ok_or_else(|| plugin_internal("guest memory export missing", ""))?;
      let mut b = [0u8; 8];
      mem
        .read(&mut store, block as usize, &mut b)
        .map_err(|e| plugin_internal("read packed manifest", e))?;
      (
        i32::from_le_bytes(b[0..4].try_into().expect("4 bytes")),
        i32::from_le_bytes(b[4..8].try_into().expect("4 bytes")),
        Some((block, 8)),
      )
    }
    n => {
      return Err(plugin_internal(
        "list_tools results",
        format!("guest list_tools must return 1 or 2 results, got {n}"),
      ));
    }
  };

  // ===== 阶段 4：读回 manifest JSON（ptr == 0 → 空 manifest，随后 parse 报错） =====
  if ptr == 0 || len <= 0 {
    return Err(plugin_exec_failed(
      "<unknown>",
      "list_tools",
      "guest returned empty manifest",
    ));
  }
  let mem = instance
    .get_memory(&mut store, "memory")
    .ok_or_else(|| plugin_internal("guest memory export missing", ""))?;
  let mut buf = vec![0u8; len as usize];
  mem
    .read(&mut store, ptr as usize, &mut buf)
    .map_err(|e| plugin_internal("read manifest", e))?;

  // ===== 阶段 5：释放结果缓冲区（打包块与 guest 自 alloc 的载荷） =====
  // guest 按 ABI 约定必须导出 dealloc；缺失时跳过释放（一次性 Store
  // 生命周期内回收，不构成泄漏），不视为错误。
  let dealloc_func = instance.get_func(&mut store, "dealloc");
  if let Some(dealloc) = dealloc_func {
    if let Some((p, l)) = packed_block {
      let _ = dealloc.call(
        &mut store,
        &[wasmtime::Val::I32(p), wasmtime::Val::I32(l)],
        &mut [],
      );
    }
    if ptr != 0 && len > 0 {
      let _ = dealloc.call(
        &mut store,
        &[wasmtime::Val::I32(ptr), wasmtime::Val::I32(len)],
        &mut [],
      );
    }
  }
  Ok(buf)
}

/// guest 函数调用错误的统一映射（加载期）。
fn map_guest_call(
  e: wasmtime::Error,
  stage: &str,
) -> ra_core::error::RiboError {
  match classify_guest_call_error(e) {
    crate::wasm::runtime::WasmRuntimeError::Timeout(reason) => {
      plugin_exec_failed("<unknown>", stage, &format!("timeout: {reason}"))
    }
    crate::wasm::runtime::WasmRuntimeError::GuestError(reason) => {
      plugin_exec_failed("<unknown>", stage, &reason)
    }
    crate::wasm::runtime::WasmRuntimeError::Forbidden(cap) => {
      plugin_invalid_name("plugin_id", "<unknown>", format!("forbidden: {cap}"))
    }
    crate::wasm::runtime::WasmRuntimeError::Cancelled => {
      plugin_exec_failed("<unknown>", stage, "cancelled")
    }
    crate::wasm::runtime::WasmRuntimeError::Internal(msg) => {
      plugin_internal(stage, msg)
    }
  }
}
