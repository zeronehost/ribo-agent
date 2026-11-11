//! 日志落文件：stdout 只留对话渲染（概设 §7 决策 5）。
//!
//! 经 `tracing-appender` 写入 `<root>/logs/cli.log.<日期>`（按日滚动），
//! 生产入口经 `try_init` 全局兜底；E2E 用 `rolling::never` 构造独立
//! 订阅器（进程内多测试并存）。

use std::path::Path;

/// 初始化全局文件日志订阅器（生产入口）。
///
/// 非阻塞写入（`non_blocking`）：调用方必须持有返回的 [`WorkerGuard`]
/// 至进程结束（写入线程随 guard drop 而停止，提前 drop 会丢尾部日志）。
///
/// # Errors
///
/// 目录创建 / 日志文件初始化失败时返回 IO 错误。
pub fn init_file_logging(
  root: &Path,
) -> std::io::Result<tracing_appender::non_blocking::WorkerGuard> {
  let dir = root.join("logs");
  std::fs::create_dir_all(&dir)?;
  let appender = tracing_appender::rolling::daily(&dir, "cli.log");
  let (writer, guard) = tracing_appender::non_blocking(appender);
  let _ = tracing_subscriber::fmt()
    .with_writer(writer) // NonBlocking 直接实现 MakeWriter
    .with_ansi(false)
    .try_init(); // 全局订阅器仅首个成功；E2E 用 rolling::never 按用例注入
  Ok(guard)
}
