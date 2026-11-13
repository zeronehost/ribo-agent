//! `plugin-demo` guest：以 PDK 宏声明的演示插件（18_ §13.2 / §11.1）。
//!
//! 构建产物（wasm32-unknown-unknown，cdylib）经宿主
//! `WasmPluginHost::load` 加载，覆盖三类场景：
//!
//! - [`echo`](mod@plugin)：纯 CPU 工具（幂等，默认 2s 超时）；
//! - [`greet`](mod@plugin)：config（args 信封）+ storage 一次读写
//!   （§11.1 ③ 完整示例）；
//! - [`spin`](mod@plugin)：死循环，演示 epoch 超时 →
//!   `plugin.plugin_timeout`（不打开工具面熔断）。
//!
//! 宿主 target 上本 crate 为空 lib（纯逻辑归 `ra-plug-pdk` 单测覆盖）；
//! guest 代码整体 `cfg(wasm32)` 门控。
//!
//! # Examples
//!
//! ```ignore
//! // 独立构建 guest（产物：target/wasm32-unknown-unknown/debug/plugin_demo.wasm）
//! cargo build --manifest-path crates/plugins/examples/plugin-demo/Cargo.toml \
//!   --target wasm32-unknown-unknown
//! // 端到端集成测试（tests/e2e.rs 自动完成上述构建）
//! cargo test --manifest-path crates/plugins/examples/plugin-demo/Cargo.toml
//! ```

#[cfg(target_arch = "wasm32")]
mod guest {
  use ra_plug_pdk::{Value, json, ribo, ribo_plugin, ribo_tool};

  /// 演示插件：`demo`（工具名宿主侧自动加 `demo__` 前缀）。
  ///
  /// 工具函数直接写在本模块内：`#[ribo_plugin]` 收集带 `#[ribo_tool]`
  /// 标注的函数生成 `__RIBO_TOOLS` 与 ABI v0 导出（alloc / dealloc /
  /// list_tools / execute）。
  #[ribo_plugin(id = "demo")]
  mod plugin {
    use super::*;

    /// echo：原样返回 args（幂等，纯 CPU）。
    #[ribo_tool(
      name = "echo",
      description = "Echoes the arguments back",
      parameters = r#"{ "type": "object", "properties": {} }"#,
      idempotent = true
    )]
    pub fn echo(args: Value) -> Result<Value, String> {
      ribo::log("echo called");
      Ok(args)
    }

    /// greet：config 前缀 + storage 计数（§11.1 ③）。
    ///
    /// - config 经 args 信封由宿主注入，`ribo::config()` 透明读取；
    /// - `storage::get` 的 `Ok(None)` = key 不存在（首次调用），
    ///   用 `unwrap_or(0)` 而非报错（v0.8.1 P1-2）。
    #[ribo_tool(
      name = "greet",
      description = "Greets a name with a configurable prefix, counting visits in storage",
      parameters = r#"{
        "type": "object",
        "properties": { "name": { "type": "string" } },
        "required": ["name"]
      }"#,
      idempotent = true
    )]
    pub fn greet(args: Value) -> Result<Value, String> {
      let cfg = ribo::config();
      let prefix = cfg
        .as_ref()
        .and_then(|c| c["greeting_prefix"].as_str())
        .unwrap_or("Hello");
      let name = args["name"].as_str().ok_or("missing name")?;

      // storage：进程内 / 插件级 / 不持久化（§6.2）
      let key = format!("count:{name}");
      let seen: u64 = ribo::storage::get(&key)?
        .map(|b| String::from_utf8_lossy(&b).parse().unwrap_or(0))
        .unwrap_or(0);
      ribo::storage::put(&key, (seen + 1).to_string().as_bytes())?;

      Ok(json!({ "message": format!("{prefix}, {name}! seen={seen}") }))
    }

    /// spin：死循环（epoch 超时演示；100ms 预算即被宿主打断）。
    #[ribo_tool(
      name = "spin",
      description = "Spins the CPU forever; demonstrates epoch timeout",
      parameters = r#"{ "type": "object", "properties": {} }"#,
      idempotent = true,
      timeout_hint_ms = 100
    )]
    pub fn spin(_args: Value) -> Result<Value, String> {
      // 自增循环（避免被编译器裁剪为空循环）
      let mut n: u64 = 0;
      loop {
        n = n.wrapping_add(1);
        if n % u64::MAX == 0 {
          n = 0;
        }
      }
    }

    /// fetch：经 host 代理发起 GET（§14 DoD 3 越权演示——宿主 ACL
    /// 未授予 http 能力时，guest 的请求在 host function 入口被拦）。
    #[ribo_tool(
      name = "fetch",
      description = "Fetches a URL through the host http proxy",
      parameters = r#"{
        "type": "object",
        "properties": { "url": { "type": "string" } },
        "required": ["url"]
      }"#,
      idempotent = false,
      timeout_hint_ms = 10000
    )]
    pub fn fetch(args: Value) -> Result<Value, String> {
      let url = args["url"].as_str().ok_or("missing url")?;
      let body = ribo::http_get(url)?;
      Ok(json!({ "body": body }))
    }
  }
}
