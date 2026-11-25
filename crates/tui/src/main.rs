//! ra-tui 入口：参数解析 → 配置/装配 → raw mode → 主循环 → 退出码。
//!
//!
//! 启动失败（配置 / 装配）在 raw mode 进入**之前**用 stderr 报错退出
//!；渲染路径禁止 `println!`，tracing 走文件通道。
//!
//! **Tokio 上下文**：入口用 `#[tokio::main]` 让整个 main 体运行在
//! multi-thread runtime 中。键盘 task 的 `spawn_blocking` 经
//! `Handle::current()` 取 runtime，若在 runtime 之外启动会 panic
//! 「there is no reactor running」；若改回手工构建 `Runtime`，必须把
//! 构建与 `enter()` 提前到启动键盘 task **之前**。

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use ra_cli::config::assemble::{
  AssembleOptions, CliSession, assemble_with_options,
};
use ra_cli::config::{LoadOutcome, resolve_config_path};

mod app;
mod input;
mod lifecycle;
mod render_helpers;
mod state;
mod trust_gate;
mod turn;
mod view;

use crate::app::App;
use crate::lifecycle::RawModeGuard;

/// ra-tui 启动参数。
#[derive(Debug, Parser)]
#[command(name = "ribo-tui", version, about = "Ribo Agent 终端 TUI")]
pub struct TuiArgs {
  /// 配置文件路径；未指定时自动探测 ribo.toml，缺失则以 mock 模式启动
  ///（与 ra-cli 共用 `resolve_config_path`）。
  #[arg(short, long, value_name = "PATH")]
  pub config: Option<PathBuf>,

  /// 覆盖配置中的工作空间根目录（harness 层 `root`）。
  #[arg(long, value_name = "PATH")]
  pub root: Option<PathBuf>,

  /// 覆盖全局域目录（默认平台配置目录 + `/ribo`）。
  #[arg(long, value_name = "PATH")]
  pub global_dir: Option<PathBuf>,

  /// 允许跨源重复的 plugin_id（首声明胜出；默认拒绝并启动致命）。
  #[arg(long)]
  pub allow_duplicate_plugin_id: bool,
}

