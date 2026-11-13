//! `ra-plug-pdk`：Ribo Agent 插件 guest 侧 Rust SDK（设计文档 18_ §11）。
//!
//! 让 Rust guest 以接近原生 [`AgentTool`](https://docs.rs/ra-tools) 的写法
//! 暴露工具，封装 ABI v0 的全部胶水：线性内存分配、args 配置信封、
//! host import 与长度上限。用法（§11.1）：
//!
//! ```ignore
//! // 编译目标为 wasm32-unknown-unknown（host 上仅用于逻辑测试）
//! use ra_plug_pdk::{ribo_plugin, ribo_tool, config, storage};
//! use serde_json::{json, Value};
//!
//! #[ribo_tool(name = "echo", description = "Echoes args back",
//!             parameters = r#"{ "type": "object" }"#, idempotent = true)]
//! fn echo(args: Value) -> Result<Value, String> { Ok(args) }
//!
//! #[ribo_tool(name = "greet", description = "Greets with config",
//!             parameters = r#"{ "type": "object" }"#, idempotent = true)]
//! fn greet(args: Value) -> Result<Value, String> {
//!   let prefix = config().and_then(|c| c["prefix"].as_str().map(String::from))
//!     .unwrap_or_else(|| "Hello".into());
//!   Ok(json!({ "message": format!("{prefix}!") }))
//! }
//!
//! #[ribo_plugin(id = "demo")]
//! mod tools {
//!   # use super::{echo, greet};
//!   pub(super) use super::*;
//! }
//! ```
//!
//! # ABI 约定（wasmtime 50 / rustc 稳定性实测校准）
//!
//! - **单 i32 打包返回**：rustc 不支持 `extern "C"` 多值返回，故
//!   `list_tools` / `execute` 返回一个 i32，指向 8 字节块
//!   `{ptr: i32 LE, len: i32 LE}`（块与载荷均由本 SDK 的 bump 分配器
//!   分配；宿主读完后经 `dealloc` 释放，Stateless 模型下 no-op）；
//! - **帧化 host 返回**：`storage_get` / `http_proxy` 的 import 只能
//!   返回单 i32，无法同时携带长度 → 宿主写入 `[len: u32 LE][payload]`，
//!   本 SDK 读首 4 字节取长度后读载荷；
//! - **args 信封**（§9）：宿主在 `config = Some(cfg)` 时传入
//!   `{"config": cfg, "args": args}`，本 SDK 透明解包——插件作者仍以
//!   `args["url"]` 读业务参数，配置经 [`config`] 读取；
//! - **错误信封**：工具返回 `Err(String)` 时 SDK 生成
//!   `{"ok":false,"error":...}`（4KB 截断），宿主据此回灌业务错误；
//! - **长度上限**（§8.1 DoS 边界）：与宿主侧同值，PDK 在发请求**前**
//!   检查（协作），宿主兜底拒绝（防恶意 guest）。
//!
//! # 注意
//!
//! guest 只应编译到 `wasm32-unknown-unknown`；在宿主 target 上编译时
//! 仅纯逻辑（信封解析 / 限长检查 / manifest 构建）可用，IO 封装
//! （[`storage`] / `http_get` / `log`）需要 wasm host import。

pub use ra_plug_pdk_macro::{ribo_plugin, ribo_tool};
pub use serde_json::{Value, json};

/// guest 侧 ABI 运行时（线性内存分配器与 ABI v0 导出实现）。
///
/// 仅 `wasm32` 目标可用：`memory_size` / `memory_grow` 内联指令与
/// `#[unsafe(no_mangle)]` 导出只在 wasm 目标上有意义；宿主 target
/// 上本 crate 仅保留纯逻辑（信封解析 / manifest 构建 / dispatch），
/// 供单测与宏展开检查使用。
#[cfg(target_arch = "wasm32")]
pub mod runtime;

/// guest 侧 IO 封装（host import），仅 `wasm32` 目标可用。
#[cfg(target_arch = "wasm32")]
pub mod io;

/// PDK 门面：插件作者的主要入口命名空间（§11.1 示例用法）。
///
/// - [`ribo::config`]：读取当前调用上下文的插件配置（纯逻辑，宿主
///   target 也可用——供单测）；
/// - [`ribo::storage`] / [`ribo::http_get`] / [`ribo::log`]：host
///   import 封装（仅 wasm32）。
pub mod ribo {
  /// 读取当前调用上下文的插件配置（转发 crate 根实现）。
  pub fn config() -> Option<serde_json::Value> {
    crate::config()
  }

  #[cfg(target_arch = "wasm32")]
  pub use crate::io::{http_get, log, storage};
}

