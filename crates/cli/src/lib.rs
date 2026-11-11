//! ra-cli 库层：REPL 主循环、行命令、渲染与日志分流。
//!
//! `main.rs` 为薄壳（仅参数解析与启动）；全部逻辑置于本 lib，
//! 供 `tests/e2e.rs` 集成测试引用（bin target 不对外导出）。

pub mod app;
pub mod commands;
pub mod config;
pub mod logging;
pub mod render;
