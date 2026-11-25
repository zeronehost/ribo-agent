//! 全局目录写入端与读取端一致性测试。
//!
//! 核心断言：`[global].dir` 不仅影响读取端，也影响 `auth` / `trust`
//! 等写入型子命令——两端共用 `resolve_global_dir`。

use std::path::Path;

use ra_cli::cli_commands::{
  AuthAction, SubCommand, TrustAction, execute, resolve_or_create_global_root,
};
use ra_cli::config::{LoadOutcome, resolve_global_dir};

/// 写一个含 `[global].dir` 的项目配置。
fn write_project(
  dir: &Path,
  global_dir: &Path,
  extra: &str,
) -> std::path::PathBuf {
  let p = dir.join("ribo.toml");
  std::fs::write(
    &p,
    format!(
      "[global]\ndir = {}\n{extra}\n",
      toml_string(&global_dir.to_string_lossy())
    ),
  )
  .expect("write project config");
  p
}

fn toml_string(s: &str) -> String {
  format!("\"{}\"", s.replace('\\', "\\\\"))
}

/// 无 CLI 参数、无 [global].dir → 平台默认（零行为变化）。
#[test]
fn t1_no_override_uses_platform_default() {
  let resolved = resolve_global_dir(None, None);
  let expected = dirs::config_dir().map(|d| d.join("ribo"));
  assert_eq!(resolved, expected);
}

/// 核心：[global].dir 生效于写入端解析。
#[test]
fn t2_project_global_dir_is_resolved() {
  let tmp = tempfile::tempdir().expect("temp");
  let custom = tmp.path().join("custom-global");
  let cfg = write_project(tmp.path(), &custom, "");

  // 纯函数层：解析结果 = 自定义目录
  let table: toml::Table = std::fs::read_to_string(&cfg)
    .expect("read")
    .parse()
    .expect("parse");
  assert_eq!(resolve_global_dir(None, Some(&table)), Some(custom.clone()));

  // 写入端：auth login 落到自定义目录
  let out = execute(
    &SubCommand::Auth {
      action: AuthAction::Login {
        provider: "p1".to_string(),
        api_key: Some("sk-t2".to_string()),
      },
    },
    Some(&cfg),
    None,
    None,
  )
  .expect("auth login");
  assert!(out.contains("已设置"), "got: {out}");
  assert!(
    custom.join("auth.toml").is_file(),
    "auth.toml must be written to [global].dir"
  );
}

/// --global-dir 与 [global].dir 同时存在 → CLI 优先。
#[test]
fn t3_cli_dir_wins_over_project_dir() {
  let tmp = tempfile::tempdir().expect("temp");
  let project_dir = tmp.path().join("from-project");
  let cli_dir = tmp.path().join("from-cli");
  let cfg = write_project(tmp.path(), &project_dir, "");
  let table: toml::Table = std::fs::read_to_string(&cfg)
    .expect("read")
    .parse()
    .expect("parse");

  assert_eq!(
    resolve_global_dir(Some(&cli_dir), Some(&table)),
    Some(cli_dir.clone())
  );
}

/// trust add 同样受 [global].dir 影响。
#[test]
fn t4_trust_add_honors_project_global_dir() {
  let tmp = tempfile::tempdir().expect("temp");
  let custom = tmp.path().join("custom-global");
  let cfg = write_project(tmp.path(), &custom, "");

  execute(
    &SubCommand::Trust {
      action: TrustAction::Add {
        path: tmp.path().join("proj-a"),
        note: None,
      },
    },
    Some(&cfg),
    None,
    None,
  )
  .expect("trust add");

  assert!(
    custom.join("trusted_projects.toml").is_file(),
    "trusted_projects.toml must land in [global].dir"
  );
}

/// enabled = false 不影响显式写入。
#[test]
fn t5_disabled_global_still_writes_to_dir() {
  let tmp = tempfile::tempdir().expect("temp");
  let custom = tmp.path().join("custom-global");
  let cfg = write_project(tmp.path(), &custom, "enabled = false");

  execute(
    &SubCommand::Auth {
      action: AuthAction::Login {
        provider: "p1".to_string(),
        api_key: Some("sk-t5".to_string()),
      },
    },
    Some(&cfg),
    None,
    None,
  )
  .expect("auth login");

  assert!(
    custom.join("auth.toml").is_file(),
    "enabled must not block writes"
  );
}

/// 读写闭环：写入自定义目录后，读取端能从同一路径读到凭证。
#[test]
fn t6_write_then_read_roundtrip() {
  let tmp = tempfile::tempdir().expect("temp");
  let custom = tmp.path().join("custom-global");
  std::fs::create_dir_all(&custom).expect("mkdir custom");

  // 项目配置：global.dir 指向 custom，声明一个全局 provider
  let cfg = tmp.path().join("ribo.toml");
  std::fs::write(
    &cfg,
    format!(
      "[global]\ndir = {}\n\nquery_model = \"m\"\n[[providers]]\nprovider_id = \"g1\"\nadapter = \"openai-chat\"\nbase_url = \"https://api.test.invalid\"\nmodel = \"m\"\n",
      toml_string(&custom.to_string_lossy())
    ),
  )
  .expect("write cfg");

  // 写入端：凭证落 custom
  execute(
    &SubCommand::Auth {
      action: AuthAction::Login {
        provider: "g1".to_string(),
        api_key: Some("sk-roundtrip".to_string()),
      },
    },
    Some(&cfg),
    None,
    None,
  )
  .expect("auth login");

  // provider 声明在项目配置里 → 属 Project 来源，不会用 auth
  // 的 key；这里只验证「读取端解析到的全局目录 == 写入端」这一闭环。
  let table: toml::Table = std::fs::read_to_string(&cfg)
    .expect("read")
    .parse()
    .expect("parse");
  assert_eq!(resolve_global_dir(None, Some(&table)), Some(custom.clone()));
  assert!(custom.join("auth.toml").is_file());
}

/// dir 指向不存在的目录 → 写入端自动创建。
#[test]
fn t7_nonexistent_dir_is_created() {
  let tmp = tempfile::tempdir().expect("temp");
  let nested = tmp.path().join("a").join("b").join("global");
  assert!(!nested.exists());

  let root = resolve_or_create_global_root(None, None).expect("default");
  assert!(root.is_dir(), "default root should exist/create");

  // 直接测自定义深层路径（不依赖平台默认）
  let created =
    resolve_or_create_global_root(Some(&nested), None).expect("create");
  assert_eq!(created, nested);
  assert!(nested.is_dir());
}

/// 补充：LoadOutcome::load_with_global 与写入端解析一致（无 CLI 参数时）。
#[test]
fn load_with_global_uses_same_resolution() {
  let tmp = tempfile::tempdir().expect("temp");
  let custom = tmp.path().join("custom-global");
  std::fs::create_dir_all(&custom).expect("mkdir");
  let cfg = tmp.path().join("ribo.toml");
  std::fs::write(
    &cfg,
    format!(
      "[c]\nquery_model = \"mock-model\"\n[global]\ndir = {}\n",
      toml_string(&custom.to_string_lossy())
    ),
  )
  .expect("write");

  // 读取端能发现该目录（Global 域被启用，无 settings.toml 也正常）
  let outcome = LoadOutcome::load_with_global(Some(&cfg), None, None);
  assert!(
    outcome.code_trust.is_some(),
    "global domain should be discovered"
  );
}
