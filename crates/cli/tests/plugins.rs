//! 插件装配集成测试（B0：阶段 6 接入层）。
//!
//! 覆盖：配置层解析（CFG）、装配契约（默认关闭零行为变化、未触碰
//! wasmtime 初始化）、启动致命失败语义（缺文件 / 路径逃逸 / 目录缺失）。
//! 真实 wasm 的 Agent 全链路见 plugin-demo e2e（独立子 crate，自带
//! guest 构建与专用 CI，不进 workspace 主路径）。

use std::path::{Path, PathBuf};

use ra_cli::config::assemble::assemble_with_steps;
use ra_cli::config::{
  ConfigMode, LoadOutcome, PluginAclSchema, PluginEntrySchema,
  PluginVerifySchema, PluginsSchema,
};

/// 最小装配输入：全部层取默认，仅指定 plugins 层与 harness.root。
fn outcome_with(root: &Path, plugins: PluginsSchema) -> LoadOutcome {
  LoadOutcome {
    provider: Default::default(),
    retry: Default::default(),
    compression: Default::default(),
    session: Default::default(),
    tools: Default::default(),
    plugins,
    harness: ra_cli::config::HarnessLayerSchema {
      root: root.to_path_buf(),
      ..Default::default()
    },
    mode: ConfigMode::Mock,
    warnings: Vec::new(),
  }
}

/// 场景：plugins 层缺省（`enabled = false`）时的装配契约。
///
/// 预期：装配成功、注册表为 `None`、无已加载插件——现有配置与
/// TUI 测试零行为变化，且不触碰 wasmtime 初始化。
#[test]
fn plugins_disabled_by_default_touches_nothing() {
  let dir = tempfile::tempdir().expect("temp dir");
  let outcome = outcome_with(dir.path(), PluginsSchema::default());
  let session = assemble_with_steps(&outcome, Vec::new()).expect("assemble");
  assert!(session.plugin_registry.is_none());
  assert!(session.loaded_plugins.is_empty());
}

/// 场景：plugins 层 TOML 解析（entries 清单 + ACL + config）。
///
/// 预期：字段逐项落位；无 provider 仍为 mock 模式，但 plugins 层
/// 照常解析（层间零交叉）。
#[test]
fn plugins_layer_parses_entries_and_acl_from_toml() {
  let dir = tempfile::tempdir().expect("temp dir");
  let path = dir.path().join("ribo.toml");
  std::fs::write(
    &path,
    r#"
[plugins]
enabled = true
roots = ["wasm-dir"]

[[plugins.entries]]
path = "plugins/demo.wasm"

[plugins.entries.acl]
log = true
storage = true
http = true
http_hosts = ["api.example.com"]
memory_limit_bytes = 16777216

[plugins.entries.config]
greeting_prefix = "你好"
"#,
  )
  .expect("write config");
  let outcome = LoadOutcome::load(Some(&path), None);
  assert_eq!(outcome.mode, ConfigMode::Mock);
  assert!(outcome.plugins.enabled);
  assert_eq!(outcome.plugins.roots.len(), 1);
  assert_eq!(outcome.plugins.roots[0], Path::new("wasm-dir"));
  let entry = &outcome.plugins.entries[0];
  assert_eq!(entry.path, Path::new("plugins/demo.wasm"));
  assert!(entry.acl.log && entry.acl.storage && entry.acl.http);
  assert_eq!(entry.acl.http_hosts.len(), 1);
  assert_eq!(entry.acl.http_hosts[0], "api.example.com");
  assert_eq!(entry.acl.memory_limit_bytes, Some(16 * 1024 * 1024));
  assert_eq!(
    entry
      .config
      .as_ref()
      .and_then(|c| c.get("greeting_prefix"))
      .and_then(|v| v.as_str()),
    Some("你好")
  );
}

