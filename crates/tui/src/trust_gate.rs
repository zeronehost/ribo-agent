//! TUI 首次信任确认 gate（19_ Step 6，设计§7.2）。
//!
//! 进入主循环前检测「项目声明了插件但代码未信任」：
//! - 按 `y`：写入 `trusted_projects.toml`（保留注释）→ 调用方重新加载配置
//!   再装配，插件正常加载；
//! - 按 `n` / 其它：不改信任记录 → 装配层跳过插件（Step 4 行为），
//!   继续启动。
//!
//! 确认在 raw mode 下读取**单个按键**，不进入 ratatui 主循环——避免与
//! 键盘 task / overlay 状态机耦合（它们服务于 REPL 会话，而非启动闸门）。

use std::{io::Write, path::Path};

use crossterm::{
  event::{Event, KeyCode, read},
  terminal::{disable_raw_mode, enable_raw_mode},
};

use ra_cli::config::trust::TrustedProjects;

/// 确认结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustDecision {
  /// 用户确认信任（已写入信任记录）。
  Granted,
  /// 用户拒绝 / 无插件需要确认（保持未信任）。
  Declined,
}

/// 是否需要弹确认：项目声明了插件但代码未信任。
pub fn needs_prompt(outcome: &ra_cli::config::LoadOutcome) -> bool {
  outcome.plugins.enabled
    && outcome
      .code_trust
      .as_ref()
      .is_some_and(|ctx| !ctx.is_project_code_trusted(&outcome.harness.root))
}

/// 弹确认并（按 y 时）写入信任记录。
///
/// `global_root` 为全局域目录（`trusted_projects.toml` 所在）；
/// `project_root` 为待授信项目路径。
///
/// # Errors
///
/// 终端 raw mode / 写文件失败 → 返回错误（调用方降级为「拒绝」并继续）。
pub fn prompt_and_grant(
  global_root: &Path,
  project_root: &Path,
  plugin_count: usize,
) -> anyhow::Result<TrustDecision> {
  let question =
    format!("项目声明了 {plugin_count} 个插件，是否信任并加载？[y/N] ");
  enable_raw_mode()?;
  let answer = loop {
    print!("{question}");
    std::io::stdout().flush()?;
    match read()? {
      Event::Key(key) => match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => break true,
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Enter => {
          break false;
        }
        _ => continue,
      },
      _ => continue,
    }
  };
  disable_raw_mode()?;
  if !answer {
    return Ok(TrustDecision::Declined);
  }

  let path = global_root.join("trusted_projects.toml");
  let mut trust = TrustedProjects::load(&path)?;
  trust.add_project(&project_root.to_string_lossy(), None);
  trust.save(&path)?;
  Ok(TrustDecision::Granted)
}
