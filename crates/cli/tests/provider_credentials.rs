//! Step 8 provider 凭证来源测试（todo/20_ 方案 T1–T12）。
//!
//! 覆盖：Global provider 从 auth.toml 补 key、Project provider 必须显式、
//! 按 id 合并（同名项目胜出 / 不同名共存）、$ENV 优先级、空 key 处理、
//! 域禁用、环境变量逃生舱口。

use ra_cli::config::{ConfigMode, LoadOutcome};

/// 写一个 provider 声明到 TOML 文本。
fn provider_toml(entries: &[(&str, &str, &str)]) -> String {
  // default 指向首个通道（无条目时悬空——属 mock 回退的测试场景）
  let mut s = String::new();
  if let Some((first, _, _)) = entries.first() {
    s.push_str(&format!(
      "[routing]\ndefault = {{ provider = \"{first}\", model = \"m\" }}\n"
    ));
  }
  for (id, _adapter, key_expr) in entries {
    // key_expr（api_key 行）必须在 [[providers.models]] 子表之前——
    // 子表后的键值对属于 models，不属于通道条目
    s.push_str(&format!(
      "\n[[providers]]\nprovider_id = \"{id}\"\nadapter = \"openai-chat\"\nbase_url = \"https://api.test.invalid\"\n{key_expr}\n\n  [[providers.models]]\n  id = \"m\"\n"
    ));
  }
  s
}

/// 建项目 + 全局目录，写配置，走 load_with_global。
fn setup(
  project_providers: &[(&str, &str, &str)],
  global_providers: &[(&str, &str, &str)],
  auth_keys: &[(&str, &str)],
) -> (tempfile::TempDir, LoadOutcome) {
  let dir = tempfile::tempdir().expect("temp dir");
  let global = dir.path().join("global");
  std::fs::create_dir_all(&global).expect("mkdir global");

  let project = dir.path().join("ribo.toml");
  std::fs::write(&project, provider_toml(project_providers))
    .expect("write project");
  if !global_providers.is_empty() {
    std::fs::write(
      global.join("settings.toml"),
      provider_toml(global_providers),
    )
    .expect("write global");
  }
  if !auth_keys.is_empty() {
    let mut auth = String::new();
    for (id, key) in auth_keys {
      auth.push_str(&format!("\n[providers.{id}]\napi_key = \"{key}\"\n"));
    }
    std::fs::write(global.join("auth.toml"), auth).expect("write auth");
  }

  let outcome =
    LoadOutcome::load_with_global(Some(&project), None, Some(&global));
  (dir, outcome)
}

fn ids(outcome: &LoadOutcome) -> Vec<&str> {
  outcome
    .provider
    .providers
    .iter()
    .map(|p| p.provider_id.as_str())
    .collect()
}

/// T1：Global provider 无 api_key + auth.toml 有 → 补充成功，非 mock。
#[test]
fn t1_global_provider_gets_key_from_auth() {
  let (_d, outcome) =
    setup(&[], &[("g1", "openai-chat", "")], &[("g1", "sk-from-auth")]);
  assert_eq!(mode_of(&outcome), "real", "global provider should load");
  assert_eq!(outcome.provider.providers[0].api_key, "sk-from-auth");
}

/// T2：Global provider 无 api_key + auth.toml 无 → 跳过 + warning。
#[test]
fn t2_global_provider_without_key_is_skipped() {
  let (_d, outcome) = setup(&[], &[("g1", "openai-chat", "")], &[]);
  assert!(outcome.provider.providers.is_empty());
  assert_eq!(mode_of(&outcome), "mock");
  assert!(outcome.warnings.iter().any(|w| w.contains("incomplete")));
}

/// T3（核心安全断言）：Project provider 无 api_key + auth.toml 有同名 key → 不补充。
#[test]
fn t3_project_provider_never_uses_auth_key() {
  let (_d, outcome) = setup(
    &[("p1", "openai-chat", "")],
    &[],
    &[("p1", "sk-should-not-be-used")],
  );
  assert!(
    outcome.provider.providers.is_empty(),
    "project provider lacking key must be skipped even if auth.toml has it"
  );
  assert_eq!(mode_of(&outcome), "mock");
}

/// T4：Project provider $ENV 占位 + 环境变量设置 → 正常展开。
#[test]
fn t4_project_provider_env_expansion() {
  #[allow(unsafe_code)]
  unsafe {
    std::env::set_var("RIBO_TEST_PROVIDER_KEY", "sk-from-env");
  }
  let (_d, outcome) = setup(
    &[("p1", "openai-chat", "api_key = \"$RIBO_TEST_PROVIDER_KEY\"")],
    &[],
    &[],
  );
  #[allow(unsafe_code)]
  unsafe {
    std::env::remove_var("RIBO_TEST_PROVIDER_KEY");
  }
  assert_eq!(mode_of(&outcome), "real");
  assert_eq!(outcome.provider.providers[0].api_key, "sk-from-env");
}

