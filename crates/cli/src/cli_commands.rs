//! 一次性 CLI 子命令。
//!
//! 子命令在 REPL 装配**之前**执行：读取配置 → 执行动作 → 打印 → 退出。
//! 无子命令时 main.rs 直接进 REPL（现状零破坏）。
//!
//! **首次使用即创建**：`auth login` / `trust add` 会创建全局域目录与
//! 对应 TOML（无 migrate 概念）。

use std::path::{Path, PathBuf};

use clap::Subcommand;

use ra_core::error::{RiboError, RiboResult, Severity};

use crate::config::credentials::AuthConfig;
use crate::config::trust::TrustedProjects;
use crate::config::{ConfigOrigin, LoadOutcome};

const DOMAIN: &str = "config";

fn cmd_error(code: &'static str, message: impl Into<String>) -> RiboError {
  RiboError::new(DOMAIN, code, message).with_severity(Severity::Error)
}

/// 一次性子命令（无子命令 → REPL）。
#[derive(Debug, Subcommand)]
pub enum SubCommand {
  /// 凭证管理：`auth login|status|logout <provider>`
  Auth {
    #[command(subcommand)]
    action: AuthAction,
  },
  /// 信任管理：`trust add|remove|list <path>`
  Trust {
    #[command(subcommand)]
    action: TrustAction,
  },
  /// 配置诊断：`config --show-origin | --explain`
  Config {
    /// 显示每个字段的来源域
    #[arg(long)]
    show_origin: bool,
    /// 显示字段来源与冲突解释（show-origin 的超集）
    #[arg(long)]
    explain: bool,
  },
  /// 插件概览：`plugins list`
  Plugins {
    #[command(subcommand)]
    action: PluginsAction,
  },
}

#[derive(Debug, Subcommand)]
pub enum AuthAction {
  /// 设置 provider 的 API key（交互提示从 stdin 读取）
  Login {
    /// provider id
    provider: String,
    /// API key（可选；省略时交互提示从 stdin 读取一行）
    #[arg(long, value_name = "KEY")]
    api_key: Option<String>,
  },
  /// 列出已配置 provider（不显示密钥）
  Status,
  /// 删除 provider
  Logout {
    /// provider id
    provider: String,
  },
}

#[derive(Debug, Subcommand)]
pub enum TrustAction {
  /// 授予项目代码信任
  Add {
    /// 项目路径
    path: PathBuf,
    /// 备注
    #[arg(long)]
    note: Option<String>,
  },
  /// 撤销项目代码信任
  Remove {
    /// 项目路径
    path: PathBuf,
  },
  /// 列出所有信任记录
  List,
}

#[derive(Debug, Subcommand)]
pub enum PluginsAction {
  /// 列出插件条目与只读挂载点（取代插件域）
  List,
}

/// 解析全局域根：`--global-dir` > 平台默认配置目录 + /ribo。
///
/// 与 `GlobalDomain::discover` 不同，本函数**会创建目录**（首次使用即
/// 创建）——子命令是用户显式操作，创建目录天经地义。
pub fn resolve_or_create_global_root(
  cli_dir: Option<&Path>,
  config_path: Option<&Path>,
) -> RiboResult<PathBuf> {
  // 与读取端共用同一解析真值（--global-dir > [global].dir > 默认）。
  let table = config_path
    .and_then(|p| std::fs::read_to_string(p).ok())
    .and_then(|text| text.parse::<toml::Table>().ok());
  let root = crate::config::resolve_global_dir(cli_dir, table.as_ref())
    .ok_or_else(|| {
      cmd_error(
        "global_dir_unavailable",
        "cannot determine platform config directory; use --global-dir",
      )
    })?;
  std::fs::create_dir_all(&root).map_err(|e| {
    cmd_error(
      "global_dir_unwritable",
      format!("cannot create global dir {}: {e}", root.display()),
    )
  })?;
  Ok(root)
}