/// 场景：`enabled = true` 但清单文件缺失 → 启动致命（禁止半装配）。
///
/// 预期：`assemble` 返回 `config.plugin_file_missing`。
#[test]
fn plugins_enabled_with_missing_file_is_fatal() {
  let dir = tempfile::tempdir().expect("temp dir");
  let outcome = outcome_with(
    dir.path(),
    PluginsSchema {
      enabled: true,
      roots: Vec::new(),
      entries: vec![PluginEntrySchema {
        path: PathBuf::from("missing.wasm"),
        config: None,
        acl: PluginAclSchema::default(),
        manifest: None,
        provenance: None,
      }],
      verify: PluginVerifySchema::default(),
    },
  );
  // CliSession 未实现 Debug，无法用 expect_err（需打印 Ok 值）
  let err = match assemble_with_steps(&outcome, Vec::new()) {
    Err(err) => err,
    Ok(_) => panic!("missing wasm must be fatal"),
  };
  assert_eq!(err.code(), "plugin_file_missing", "got: {}", err.code());
}

/// 场景：清单路径含 `..` 逃逸组件 → 启动致命（与 `tools.fs_root`
/// 同一沙箱纪律：插件 wasm 恒位于 harness.root 之下）。
#[test]
fn plugins_reject_escaping_entry_path() {
  let dir = tempfile::tempdir().expect("temp dir");
  let outcome = outcome_with(
    dir.path(),
    PluginsSchema {
      enabled: true,
      roots: Vec::new(),
      entries: vec![PluginEntrySchema {
        path: PathBuf::from("../escape.wasm"),
        config: None,
        acl: PluginAclSchema::default(),
        manifest: None,
        provenance: None,
      }],
      verify: PluginVerifySchema::default(),
    },
  );
  let err = match assemble_with_steps(&outcome, Vec::new()) {
    Err(err) => err,
    Ok(_) => panic!("escaping path must be fatal"),
  };
  assert_eq!(err.code(), "plugin_load_failed", "got: {}", err.code());
}

/// Windows 目录联接（reparse point）：无需特权即可把一个目录「挂载」
/// 到别处——逃逸语义与符号链接等价，但权限门槛更低。
#[cfg(windows)]
fn junction(target: &Path, link: &Path) -> bool {
  std::process::Command::new("cmd")
    .args(["/C", "mklink", "/J"])
    .arg(link)
    .arg(target)
    .output()
    .map(|out| out.status.success())
    .unwrap_or(false)
}

/// 在 `link` 处建一个指向 `target` 的链接目录。
///
/// Unix 用符号链接；Windows 优先目录联接（免特权），失败再退目录符号
/// 链接（需开发者模式）。返回 `false` 表示宿主不支持创建链接。
fn link_dir(target: &Path, link: &Path) -> bool {
  #[cfg(unix)]
  {
    std::os::unix::fs::symlink(target, link).is_ok()
  }
  #[cfg(windows)]
  {
    junction(target, link)
      || std::os::windows::fs::symlink_dir(target, link).is_ok()
  }
}

/// 构造「沙箱内 + 沙箱外」双目录布局：`inside` 为 harness.root，
/// `outside` 与之平级（`..` 逃逸目标）。
fn sandbox_layout() -> (tempfile::TempDir, PathBuf, PathBuf) {
  let tmp = tempfile::tempdir().expect("temp dir");
  let inside = tmp.path().join("inside");
  let outside = tmp.path().join("outside");
  std::fs::create_dir_all(&inside).expect("mkdir inside");
  std::fs::create_dir_all(&outside).expect("mkdir outside");
  // 沙箱外的 wasm：逃逸一旦得手，它就会被扫进插件清单
  std::fs::write(outside.join("evil.wasm"), b"\0asm").expect("write wasm");
  (tmp, inside, outside)
}

/// 场景：`roots` 含 `..` 逃逸组件 → 启动致命。
///
/// 预期：装配期即拒绝并返回 `config.plugin_load_failed`——扫描目录恒位于
/// harness.root 之下（`Path::join` 遇 `..` 会解析到沙箱外）。
#[test]
fn plugins_reject_escaping_root_path() {
  let (_tmp, inside, _outside) = sandbox_layout();
  let outcome = outcome_with(
    &inside,
    PluginsSchema {
      enabled: true,
      // `../outside` 相对 harness.root（inside）逃逸到沙箱外
      roots: vec![PathBuf::from("../outside")],
      entries: Vec::new(),
      verify: PluginVerifySchema::default(),
    },
  );
  let err = match assemble_with_steps(&outcome, Vec::new()) {
    Err(err) => err,
    Ok(_) => panic!("escaping root must be fatal"),
  };
  assert_eq!(err.code(), "plugin_load_failed", "got: {}", err.code());
}