/// T5b/T5c：不同名 provider 两域共存，project 在前。
#[test]
fn t5b_merge_keeps_both_domains_in_order() {
  let (_d, outcome) = setup(
    &[("p1", "openai-chat", "api_key = \"k1\"")],
    &[("g1", "openai-chat", "api_key = \"k2\"")],
    &[],
  );
  assert_eq!(
    ids(&outcome),
    vec!["p1", "g1"],
    "project first, global appended"
  );
}

/// T5：同名 provider → 项目整条胜出（不继承全局 base_url/key）。
#[test]
fn t5_same_id_project_wins_whole_entry() {
  let dir = tempfile::tempdir().expect("temp dir");
  let global = dir.path().join("global");
  std::fs::create_dir_all(&global).expect("mkdir global");
  let project = dir.path().join("ribo.toml");
  // project 声明同名 provider，自带 key
  std::fs::write(
    &project,
    provider_toml(&[("same", "openai-chat", "api_key = \"project-key\"")]),
  )
  .expect("write project");
  // global 声明同名 provider，不同 base_url + auth 有 key
  std::fs::write(
    global.join("settings.toml"),
    "[routing]\ndefault = { provider = \"same\", model = \"m\" }\n\n[[providers]]\nprovider_id = \"same\"\nadapter = \"openai-chat\"\nbase_url = \"https://global.test.invalid\"\n\n  [[providers.models]]\n  id = \"m\"\n",
  )
  .expect("write global");
  std::fs::write(
    global.join("auth.toml"),
    "[providers.same]\napi_key = \"auth-key\"\n",
  )
  .expect("write auth");

  let outcome =
    LoadOutcome::load_with_global(Some(&project), None, Some(&global));
  assert_eq!(outcome.provider.providers.len(), 1);
  let p = &outcome.provider.providers[0];
  assert_eq!(
    p.api_key, "project-key",
    "project key wins, not auth/global"
  );
  assert_eq!(
    p.base_url, "https://api.test.invalid",
    "project base_url, not inherited"
  );
}

/// T12：auth.toml 里的 key 恰为 "$ENV_NAME" → 原样使用，不展开。
#[test]
fn t12_auth_key_literal_dollar_is_not_expanded() {
  let (_d, outcome) = setup(
    &[],
    &[("g1", "openai-chat", "")],
    &[("g1", "$RIBO_SHOULD_NOT_EXPAND")],
  );
  assert_eq!(mode_of(&outcome), "real");
  assert_eq!(
    outcome.provider.providers[0].api_key, "$RIBO_SHOULD_NOT_EXPAND",
    "auth.toml value must be used verbatim (D5/T12)"
  );
}

/// T11：全空白 api_key 视为未配置。
#[test]
fn t11_whitespace_key_is_unconfigured() {
  let (_d, outcome) = setup(
    &[],
    &[("g1", "openai-chat", "api_key = \"   \"")],
    &[("g1", "sk-from-auth")],
  );
  assert_eq!(mode_of(&outcome), "real");
  assert_eq!(outcome.provider.providers[0].api_key, "sk-from-auth");
}

/// T9：global.enabled = false → global provider 不加载。
#[test]
fn t9_disabled_global_skips_its_providers() {
  let dir = tempfile::tempdir().expect("temp dir");
  let global = dir.path().join("global");
  std::fs::create_dir_all(&global).expect("mkdir global");
  let project = dir.path().join("ribo.toml");
  std::fs::write(
    &project,
    "[global]\nenabled = false\n\n[routing]\ndefault = { provider = \"p1\", model = \"m\" }\n\n[[providers]]\nprovider_id = \"p1\"\nadapter = \"openai-chat\"\nbase_url = \"https://api.test.invalid\"\n\n  [[providers.models]]\n  id = \"m\"\n",
  )
  .expect("write project");
  std::fs::write(
    global.join("settings.toml"),
    provider_toml(&[("g1", "openai-chat", "api_key = \"k\"")]),
  )
  .expect("write global");

  let outcome =
    LoadOutcome::load_with_global(Some(&project), None, Some(&global));
  assert!(
    outcome.provider.providers.is_empty(),
    "disabled global domain must not load its providers (T9)"
  );
}

fn mode_of(outcome: &LoadOutcome) -> &'static str {
  match outcome.mode {
    ConfigMode::Real => "real",
    ConfigMode::Mock => "mock",
  }
}
