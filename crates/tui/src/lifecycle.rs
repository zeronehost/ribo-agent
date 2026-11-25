//! 生命周期管理：raw mode RAII guard、终端恢复单一入口与 panic hook。
//!
//!
//! 核心约束：`std::process::exit` **不执行 Drop**——所有退出路径必须先
//! 显式调用 [`restore_terminal`] 再 exit；[`finish_exit`] 是
//! 「正常退出 / 超时强退」两条路径的同源出口。

use std::time::Duration;

use anyhow::Context as _;

/// Ctrl-C 超时默认值（2s，可被配置文件 `[tui] ctrl_c_timeout_ms` 覆盖）。
pub const DEFAULT_CTRL_C_TIMEOUT: Duration = Duration::from_secs(2);

/// raw mode + 备用屏 + 隐藏光标的 RAII guard。
///
/// Drop 逆序恢复——但**正常退出不依赖 Drop**（`process::exit` 不执行
/// Drop），退出路径统一走 [`finish_exit`]。
pub struct RawModeGuard;

impl RawModeGuard {
  /// 进入 raw mode + 备用屏 + 隐藏光标。
  ///
  /// # Errors
  ///
  /// 任一步骤失败时返回错误（调用方在构造 guard **之前**用 stderr
  /// 报错退出）。
  pub fn enter() -> anyhow::Result<Self> {
    crossterm::terminal::enable_raw_mode().context("enable raw mode failed")?;
    crossterm::execute!(
      std::io::stdout(),
      crossterm::terminal::EnterAlternateScreen,
      crossterm::cursor::Hide
    )
    .context("enter alternate screen failed")?;
    Ok(Self)
  }
}

impl Drop for RawModeGuard {
  fn drop(&mut self) {
    // 兜底恢复（正常退出已由 finish_exit 显式恢复，此处幂等）
    let _ = restore_terminal();
  }
}

/// 显式恢复终端（单一清理入口）。
///
/// 逆序执行：显光标 → 离开备用屏 → 关闭 raw mode。被「正常退出 /
/// 超时强退 / panic hook」共用（`process::exit` 不执行 Drop，
/// 绝不裸退出）。
pub fn restore_terminal() -> std::io::Result<()> {
  crossterm::execute!(
    std::io::stdout(),
    crossterm::cursor::Show,
    crossterm::terminal::LeaveAlternateScreen
  )?;
  crossterm::terminal::disable_raw_mode()
}

/// 退出同源出口：显式恢复终端后以指定退出码结束进程。
///
/// `code`：优雅退出 0 / 超时强退 1。
pub fn finish_exit(code: i32) -> ! {
  let _ = restore_terminal();
  std::process::exit(code);
}

/// 安装 panic hook：恢复终端后再链式调用原 hook。
pub fn install_panic_hook() {
  let original = std::panic::take_hook();
  std::panic::set_hook(Box::new(move |info| {
    let _ = restore_terminal();
    original(info);
  }));
}

/// 从配置文本解析 `[tui] ctrl_c_timeout_ms`（缺失 / 非法 → 默认 2s）。
///
/// TUI 层不在六层 schema 内，由 ra-tui 自行
/// 读取同一配置文件的本表，单字段、失败保守回退。
pub fn parse_ctrl_c_timeout(config_text: Option<&str>) -> Duration {
  let Some(text) = config_text else {
    return DEFAULT_CTRL_C_TIMEOUT;
  };
  let Ok(table) = text.parse::<toml::Table>() else {
    return DEFAULT_CTRL_C_TIMEOUT;
  };
  table
    .get("tui")
    .and_then(|tui| tui.get("ctrl_c_timeout_ms"))
    .and_then(|v| v.as_integer())
    .map(|ms| {
      if ms <= 0 {
        DEFAULT_CTRL_C_TIMEOUT
      } else {
        Duration::from_millis(ms as u64)
      }
    })
    .unwrap_or(DEFAULT_CTRL_C_TIMEOUT)
}

#[cfg(test)]
mod tests {
  use super::*;

  // 场景：缺失 / 损坏 / 非法配置保守回退默认 2s。
  #[test]
  fn ctrl_c_timeout_falls_back_to_default() {
    assert_eq!(parse_ctrl_c_timeout(None), DEFAULT_CTRL_C_TIMEOUT);
    assert_eq!(
      parse_ctrl_c_timeout(Some("broken [[[")),
      DEFAULT_CTRL_C_TIMEOUT
    );
    assert_eq!(
      parse_ctrl_c_timeout(Some("[tui]\nctrl_c_timeout_ms = 0")),
      DEFAULT_CTRL_C_TIMEOUT
    );
    assert_eq!(
      parse_ctrl_c_timeout(Some("[tui]\nctrl_c_timeout_ms = 500")),
      Duration::from_millis(500)
    );
  }
}