/// 场景：`roots` 为绝对路径 → 启动致命。
///
/// 预期：`config.plugin_load_failed`——`Path::join` 遇绝对路径会直接替换
/// 基址，扫描目录完全脱离 harness.root。
#[test]
fn plugins_reject_absolute_root_path() {
  let (_tmp, inside, outside) = sandbox_layout();
  let outcome = outcome_with(
    &inside,
    PluginsSchema {
      enabled: true,
      roots: vec![outside],
      entries: Vec::new(),
      verify: PluginVerifySchema::default(),
    },
  );
  let err = match assemble_with_steps(&outcome, Vec::new()) {
    Err(err) => err,
    Ok(_) => panic!("absolute root must be fatal"),
  };
  assert_eq!(err.code(), "plugin_load_failed", "got: {}", err.code());
}

/// 场景：扫描目录内的链接（符号链接 / 目录联接）指向 harness.root 之外
/// → 启动致命。
///
/// 预期：`config.plugin_load_failed`——词法合法（相对路径、无 `..`）但
/// 规范化后落在沙箱之外，属链接逃逸。
#[test]
fn plugins_reject_symlinked_escape_from_root_scan() {
  let (_tmp, inside, outside) = sandbox_layout();
  if !link_dir(&outside, &inside.join("plugins")) {
    eprintln!("skip: link creation not permitted on this host");
    return;
  }
  let outcome = outcome_with(
    &inside,
    PluginsSchema {
      enabled: true,
      roots: vec![PathBuf::from("plugins")],
      entries: Vec::new(),
      verify: PluginVerifySchema::default(),
    },
  );
  let err = match assemble_with_steps(&outcome, Vec::new()) {
    Err(err) => err,
    Ok(_) => panic!("symlinked escape must be fatal"),
  };
  assert_eq!(err.code(), "plugin_load_failed", "got: {}", err.code());
}

/// 场景：显式清单项经链接指向沙箱外 → 启动致命。
///
/// 预期：`config.plugin_load_failed`——词法校验放行但规范化后越界，
/// 与 roots 扫描走同一归属判定。
#[test]
fn plugins_reject_symlinked_escape_from_entries() {
  let (_tmp, inside, outside) = sandbox_layout();
  if !link_dir(&outside, &inside.join("plugins")) {
    eprintln!("skip: link creation not permitted on this host");
    return;
  }
  let outcome = outcome_with(
    &inside,
    PluginsSchema {
      enabled: true,
      roots: Vec::new(),
      entries: vec![PluginEntrySchema {
        path: PathBuf::from("plugins/evil.wasm"),
        config: None,
        acl: PluginAclSchema::default(),
        manifest: None,
        provenance: None,
      }],
      verify: PluginVerifySchema::default(),
    },
  );
  let err = match assemble_with_steps(&outcome, Vec::new()) {
    Err(err) => err,
    Ok(_) => panic!("symlinked escape must be fatal"),
  };
  assert_eq!(err.code(), "plugin_load_failed", "got: {}", err.code());
}

/// 场景：harness.root 本身是绝对路径（`--root` 覆盖的常见形态）→
/// 不误判为逃逸。
///
/// 预期：roots 下的合法相对子目录照常扫描，装配成功——归属判定以
/// harness.root 为基准，不以「路径是否绝对」为准。
#[test]
fn plugins_accept_absolute_harness_root() {
  let (_tmp, inside, _outside) = sandbox_layout();
  std::fs::create_dir_all(inside.join("plugins")).expect("mkdir plugins");
  let outcome = outcome_with(
    &inside,
    PluginsSchema {
      enabled: true,
      roots: vec![PathBuf::from("plugins")],
      entries: Vec::new(),
      verify: PluginVerifySchema::default(),
    },
  );
  let session = assemble_with_steps(&outcome, Vec::new()).expect("assemble");
  // 目录内无 *.wasm → 无插件加载，注册表已建但加载清单为空
  assert!(session.loaded_plugins.is_empty());
}

