//! ra-cli 入口：参数解析与启动（14 详细设计 §9/§13）。
//!
//! 薄壳：装配 → 日志落文件 → REPL 主循环；逻辑见 lib（`ribo_cli`
//! crate 内部模块）。

use std::{path::PathBuf, process::ExitCode};

use clap::Parser;

use ra_cli::app::{Repl, StdinLineSource};
use ra_cli::config::{LoadOutcome, resolve_config_path};
use ra_cli::logging::init_file_logging;
use ra_cli::render::TerminalRenderer;

/// ra-cli 启动参数。
#[derive(Debug, Parser)]
#[command(name = "ribo", version, about = "Ribo Agent 命令行交互层")]
pub struct CliArgs {
  /// 配置文件路径；未指定时自动探测 ribo.toml，缺失则以 mock 模式启动
  #[arg(short, long, value_name = "PATH")]
  pub config: Option<PathBuf>,

  /// 覆盖配置中的工作空间根目录（harness 层 `root`）
  #[arg(long, value_name = "PATH")]
  pub root: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> ExitCode {
  // ===== 阶段 1：参数与配置加载 =====
  let args = CliArgs::parse();
  // 未指定 --config 时探测默认文件；探测不到则传 None 走全默认 mock
  let config_path = resolve_config_path(args.config.as_deref());
  let outcome = LoadOutcome::load(config_path.as_deref(), args.root.as_deref());

  // ===== 阶段 2：日志落文件（stdout 只留对话渲染）=====
  // guard 持有到进程结束：非阻塞写入线程随 guard drop 停止，
  // 提前 drop 会丢尾部日志
  let _log_guard = match init_file_logging(&outcome.harness.root) {
    Ok(guard) => guard,
    Err(e) => {
      eprintln!("日志初始化失败：{e}");
      return ExitCode::FAILURE;
    }
  };

  // ===== 阶段 3：装配 =====
  let session = match ra_cli::config::assemble::assemble(&outcome) {
    Ok(session) => session,
    Err(e) => {
      eprintln!("装配失败：{e}");
      return ExitCode::FAILURE;
    }
  };

  // ===== 阶段 4：REPL 主循环 =====
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