/// 执行子命令，返回面向用户的输出文本（main.rs 打印后退出）。
///
/// # Errors
///
/// 文件 IO / TOML 解析失败 → `config.*` 错误码。
pub fn execute(
  command: &SubCommand,
  config_path: Option<&Path>,
  cli_root: Option<&Path>,
  global_dir: Option<&Path>,
) -> RiboResult<String> {
  match command {
    SubCommand::Auth { action } => {
      execute_auth(action, global_dir, config_path)
    }
    SubCommand::Trust { action } => {
      execute_trust(action, global_dir, config_path)
    }
    SubCommand::Config {
      show_origin,
      explain,
    } => execute_config(
      *show_origin || *explain,
      config_path,
      cli_root,
      global_dir,
    ),
    SubCommand::Plugins { action } => {
      execute_plugins(action, config_path, cli_root, global_dir)
    }
  }
}

/// auth 子命令。
fn execute_auth(
  action: &AuthAction,
  global_dir: Option<&Path>,
  config_path: Option<&Path>,
) -> RiboResult<String> {
  let root = resolve_or_create_global_root(global_dir, config_path)?;
  let path = root.join("auth.toml");
  let mut cfg = AuthConfig::load(&path)?;
  match action {
    AuthAction::Login { provider, api_key } => {
      let key = match api_key {
        Some(k) => k.clone(),
        None => {
          // 提示用标准库 print!（Windows 控制台 UTF-8 正常）；
          // 密钥用 rpassword 从 TTY 无回显读取（不回显到终端）。
          // rpassword::prompt_password 的内部输出通道在 Windows
          // 会把 UTF-8 中文按 GBK 解码导致乱码，故提示与读取分离。
          use std::io::Write;
          print!("请输入 provider `{provider}` 的 API key：");
          std::io::stdout()
            .flush()
            .map_err(|e| cmd_error("auth_key_read_failed", e.to_string()))?;
          let key = rpassword::read_password()
            .map_err(|e| cmd_error("auth_key_read_failed", e.to_string()))?;
          let trimmed = key.trim().to_string();
          if trimmed.is_empty() {
            return Err(cmd_error("auth_key_invalid", "empty API key"));
          }
          trimmed
        }
      };
      cfg.set_api_key(provider, &key);
      cfg.save(&path)?;
      Ok(format!("已设置 provider `{provider}` 的 API key"))
    }
    AuthAction::Status => {
      let ids = cfg.provider_ids();
      if ids.is_empty() {
        return Ok(format!(
          "未配置任何 provider（auth.toml: {}）",
          path.display()
        ));
      }
      let mut out = format!("已配置的 provider（{}）：\n", path.display());
      for id in ids {
        out.push_str(&format!("  - {id}\n"));
      }
      Ok(out)
    }
    AuthAction::Logout { provider } => {
      let removed = cfg.remove_provider(provider);
      if removed {
        cfg.save(&path)?;
        Ok(format!("已删除 provider `{provider}`"))
      } else {
        Ok(format!("provider `{provider}` 不存在，无需删除"))
      }
    }
  }
}

/// trust 子命令。
fn execute_trust(
  action: &TrustAction,
  global_dir: Option<&Path>,
  config_path: Option<&Path>,
) -> RiboResult<String> {
  let root = resolve_or_create_global_root(global_dir, config_path)?;
  let path = root.join("trusted_projects.toml");
  let mut trust = TrustedProjects::load(&path)?;
  match action {
    TrustAction::Add {
      path: project,
      note,
    } => {
      let key = project.to_string_lossy();
      trust.add_project(&key, note.as_deref());
      trust.save(&path)?;
      Ok(format!("已授予项目 `{key}` 代码信任"))
    }
    TrustAction::Remove { path: project } => {
      let key = project.to_string_lossy();
      let ok = trust.revoke_project(&key);
      if ok {
        trust.save(&path)?;
        Ok(format!("已撤销项目 `{key}` 的代码信任"))
      } else {
        Ok(format!("项目 `{key}` 无信任记录，无需撤销"))
      }
    }
    TrustAction::List => {
      let projects = trust.projects();
      if projects.is_empty() {
        return Ok(format!(
          "无信任记录（trusted_projects.toml: {}）",
          path.display()
        ));
      }
      let mut out = String::from("代码信任记录：\n");
      for entry in projects {
        out.push_str(&format!(
          "  - {} = {}\n",
          entry.path,
          if entry.code_trust {
            "trusted"
          } else {
            "denied"
          }
        ));
      }
      Ok(out)
    }
  }
}