/// 场景：`entries.manifest` 配置解析（Step C B3.2 权威清单字段）。
#[test]
fn plugins_manifest_field_parses_from_toml() {
  let dir = tempfile::tempdir().expect("temp dir");
  let path = dir.path().join("ribo.toml");
  std::fs::write(
    &path,
    r#"
[plugins]
enabled = true

[[plugins.entries]]
path = "plugins/demo.wasm"
manifest = "auth/demo.json"
"#,
  )
  .expect("write config");
  let outcome = LoadOutcome::load(Some(&path), None);
  let entry = &outcome.plugins.entries[0];
  assert_eq!(entry.manifest.as_deref(), Some(Path::new("auth/demo.json")));
}

/// 场景：权威清单文件缺失 → 启动致命（B3.2 信任链不完整不如明确失败）。
#[test]
fn plugins_missing_authority_manifest_is_fatal() {
  let dir = tempfile::tempdir().expect("temp dir");
  let outcome = outcome_with(
    dir.path(),
    PluginsSchema {
      enabled: true,
      roots: Vec::new(),
      entries: vec![PluginEntrySchema {
        path: PathBuf::from("demo.wasm"),
        manifest: Some(PathBuf::from("missing.json")),
        provenance: None,
        config: None,
        acl: PluginAclSchema::default(),
      }],
      verify: PluginVerifySchema::default(),
    },
  );
  let err = match assemble_with_steps(&outcome, Vec::new()) {
    Err(err) => err,
    Ok(_) => panic!("missing authority manifest must be fatal"),
  };
  assert_eq!(
    err.code(),
    "plugin_manifest_unreadable",
    "got: {}",
    err.code()
  );
}

/// 场景：`roots` 目录不存在 → 启动致命（配置错误在装配期早暴露）。
#[test]
fn plugins_roots_scan_missing_dir_is_fatal() {
  let dir = tempfile::tempdir().expect("temp dir");
  let outcome = outcome_with(
    dir.path(),
    PluginsSchema {
      enabled: true,
      roots: vec![PathBuf::from("no-such-dir")],
      entries: Vec::new(),
      verify: PluginVerifySchema::default(),
    },
  );
  let err = match assemble_with_steps(&outcome, Vec::new()) {
    Err(err) => err,
    Ok(_) => panic!("missing plugin root must be fatal"),
  };
  assert_eq!(err.code(), "plugin_dir_unreadable", "got: {}", err.code());
}

// ===== Step C 来源校验（签名 / 信任链）=====

use ring::signature::KeyPair;

fn hex(bytes: &[u8]) -> String {
  const D: &[u8; 16] = b"0123456789abcdef";
  let mut s = String::with_capacity(bytes.len() * 2);
  for b in bytes {
    s.push(D[(b >> 4) as usize] as char);
    s.push(D[(b & 15) as usize] as char);
  }
  s
}

/// 与 `ra_plugins::provenance` 的签名载荷同构（domain-separated + NUL 分隔）。
/// 测试侧独立实现，避免生产 API 暴露签名工具。
fn signature_message(plugin_id: &str, wasm: &[u8], manifest: &[u8]) -> Vec<u8> {
  let digest = blake3::hash(wasm);
  let mut msg = b"ribo-plugin-provenance-v1\0".to_vec();
  msg.extend_from_slice(plugin_id.as_bytes());
  msg.push(0);
  msg.extend_from_slice(hex(digest.as_bytes()).as_bytes());
  msg.push(0);
  msg.extend_from_slice(hex(blake3::hash(manifest).as_bytes()).as_bytes());
  msg
}

