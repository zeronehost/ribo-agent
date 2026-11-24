//! 测试辅助：固定根目录下的临时子目录。
//!
//! 把测试产物落到 `tmp/<caller>-<pid>-<seq>` 下（相对于
//! `cargo test -p ra-harness` 执行时的工作目录——即 workspace 根），
//! 便于事后检查。PID + 进程内全局计数器保证并行测试（多进程、多线程）
//! 都不会踩到同一路径。目录在测试结束时**不清理**，便于失败时回看
//! 落盘文件；手动删 `tmp/` 即可清空。

use std::{
  path::PathBuf,
  sync::atomic::{AtomicU64, Ordering},
};

/// 所有测试产物的根目录（相对于 `cargo test` 运行目录的相对路径）。
///
/// `cargo test -p ra-harness` 默认从 workspace 根启动，此时
/// `../../tmp` 解析为 `<workspace>/tmp/`；用户可通过改此常量指向别的位置。
pub const HARNESS_TEST_ROOT: &str = "../../tmp";

/// 给某个测试调用者分配一个带唯一后缀的子目录。
///
/// 目录会立即创建（递归 `create_dir_all`），返回绝对路径。
/// 内部用 **PID + 进程内全局计数器** 组合保证唯一性：
/// - PID 隔离 cargo test 并行的多个测试二进制（每个二进制一个 pid）；
/// - 计数器隔离同一测试二进制内的并行线程（AtomicU64 在进程内自增）。
pub fn test_dir(caller: &str) -> PathBuf {
  // 相对路径从 `cargo test` 运行目录解绝对，比硬编码绝对路径稳
  let root = std::env::current_dir()
    .unwrap_or_else(|_| PathBuf::from("."))
    .join(HARNESS_TEST_ROOT);
  std::fs::create_dir_all(&root).expect("create harness test root dir");

  static SEQ: AtomicU64 = AtomicU64::new(0);
  let seq = SEQ.fetch_add(1, Ordering::Relaxed);
  let pid = std::process::id();
  let sub = root.join(format!("{caller}-{pid}-{seq}"));
  std::fs::create_dir_all(&sub).expect("create harness test subdir");
  sub
}
