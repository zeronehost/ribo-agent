//! ra-cli 入口：参数解析与启动。
//!
//! 薄壳：装配 → 日志落文件 → REPL 主循环；逻辑见 lib（`ribo_cli`
//! crate 内部模块）。

use std::{path::PathBuf, process::ExitCode};

use clap::Parser;
use ra_cli::cli_commands::{SubCommand, execute};

use ra_cli::app::{Repl, StdinLineSource};
use ra_cli::config::{LoadOutcome, resolve_config_path};
use ra_cli::logging::init_file_logging_with_fallback;
use ra_cli::render::TerminalRenderer;

/// ra-cli 启动参数。
#[derive(Debug, Parser)]
#[command(name = "ribo", version, about = "Ribo Agent 命令行交互层")]
pub struct CliArgs {
  /// 配置文件路径；未指定时自动探测 ribo.toml
  #[arg(short, long, value_name = "PATH")]
  pub config: Option<PathBuf>,

  /// 覆盖配置中的工作空间根目录（harness 层 `root`）
  #[arg(long, value_name = "PATH")]
  pub root: Option<PathBuf>,

  /// 覆盖全局域目录（默认平台配置目录 + `/ribo`）
  #[arg(long, value_name = "PATH")]
  pub global_dir: Option<PathBuf>,

  /// 允许跨源重复的 plugin_id（首声明胜出；默认拒绝并启动致命）
  #[arg(long)]
  pub allow_duplicate_plugin_id: bool,

  /// 一次性子命令（无子命令 → 进入 REPL）
  #[command(subcommand)]
  pub command: Option<SubCommand>,
}

#[tokio::main]
async fn main() -> ExitCode {
  // 参数与配置加载
  let args = CliArgs::parse();
  // 未指定 --config 时向上探测；显式指定但文件不存在 → 报错退出
  let config_path = match resolve_config_path(args.config.as_deref()) {
    Ok(p) => p,
    Err(e) => {
      eprintln!("配置路径错误：{e}");
      return ExitCode::FAILURE;
    }
  };

  // 一次性子命令（装配前执行、打印后退出）
  if let Some(command) = &args.command {
    match execute(
      command,
      config_path.as_deref(),
      args.root.as_deref(),
      args.global_dir.as_deref(),
    ) {
      Ok(output) => {
        println!("{output}");
        return ExitCode::SUCCESS;
      }
      Err(e) => {
        eprintln!("命令失败：{e}");
        return ExitCode::FAILURE;
      }
    }
  }

  let outcome = LoadOutcome::load_with_global(
    config_path.as_deref(),
    args.root.as_deref(),
    args.global_dir.as_deref(),
  );
  // release 构建下三类配置异常为致命错误
  if let Some(err) = &outcome.fatal_error {
    eprintln!("配置错误：{err}");
    return ExitCode::FAILURE;
  }

  // 日志落文件（stdout 只留对话渲染）
  // guard 持有到进程结束：非阻塞写入线程随 guard drop 停止，
  // 提前 drop 会丢尾部日志
  // 全局域启用 → 日志写全局目录；否则写项目目录（失败自动回退）
  let _log_guard = match init_file_logging_with_fallback(
    &outcome.log_root(),
    &outcome.harness.root,
  ) {
    Ok(guard) => guard,
    Err(e) => {
      eprintln!("日志初始化失败：{e}");
      return ExitCode::FAILURE;
    }
  };
  // 启动摘要补记到日志文件：订阅器此刻才就绪，此前配置加载
  // 产生的 tracing 记录不会落盘，故 mode / warnings 在此集中补记。
  tracing::info!(
    mode = ?outcome.mode,
    providers = outcome.provider.providers.len(),
    warnings = outcome.warnings.len(),
    "config loaded"
  );
  for w in &outcome.warnings {
    tracing::warn!(warning = %w, "config warning");
  }

  // 装配
  let options = ra_cli::config::assemble::AssembleOptions {
    allow_duplicate_plugin_id: args.allow_duplicate_plugin_id,
  };
  let session = match ra_cli::config::assemble::assemble_with_options(
    &outcome,
    Vec::new(),
    options,
  ) {
    Ok(session) => session,
    Err(e) => {
      tracing::error!(code = %e.code(), "assemble failed");
      eprintln!("装配失败：{e}");
      return ExitCode::FAILURE;
    }
  };

  // REPL 主循环
  let repl = Repl::new(
    StdinLineSource::new(),
    session,
    Box::new(TerminalRenderer::new()),
  );
  match repl.run().await {
    Ok(()) => ExitCode::SUCCESS,
    Err(e) => {
      eprintln!("错误：{e}");
      ExitCode::FAILURE
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// 场景：无子命令 → command=None（进 REPL，零破坏）。
  #[test]
  fn no_subcommand_keeps_repl_default() {
    let args = CliArgs::try_parse_from(["ribo"]).expect("parse");
    assert!(args.command.is_none());
  }

  /// 场景：各子命令可解析（clap 防漂移）。
  #[test]
  fn all_subcommands_parse() {
    let cases = [
      (vec!["ribo", "auth", "status"], "auth status"),
      (vec!["ribo", "auth", "login", "openai"], "auth login"),
      (vec!["ribo", "auth", "logout", "openai"], "auth logout"),
      (vec!["ribo", "trust", "add", "/proj"], "trust add"),
      (vec!["ribo", "trust", "remove", "/proj"], "trust remove"),
      (vec!["ribo", "trust", "list"], "trust list"),
      (
        vec!["ribo", "config", "--show-origin"],
        "config show-origin",
      ),
      (vec!["ribo", "config", "--explain"], "config explain"),
      (vec!["ribo", "plugins", "list"], "plugins list"),
    ];
    for (argv, label) in cases {
      let args = CliArgs::try_parse_from(argv)
        .unwrap_or_else(|e| panic!("{label}: {e}"));
      assert!(args.command.is_some(), "{label} 应有子命令");
    }
    // 结合全局开关
    let args = CliArgs::try_parse_from([
      "ribo",
      "--allow-duplicate-plugin-id",
      "plugins",
      "list",
    ])
    .expect("global flag before subcommand");
    assert!(args.allow_duplicate_plugin_id);
    assert!(args.command.is_some());
  }
}