fn signed_guest() -> (Vec<u8>, Vec<u8>) {
  let manifest = br#"{"plugin_id":"signed","tools":[]}"#.to_vec();
  let wat_src = format!(
    r#"(module
      (memory (export "memory") 1)
      (data (i32.const 16) "{m}")
      (func (export "alloc") (param i32) (result i32) (i32.const 4096))
      (func (export "dealloc") (param i32 i32))
      (func (export "list_tools") (result i32 i32)
        (i32.const 16) (i32.const {len}))
      (func (export "execute") (param i32 i32 i32 i32) (result i32 i32)
        (i32.const 0) (i32.const 0)))"#,
    m = String::from_utf8(manifest.clone())
      .unwrap()
      .replace('"', "\\\""),
    len = manifest.len(),
  );
  (wat::parse_str(&wat_src).expect("valid WAT"), manifest)
}

/// 写入一套完整签名材料，返回可复用的插件配置。
fn signed_fixture(
  dir: &Path,
  wasm: &[u8],
  manifest: &[u8],
  seed: &[u8; 32],
) -> PluginsSchema {
  let pair = ring::signature::Ed25519KeyPair::from_seed_unchecked(seed)
    .expect("valid seed");
  let digest = blake3::hash(wasm);
  let sig = pair.sign(&signature_message("signed", wasm, manifest));
  let trusted = ring::signature::Ed25519KeyPair::from_seed_unchecked(seed)
    .expect("valid seed");
  std::fs::write(
    dir.join("keys.json"),
    format!(
      r#"{{"keys":[{{"id":"release","public_key":"{}"}}]}}"#,
      hex(trusted.public_key().as_ref())
    ),
  )
  .expect("write keys");
  std::fs::write(
    dir.join("provenance.json"),
    format!(
      r#"{{"signer_id":"release","signature":"{}","wasm_digest":"{}"}}"#,
      hex(sig.as_ref()),
      hex(digest.as_bytes())
    ),
  )
  .expect("write provenance");
  std::fs::write(dir.join("manifest.json"), manifest).expect("write manifest");
  std::fs::write(dir.join("signed.wasm"), wasm).expect("write wasm");

  PluginsSchema {
    enabled: true,
    roots: Vec::new(),
    entries: vec![PluginEntrySchema {
      path: PathBuf::from("signed.wasm"),
      config: None,
      acl: PluginAclSchema::default(),
      manifest: Some(PathBuf::from("manifest.json")),
      provenance: Some(PathBuf::from("provenance.json")),
    }],
    verify: PluginVerifySchema {
      require_signature: true,
      trusted_keys: Some(PathBuf::from("keys.json")),
    },
  }
}

fn assemble_err(outcome: &LoadOutcome) -> ra_core::error::RiboError {
  match assemble_with_steps(outcome, Vec::new()) {
    Err(err) => err,
    Ok(_) => panic!("signed plugin verification must be fatal on failure"),
  }
}

/// 场景：TOML 中 `[plugins.verify]` 与 `provenance` 字段逐项落位。
#[test]
fn plugins_verify_fields_parse_from_toml() {
  let dir = tempfile::tempdir().expect("temp dir");
  let path = dir.path().join("ribo.toml");
  std::fs::write(
    &path,
    r#"
[plugins]
enabled = true

[plugins.verify]
require_signature = true
trusted_keys = "keys.json"

[[plugins.entries]]
path = "signed.wasm"
manifest = "manifest.json"
provenance = "provenance.json"
"#,
  )
  .expect("write config");
  let outcome = LoadOutcome::load(Some(&path), None);
  assert!(outcome.plugins.verify.require_signature);
  assert_eq!(
    outcome.plugins.verify.trusted_keys,
    Some(PathBuf::from("keys.json"))
  );
  assert_eq!(
    outcome.plugins.entries[0].provenance,
    Some(PathBuf::from("provenance.json"))
  );
}