/// ra-tui 入口：整个函数体运行在 `#[tokio::main]` 的 runtime 上下文中。
#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
  // 参数与配置加载（raw mode 之前，失败 stderr 退出）
  let args = TuiArgs::parse();
  // 与 ra-cli 同规则：未指定 --config 时向上探测；显式缺失 → 报错退出
  let config_path = match resolve_config_path(args.config.as_deref()) {
    Ok(p) => p,
    Err(e) => {
      eprintln!("配置路径错误：{e}");
      return ExitCode::FAILURE;
    }
  };
  let mut outcome = LoadOutcome::load_with_global(
    config_path.as_deref(),
    args.root.as_deref(),
    args.global_dir.as_deref(),
  );
  // release 构建下三类配置异常为致命错误
  if let Some(err) = &outcome.fatal_error {
    eprintln!("配置错误：{err}");
    return ExitCode::FAILURE;
  }
  let root = outcome.harness.root.clone();

  // 日志落文件（stdout 由 TUI 全屏占用）
  // 全局域启用 → 日志写全局目录；否则写项目目录（失败自动回退）
  let log_guard = match ra_cli::logging::init_file_logging_with_fallback(
    &outcome.log_root(),
    &root,
  ) {
    Ok(guard) => guard,
    Err(err) => {
      eprintln!("日志初始化失败：{err}");
      return ExitCode::FAILURE;
    }
  };
  // 启动摘要补记：订阅器此刻才就绪，配置加载时的 tracing 不会落盘
  tracing::info!(
    mode = ?outcome.mode,
    providers = outcome.provider.providers.len(),
    warnings = outcome.warnings.len(),
    "config loaded"
  );
  for w in &outcome.warnings {
    tracing::warn!(warning = %w, "config warning");
  }

  // Ctrl-C 超时（[tui] 层，读实际生效的配置文件）
  // 必须基于解析后的路径：未指定 --config 时 args.config 为 None，
  // 但默认 ribo.toml 可能已被探测并加载。
  let config_text = config_path
    .as_deref()
    .and_then(|path| std::fs::read_to_string(path).ok());
  let ctrl_c_timeout = lifecycle::parse_ctrl_c_timeout(config_text.as_deref());

  // 首次信任确认
  // 项目有插件但未 code-trust 时，raw mode 下读单个 y/n：
  // y → 写 trusted_projects.toml 并重载配置（插件正常加载）；
  // n → 保持未信任，装配层跳过插件（信任闸门拒绝），继续启动。
  if trust_gate::needs_prompt(&outcome) {
    let plugin_count = outcome.plugins.entries.len();
    let global_root = outcome
      .code_trust
      .as_ref()
      .and_then(|ctx| ctx.global_root())
      .map(std::path::Path::to_path_buf);
    if let Some(groot) = global_root {
      match trust_gate::prompt_and_grant(
        &groot,
        &outcome.harness.root,
        plugin_count,
      ) {
        Ok(trust_gate::TrustDecision::Granted) => {
          // 重新加载：让 code_trust 上下文看到刚写入的信任记录
          let reloaded = LoadOutcome::load_with_global(
            config_path.as_deref(),
            args.root.as_deref(),
            args.global_dir.as_deref(),
          );
          outcome = reloaded;
        }
        Ok(trust_gate::TrustDecision::Declined) => {
          eprintln!("已跳过插件加载（项目未授信）。");
        }
        Err(err) => {
          eprintln!("信任确认失败（{err}），本次跳过插件加载。");
        }
      }
    }
  }

  // 装配（失败在 guard 前 stderr 退出）
  let session: CliSession = match assemble_with_options(
    &outcome,
    Vec::new(),
    AssembleOptions {
      allow_duplicate_plugin_id: args.allow_duplicate_plugin_id,
    },
  ) {
    Ok(session) => session,
    Err(err) => {
      tracing::error!(code = %err.code(), "assemble failed");
      eprintln!("装配失败：{err}");
      return ExitCode::FAILURE;
    }
  };

  // raw mode + panic hook + 主循环
  let _log_guard = log_guard; // guard 持有至进程结束（非阻塞 writer）
  let _raw = match RawModeGuard::enter() {
    Ok(guard) => guard,
    Err(err) => {
      eprintln!("终端初始化失败：{err}");
      return ExitCode::FAILURE;
    }
  };
  lifecycle::install_panic_hook();

  // spawn_blocking 需要 Tokio 上下文：此处已在 #[tokio::main] 的 runtime 内。
  // 若改为手工 Runtime::block_on，必须先构建并 enter() 到本行之前。
  let (key_rx, keyboard_task) = crate::app::spawn_keyboard();
  let terminal = match ratatui::Terminal::new(
    ratatui::backend::CrosstermBackend::new(std::io::stdout()),
  ) {
    Ok(terminal) => terminal,
    Err(err) => {
      eprintln!("终端后端初始化失败：{err}");
      return ExitCode::FAILURE;
    }
  };

  let mut app = App::new(session, key_rx);
  app.state.ctrl_c_timeout = ctrl_c_timeout;
  // runtime 由 #[tokio::main] 提供，直接 await 主循环即可
  let run_result = app.run(terminal).await;

  // 键盘 task 在主循环退出后由 blocking_send 失败自然结束
  let _ = keyboard_task.is_finished();

  match run_result {
    Ok(()) => ExitCode::SUCCESS,
    Err(err) => {
      // run 内部错误路径已由 panic hook / guard 兜底恢复终端
      eprintln!("TUI 运行错误：{err:#}");
      ExitCode::FAILURE
    }
  }
}