/// 工具元数据（`#[ribo_tool]` 生成的描述符，构建 `list_tools` manifest）。
#[derive(Debug, Clone, Copy)]
pub struct ToolMeta {
  /// 工具短名（宿主侧自动加 `{plugin_id}__` 前缀）。
  pub name: &'static str,
  /// 给 LLM 看的工具描述。
  pub description: &'static str,
  /// 参数 JSON Schema（延迟构建，manifest 加载期调用一次）。
  pub parameters_fn: fn() -> serde_json::Value,
  /// 单工具超时（毫秒；缺省 2000）。
  pub timeout_hint_ms: Option<u64>,
  /// 幂等标记（决定宿主侧 Parallel / Sequential 调度）。
  pub idempotent: bool,
  /// 单工具线性内存覆写（仅收紧）。
  pub memory_limit_bytes: Option<usize>,
}

/// 工具条目：元数据 + 调用入口。
#[derive(Clone, Copy)]
pub struct ToolEntry {
  /// 工具元数据。
  pub meta: ToolMeta,
  /// 调用入口：接收（已解包信封的）业务 args，返回结果或错误文本。
  pub call: fn(serde_json::Value) -> Result<serde_json::Value, String>,
}

// ===== 长度上限（与宿主侧 crates/plugins/src/wasm 同值，§8.1 契约） =====

/// storage key 长度上限（字节）。
pub const MAX_KEY_LEN: usize = 256;
/// storage value 长度上限（字节）。
pub const MAX_VALUE_LEN: usize = 256 * 1024;
/// http 请求 JSON 长度上限（字节）。
pub const MAX_REQ_LEN: usize = 64 * 1024;
/// 日志文本长度上限（字节）。
pub const MAX_LOG_LEN: usize = 16 * 1024;
/// 错误文本回灌截断上限（字节，与宿主侧 `truncate_error` 一致）。
pub const ERROR_TRUNCATE_BYTES: usize = 4096;

thread_local! {
  static CURRENT_CONFIG: std::cell::RefCell<Option<serde_json::Value>> =
    const { std::cell::RefCell::new(None) };
}

/// 读取当前调用上下文的插件配置（args 信封顶层 `config`）。
///
/// 每次工具调用开始时由 `execute` 入口写入、结束后恢复（支持同一
/// wasm 实例内连续多次 execute，v0.8.3 P2-5）；未配置时返回 `None`。
pub fn config() -> Option<serde_json::Value> {
  CURRENT_CONFIG.with(|c| c.borrow().clone())
}

/// 在 `cfg` 上下文中调用 `f`，结束后恢复原值（RAII 风格，防串味）。
#[cfg(any(target_arch = "wasm32", test))]
pub(crate) fn with_config<R>(
  cfg: Option<serde_json::Value>,
  f: impl FnOnce() -> R,
) -> R {
  CURRENT_CONFIG.with(|c| {
    let old = c.borrow().clone();
    *c.borrow_mut() = cfg;
    let out = f();
    *c.borrow_mut() = old;
    out
  })
}

/// 解包 args 信封（§9）：顶层为 `{"config": cfg, "args": args}` 时拆出
/// 配置与业务参数；否则原样透传（配置为 `None`）。
#[cfg(any(target_arch = "wasm32", test))]
pub(crate) fn unwrap_envelope(
  raw: serde_json::Value,
) -> (Option<serde_json::Value>, serde_json::Value) {
  match &raw {
    serde_json::Value::Object(map)
      if map.contains_key("config") && map.contains_key("args") =>
    {
      (Some(raw["config"].clone()), raw["args"].clone())
    }
    _ => (None, raw),
  }
}

/// 按 UTF-8 字符边界截断（与宿主侧 `truncate_error` 同语义）。
#[cfg(any(target_arch = "wasm32", test))]
pub(crate) fn truncate_error(s: &str, max_bytes: usize) -> String {
  if s.len() <= max_bytes {
    return s.to_owned();
  }
  let mut end = max_bytes.min(s.len());
  while end > 0 && !s.is_char_boundary(end) {
    end -= 1;
  }
  let mut out = String::with_capacity(end + 16);
  out.push_str(&s[..end]);
  out.push_str("... [truncated]");
  out
}

/// 构建插件 manifest JSON（`list_tools` 载荷，§8.1 顶层格式）。
#[cfg(any(target_arch = "wasm32", test))]
pub(crate) fn build_manifest(
  plugin_id: &str,
  tools: &[ToolEntry],
) -> serde_json::Value {
  serde_json::json!({
    "plugin_id": plugin_id,
    "tools": tools
      .iter()
      .map(|t| serde_json::json!({
        "name": t.meta.name,
        "description": t.meta.description,
        "parameters": (t.meta.parameters_fn)(),
        "timeout_hint_ms": t.meta.timeout_hint_ms,
        "idempotent": t.meta.idempotent,
        "memory_limit_bytes": t.meta.memory_limit_bytes,
      }))
      .collect::<Vec<_>>(),
  })
}