/// 场景：生产策略启用但缺公钥清单 → 启动致命（禁止空信任列表静默放行）。
#[test]
fn plugins_require_signature_without_keys_is_fatal() {
  let dir = tempfile::tempdir().expect("temp dir");
  let outcome = outcome_with(
    dir.path(),
    PluginsSchema {
      enabled: true,
      roots: Vec::new(),
      entries: Vec::new(),
      verify: PluginVerifySchema {
        require_signature: true,
        trusted_keys: None,
      },
    },
  );
  let err = assemble_err(&outcome);
  assert!(
    err.code().contains("trusted_keys_missing"),
    "got: {}",
    err.code()
  );
}

/// 场景：签名有效 + 摘要匹配 + 签名者授信 → 插件正常装配。
#[test]
fn plugins_signed_wasm_loads() {
  let dir = tempfile::tempdir().expect("temp dir");
  let (wasm, manifest) = signed_guest();
  let schema = signed_fixture(dir.path(), &wasm, &manifest, &[7; 32]);
  let outcome = outcome_with(dir.path(), schema);
  let session = assemble_with_steps(&outcome, Vec::new()).expect("assemble");
  assert_eq!(session.loaded_plugins, vec!["signed".to_string()]);
}

/// 场景：wasm 被替换（签名仍指向旧摘要）→ 编译前拒绝。
#[test]
fn plugins_tampered_wasm_is_rejected() {
  let dir = tempfile::tempdir().expect("temp dir");
  let (wasm, manifest) = signed_guest();
  // 用原始 wasm 生成签名，随后在磁盘上替换文件——模拟发布后篡改
  let schema = signed_fixture(dir.path(), &wasm, &manifest, &[7; 32]);
  let mut tampered = wasm.clone();
  tampered.push(0);
  std::fs::write(dir.path().join("signed.wasm"), tampered)
    .expect("overwrite with tampered wasm");
  let outcome = outcome_with(dir.path(), schema);
  let err = assemble_err(&outcome);
  assert_eq!(err.code(), "plugin.plugin_signature_invalid", "got: {err}");
  assert!(err.message().contains("wasm digest mismatch"), "got: {err}");
}

/// 场景：签名者不在公钥白名单 → 拒绝（换钥签名的伪造来源）。
#[test]
fn plugins_untrusted_signer_is_rejected() {
  let dir = tempfile::tempdir().expect("temp dir");
  let (wasm, manifest) = signed_guest();
  let schema = signed_fixture(dir.path(), &wasm, &manifest, &[7; 32]);
  // 信任清单里只登记另一个 ID：签名者的 `release` 不在白名单中
  std::fs::write(
    dir.path().join("keys.json"),
    r#"{"keys":[{"id":"other","public_key":"0000000000000000000000000000000000000000000000000000000000000000"}]}"#,
  )
  .expect("overwrite trust list");
  let outcome = outcome_with(dir.path(), schema);
  let err = assemble_err(&outcome);
  assert_eq!(err.code(), "plugin.plugin_signature_invalid", "got: {err}");
  assert!(err.message().contains("untrusted signer"), "got: {err}");
}

/// 场景：生产策略下 roots 扫描项（无清单 / 无签名）→ 拒绝。
#[test]
fn plugins_unsigned_root_scan_is_rejected_in_production() {
  let dir = tempfile::tempdir().expect("temp dir");
  let (wasm, _) = signed_guest();
  std::fs::create_dir_all(dir.path().join("wasm")).expect("create dir");
  std::fs::write(dir.path().join("wasm/a.wasm"), wasm).expect("write wasm");
  std::fs::write(
    dir.path().join("keys.json"),
    r#"{"keys":[{"id":"release","public_key":"0000000000000000000000000000000000000000000000000000000000000000"}]}"#,
  )
  .expect("write keys");
  let outcome = outcome_with(
    dir.path(),
    PluginsSchema {
      enabled: true,
      roots: vec![PathBuf::from("wasm")],
      entries: Vec::new(),
      verify: PluginVerifySchema {
        require_signature: true,
        trusted_keys: Some(PathBuf::from("keys.json")),
      },
    },
  );
  let err = assemble_err(&outcome);
  assert_eq!(err.code(), "plugin.plugin_signature_invalid", "got: {err}");
  assert!(
    err.message().contains("signed authority manifest required"),
    "got: {err}"
  );
}
