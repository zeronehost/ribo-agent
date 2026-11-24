//! 日志落文件：stdout 只留对话渲染（模式显式化后固定）。
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
    .with_timer(
      tracing_subscriber::fmt::time::OffsetTime::local_rfc_3339()
        .unwrap_or_else(|_| {
          tracing_subscriber::fmt::time::OffsetTime::new(
            time::UtcOffset::UTC,
            time::format_description::well_known::Rfc3339,
          )
        }),
    )
    .with_ansi(false)
    .try_init(); // 全局订阅器仅首个成功；E2E 用 rolling::never 按用例注入
  Ok(guard)
}

/// 按「全局优先、项目兜底」初始化日志。
///
/// `preferred` 为全局域目录（Global 域启用时），`fallback` 为项目根。
/// 全局目录不可创建/不可写时（如只读共享目录、权限不足）不阻断启动，
/// 回退到 `fallback` 并在 stderr 提示；两者都失败才返回错误。
///
/// # Errors
///
/// `preferred` 与 `fallback` 均无法创建日志目录时返回 IO 错误。
pub fn init_file_logging_with_fallback(
  preferred: &Path,
  fallback: &Path,
) -> std::io::Result<tracing_appender::non_blocking::WorkerGuard> {
  match init_file_logging(preferred) {
    Ok(guard) => Ok(guard),
    Err(err) if preferred != fallback => {
      eprintln!(
        "日志目录 {} 不可用（{err}），回退到项目目录 {}",
        preferred.display(),
        fallback.display()
      );
      init_file_logging(fallback)
    }
    Err(err) => Err(err),
  }
}