/// 按短名派发工具调用（信封已解包）。
#[cfg(any(target_arch = "wasm32", test))]
pub(crate) fn dispatch(
  tools: &[ToolEntry],
  name: &str,
  cfg: Option<serde_json::Value>,
  args: serde_json::Value,
) -> Result<serde_json::Value, String> {
  let entry = tools
    .iter()
    .find(|t| t.meta.name == name)
    .ok_or_else(|| format!("unknown tool `{name}`"))?;
  with_config(cfg, || (entry.call)(args))
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  /// 场景：args 信封解包——`{"config":..,"args":..}` 拆出两者；
  /// 普通对象原样透传（config = None）。
  #[test]
  fn unwrap_envelope_splits_config_and_args() {
    let (cfg, args) = unwrap_envelope(json!({
      "config": { "greeting_prefix": "Hi" },
      "args": { "name": "a" }
    }));
    assert_eq!(cfg, Some(json!({ "greeting_prefix": "Hi" })));
    assert_eq!(args, json!({ "name": "a" }));

    // 仅含 config / 仅含 args / 无键：均视为普通 args
    let (cfg, args) = unwrap_envelope(json!({ "url": "https://x" }));
    assert_eq!(cfg, None);
    assert_eq!(args, json!({ "url": "https://x" }));
  }

  /// 场景：`with_config` 调用后恢复原值（支持同一实例连续多次 execute）。
  #[test]
  fn with_config_restores_previous_value() {
    let first = with_config(Some(json!({"a": 1})), config);
    assert_eq!(first, Some(json!({"a": 1})));
    assert_eq!(config(), None, "config must be restored after the call");
  }

  /// 场景：`truncate_error` UTF-8 边界安全（与宿主侧同语义）。
  #[test]
  fn truncate_error_respects_char_boundaries() {
    let text = "🦀".repeat(2000);
    let cut = truncate_error(&text, 4096);
    assert!(cut.ends_with("... [truncated]"));
    assert_eq!(truncate_error("short", 4096), "short");
  }

  /// 场景：`build_manifest` 产出 §8.1 顶层格式（plugin_id + tools，
  /// 含 timeout_hint_ms / idempotent / memory_limit_bytes 字段名）。
  #[test]
  fn build_manifest_matches_host_contract() {
    fn params() -> serde_json::Value {
      json!({ "type": "object", "properties": {} })
    }
    fn call(_: serde_json::Value) -> Result<serde_json::Value, String> {
      Ok(json!(null))
    }
    let tools = [ToolEntry {
      meta: ToolMeta {
        name: "echo",
        description: "Echoes",
        parameters_fn: params,
        timeout_hint_ms: Some(10_000),
        idempotent: true,
        memory_limit_bytes: Some(16 << 20),
      },
      call,
    }];
    let m = build_manifest("demo", &tools);
    assert_eq!(m["plugin_id"], "demo");
    assert_eq!(m["tools"][0]["name"], "echo");
    assert_eq!(m["tools"][0]["timeout_hint_ms"], 10_000);
    assert_eq!(m["tools"][0]["idempotent"], true);
    assert_eq!(m["tools"][0]["memory_limit_bytes"], 16 << 20);
    assert_eq!(m["tools"][0]["parameters"]["type"], "object");
  }

  /// 场景：`dispatch` 按短名派发、未知工具报错、config 注入生效。
  #[test]
  fn dispatches_by_name_and_injects_config() {
    fn params() -> serde_json::Value {
      json!({ "type": "object" })
    }
    fn greet(_args: serde_json::Value) -> Result<serde_json::Value, String> {
      let prefix = config()
        .and_then(|c| c["prefix"].as_str().map(String::from))
        .unwrap_or_else(|| "Hello".into());
      Ok(json!({ "message": prefix }))
    }
    let tools = [ToolEntry {
      meta: ToolMeta {
        name: "greet",
        description: "g",
        parameters_fn: params,
        timeout_hint_ms: None,
        idempotent: true,
        memory_limit_bytes: None,
      },
      call: greet,
    }];

    let out =
      dispatch(&tools, "greet", Some(json!({"prefix": "你好"})), json!({}))
        .expect("dispatch greet");
    assert_eq!(out, json!({ "message": "你好" }));
    assert_eq!(config(), None, "config restored after dispatch");

    let err = dispatch(&tools, "ghost", None, json!({})).unwrap_err();
    assert!(err.contains("unknown tool"), "got: {err}");
  }
}