/// config 子命令：来源标记 / 冲突解释。
fn execute_config(
  show_origin: bool,
  config_path: Option<&Path>,
  cli_root: Option<&Path>,
  global_dir: Option<&Path>,
) -> RiboResult<String> {
  if !show_origin {
    return Err(cmd_error(
      "config_arg_missing",
      "config requires --show-origin or --explain",
    ));
  }
  let outcome =
    LoadOutcome::load_with_global(config_path, cli_root, global_dir);
  let mut out = String::from("配置来源（未列出的字段为 default）：\n");
  for (field, origin) in outcome.origins.iter() {
    let label = match origin {
      ConfigOrigin::Default => "default",
      ConfigOrigin::Global => "global",
      ConfigOrigin::Project => "project",
      ConfigOrigin::Cli => "cli",
    };
    out.push_str(&format!("  {field} = {label}\n"));
  }
  Ok(out)
}

/// plugins 子命令：两栏列出条目与域。
fn execute_plugins(
  action: &PluginsAction,
  config_path: Option<&Path>,
  cli_root: Option<&Path>,
  global_dir: Option<&Path>,
) -> RiboResult<String> {
  match action {
    PluginsAction::List => {
      let outcome =
        LoadOutcome::load_with_global(config_path, cli_root, global_dir);
      let mut out = String::from("Plugin Entries:\n");
      for entry in &outcome.plugins.entries {
        out.push_str(&format!("  - {}  [project]\n", entry.path.display()));
      }
      if let Ok(mounts) = crate::config::assemble::validate_fs_mounts(&outcome)
        && !mounts.is_empty()
      {
        out.push_str("\nRead-only mounts:\n");
        for (name, root) in &mounts {
          out.push_str(&format!("  - {:<16} {}\n", name, root.display()));
        }
      }
      Ok(out)
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// 场景：config --show-origin 零配置显示 default（无字段记录时不报错）。
  #[test]
  fn show_origin_on_zero_config_returns_report() {
    let dir = tempfile::tempdir().expect("temp dir");
    let empty_cfg = dir.path().join("no-config.toml");
    let out = execute(
      &SubCommand::Config {
        show_origin: true,
        explain: false,
      },
      Some(&empty_cfg),
      None,
      None,
    )
    .expect("show-origin succeeds on zero config");
    assert!(out.contains("配置来源"));
  }

  /// 场景：plugins list 输出（Plugin Entries / Read-only mounts）。
  ///
  /// 挂载点从全局 settings.toml 声明（第 4 参数即 global_dir）。
  #[test]
  fn plugins_list_has_two_sections() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mount_src = tempfile::tempdir().expect("mount temp dir");
    let cfg = dir.path().join("ribo.toml");
    std::fs::write(&cfg, "").expect("write config");
    // TOML 字面量字符串（单引号），避免 Windows 路径反斜杠被当作转义序列。
    std::fs::write(
      dir.path().join("settings.toml"),
      format!("[fs.mounts]\ndataset = '{}'\n", mount_src.path().display()),
    )
    .expect("write global settings");
    let out = execute(
      &SubCommand::Plugins {
        action: PluginsAction::List,
      },
      Some(&cfg),
      None,
      Some(dir.path()),
    )
    .expect("plugins list");
    assert!(out.contains("Plugin Entries:"));
    assert!(out.contains("Read-only mounts:"));
    assert!(out.contains("dataset"));
  }

  /// 场景：全局域根创建（显式路径）。
  #[test]
  fn global_root_is_created_on_demand() {
    let dir = tempfile::tempdir().expect("temp dir");
    let target = dir.path().join("nested").join("ribo");
    let root = resolve_or_create_global_root(Some(&target), None)
      .expect("global root created");
    assert_eq!(root, target);
    assert!(target.is_dir());
  }

  /// 场景：trust add/list/remove 往返保留语义。
  #[test]
  fn trust_commands_roundtrip() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("trusted_projects.toml");
    let mut trust = TrustedProjects::load(&path).expect("missing → empty");
    trust.add_project("/proj", Some("note"));
    assert!(trust.is_code_trusted("/proj"));
    trust.save(&path).expect("save");
    let mut reloaded = TrustedProjects::load(&path).expect("reload");
    assert!(reloaded.is_code_trusted("/proj"));
    reloaded.revoke_project("/proj");
    assert!(!reloaded.is_code_trusted("/proj"));
  }
}
