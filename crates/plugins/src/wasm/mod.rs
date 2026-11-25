//! WASM 宿主层。
//!
//! 模块职责：
//!
//! - [`runtime`]：`PluginRuntime` 进程级单例（Engine + 唯一 epoch ticker + ModuleCache）、`HostState` / `HostStateSnapshot`、`run_tool` 执行引擎与取消 / 错误分类机制；
//! - [`hostfns`]：Host Functions（log / storage_get / storage_put / http_proxy，按 ACL 注入）与 `Linker` 构建；
//! - [`storage`]：插件级进程内 storage（DoS 边界常量所在）；
//! - [`abi`]：ABI v0（JSON over linear memory）与 manifest 解析；
//! - [`host`]：`WasmPluginHost`（加载 `.wasm` 为 `Plugin`）与 `WasmPlugin`（Stateless Store + config args 信封）。

pub mod abi;
pub mod host;
pub mod hostfns;
pub mod runtime;
pub mod storage;

#[cfg(test)]
mod tests;
