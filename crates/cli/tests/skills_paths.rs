//! 技能路径纪律集成测试。
//!
//! 覆盖：旧 `<root>/skills/` 不被扫描（硬切换，仅读 `.ribo/skills/`）；
//! 同名技能 Project > Global（SkillsLoader roots 顺序首个胜出）。

use std::path::Path;

use ra_cli::config::assemble::assemble_with_steps;
use ra_cli::config::code_trust::CodeTrustContext;
use ra_cli::config::trust::TrustedProjects;
use ra_cli::config::{ConfigMode, HarnessLayerSchema, LoadOutcome};

fn outcome_with(
  root: &Path,
  code_trust: Option<CodeTrustContext>,
) -> LoadOutcome {
  LoadOutcome {
    provider: Default::default(),
    retry: Default::default(),
    compression: Default::default(),
    session: Default::default(),
    tools: Default::default(),
    plugins: Default::default(),
    harness: HarnessLayerSchema {
      root: root.to_path_buf(),
      ..Default::default()
    },
    mode: ConfigMode::Mock,
    warnings: Vec::new(),
    origins: Default::default(),
    code_trust,
    fs_mounts: Default::default(),
    fatal_error: None,
  }
}

fn write_skill(dir: &Path, name: &str, description: &str) {
  std::fs::create_dir_all(dir).expect("mkdir skill dir");
  std::fs::write(
    dir.join("SKILL.md"),
    format!("---\nname: {name}\ndescription: {description}\n---\nbody"),
  )
  .expect("write SKILL.md");
}

/// 场景：`.ribo/skills/` 与旧 `<root>/skills/` 并存 → 只读新路径，
/// 旧布局技能不识别（硬切换，无迁移无兼容）。
#[test]
fn t2_old_skills_dir_is_not_scanned() {
  let dir = tempfile::tempdir().expect("temp dir");
  let root = dir.path();
  // 旧布局技能（legacy）
  write_skill(&root.join("skills/legacy"), "legacy", "old layout");
  // 新布局技能（current）
  write_skill(&root.join(".ribo/skills/current"), "current", "new layout");

  let session = assemble_with_steps(&outcome_with(root, None), Vec::new())
    .expect("assemble");
  assert!(
    session.skills.contains(&"current".to_string()),
    "skills: {:?}",
    session.skills
  );
  assert!(
    !session.skills.contains(&"legacy".to_string()),
    "old-layout skill must not be scanned, got: {:?}",
    session.skills
  );
}

/// 场景：全局域与项目 `.ribo/skills/` 有同名技能 → 项目根胜出
///（first root wins，Project 在 Global 之前）。
#[test]
fn t12_project_skill_wins_over_global() {
  let dir = tempfile::tempdir().expect("temp dir");
  let root = dir.path();
  let global = dir.path().join("global");
  write_skill(&global.join("skills/shared"), "shared", "global version");
  write_skill(
    &root.join(".ribo/skills/shared"),
    "shared",
    "project version",
  );

  let ctx =
    CodeTrustContext::new(TrustedProjects::empty()).with_global_root(&global);
  let session = assemble_with_steps(&outcome_with(root, Some(ctx)), Vec::new())
    .expect("assemble");

  let idx = session
    .skills
    .iter()
    .position(|s| s == "shared")
    .expect("shared skill loaded");
  assert_eq!(session.skill_origins[idx], "project");
  // 同名去重：只保留一个条目
  assert_eq!(
    session.skills.iter().filter(|s| **s == "shared").count(),
    1,
    "skills: {:?}",
    session.skills
  );
}
