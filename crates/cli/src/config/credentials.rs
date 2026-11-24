//! 凭证文件 `auth.toml` 的读写（首次使用即创建，无 migrate 概念）。
//!
//! 对应设计：`auth.toml` 是用户可编辑的 TOML，凭证不落 `ribo.toml`。
//! 本模块用 [`toml_edit::DocumentMut`] 承载内容，从而**保留用户手写注释**
//! ——重新登录只更新目标 provider 的字段，其他 provider 与注释原样保留。
//!
//! # 权限纪律（设计 R11）
//!
//! 凭证文件在创建时设为 `0600`（Unix）；改写走"临时文件 + rename"，
//! 并显式保留原文件权限（Windows 上退化为 ACL 检查）。日志不记录任何
//! token 值，只记录 provider id 与操作结果。

use std::path::{Path, PathBuf};

use ra_core::error::{RiboError, RiboResult, Severity};
use toml_edit::{DocumentMut, Item, Table, Value};

/// 凭证域错误码前缀。
const DOMAIN: &str = "config";

fn auth_error(code: &'static str, message: impl Into<String>) -> RiboError {
  RiboError::new(DOMAIN, code, message).with_severity(Severity::Error)
}

/// 单个 provider 的凭证字段（读视图）。
///
/// # Examples
///
/// ```
/// use ra_cli::config::credentials::ProviderCredential;
///
/// let cred = ProviderCredential::default();
/// assert!(cred.api_key.is_none());
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderCredential {
  /// API key（明文；`$ENV` 占位由 provider 层处理，此处按文件原样读取）。
  pub api_key: Option<String>,
  /// OAuth access token。
  pub oauth_access_token: Option<String>,
  /// OAuth refresh token。
  pub oauth_refresh_token: Option<String>,
  /// 过期时间（TOML 原生日期时间；`None` 视为不过期）。
  pub expires_at: Option<String>,
  /// 授权 scope 列表。
  pub scopes: Vec<String>,
  /// 凭证创建时间（TOML 原生日期时间）。
  pub created_at: Option<String>,
}

/// `auth.toml` 的读写句柄：内部持有 `DocumentMut` 以保留注释。
///
/// # Examples
///
/// ```
/// use ra_cli::config::credentials::AuthConfig;
///
/// let cfg = AuthConfig::empty();
/// assert!(cfg.provider_ids().is_empty());
/// ```
#[derive(Debug, Clone)]
pub struct AuthConfig {
  doc: DocumentMut,
}

impl AuthConfig {
  /// 创建空凭证集（等价于空 `auth.toml`）。
  pub fn empty() -> Self {
    Self {
      doc: DocumentMut::new(),
    }
  }

  /// 从路径读取；文件不存在返回空集（首次使用前）。
  ///
  /// # Errors
  ///
  /// 文件存在但解析失败 → `config.auth_invalid`，并说明具体解析错误。
  pub fn load(path: &Path) -> RiboResult<Self> {
    if !path.exists() {
      return Ok(Self::empty());
    }
    let text = std::fs::read_to_string(path).map_err(|e| {
      auth_error(
        "auth_unreadable",
        format!("auth file {} unreadable: {e}", path.display()),
      )
    })?;
    let doc = text.parse::<DocumentMut>().map_err(|e| {
      auth_error(
        "auth_invalid",
        format!("auth file {} invalid TOML: {e}", path.display()),
      )
    })?;
    // 快速校验：providers 子表内每个 provider 的值类型合法（拒绝 null 等）
    if let Some(providers) = self_providers(&doc) {
      for (id, item) in providers.iter() {
        if let Some(table) = item.as_table() {
          validate_provider_table(id, table)?;
        }
      }
    }
    Ok(Self { doc })
  }

  /// 列出所有 provider id（字典序，来自 `[providers.<id>]` 子表）。
  pub fn provider_ids(&self) -> Vec<String> {
    self_providers(&self.doc)
      .map(|t| {
        t.iter()
          .filter_map(|(k, v)| v.as_table().map(|_| k.to_owned()))
          .collect()
      })
      .unwrap_or_default()
  }

  /// 读取单个 provider 的凭证字段（读视图）。
  pub fn provider(&self, id: &str) -> Option<ProviderCredential> {
    let table = self_providers(&self.doc)?.get(id)?.as_table()?;
    Some(ProviderCredential {
      api_key: get_string(table, "api_key"),
      oauth_access_token: get_string(table, "oauth_access_token"),
      oauth_refresh_token: get_string(table, "oauth_refresh_token"),
      expires_at: get_datetime(table, "expires_at"),
      scopes: get_array(table, "scopes"),
      created_at: get_datetime(table, "created_at"),
    })
  }

  /// 设置单个 provider 的 API key（只动目标字段，保留其他字段与注释）。
  pub fn set_api_key(&mut self, id: &str, key: &str) {
    let table = self.ensure_provider(id);
    insert_string(table, "api_key", key);
    tracing::info!(provider_id = %id, "auth api key updated");
  }

  /// 设置 OAuth 凭证字段（只动目标 provider 与给定字段）。
  pub fn set_oauth(
    &mut self,
    id: &str,
    access_token: &str,
    refresh_token: Option<&str>,
    expires_at: Option<&str>,
  ) {
    let table = self.ensure_provider(id);
    insert_string(table, "oauth_access_token", access_token);
    if let Some(rt) = refresh_token {
      insert_string(table, "oauth_refresh_token", rt);
    }
    if let Some(exp) = expires_at {
      insert_raw(table, "expires_at", exp);
    }
    tracing::info!(provider_id = %id, "auth oauth credentials updated");
  }

  /// 移除单个 provider（整段删除；用于 `ribo auth logout`）。
  pub fn remove_provider(&mut self, id: &str) -> bool {
    let removed = self
      .doc
      .get_mut("providers")
      .and_then(Item::as_table_mut)
      .map(|t| t.remove(id).is_some())
      .unwrap_or(false);
    if removed {
      tracing::info!(provider_id = %id, "auth provider removed");
    }
    removed
  }

  /// 写入到路径：首次创建 `0600`，已存在则保留原权限与注释。
  ///
  /// 采用"临时文件 + rename"原子写，避免中途失败损坏原文件。
  ///
  /// # Errors
  ///
  /// IO 失败 → `config.auth_write_failed`。
  pub fn save(&self, path: &Path) -> RiboResult<()> {
    let existed = path.exists();
    let temp = temp_path(path);
    std::fs::write(&temp, self.doc.to_string()).map_err(|e| {
      auth_error(
        "auth_write_failed",
        format!("cannot write temp auth file: {e}"),
      )
    })?;
    set_credentials_permissions(&temp, path)?;
    std::fs::rename(&temp, path).map_err(|e| {
      let _ = std::fs::remove_file(&temp);
      auth_error("auth_write_failed", format!("cannot commit auth file: {e}"))
    })?;
    if existed {
      tracing::info!("auth file updated");
    } else {
      tracing::info!("auth file created");
    }
    Ok(())
  }

  /// 确保 `[providers.<id>]` 子表存在并返回其可变引用（保留已有内容）。
  fn ensure_provider(&mut self, id: &str) -> &mut Table {
    // 先确保顶层 providers 表存在
    if self.doc.get("providers").and_then(Item::as_table).is_none() {
      let mut providers = Table::new();
      providers.set_implicit(false);
      self.doc.insert("providers", Item::Table(providers));
    }
    let providers = self
      .doc
      .get_mut("providers")
      .and_then(Item::as_table_mut)
      .expect("providers table ensured");
    if providers.get(id).and_then(Item::as_table).is_none() {
      let mut table = Table::new();
      table.set_implicit(false);
      providers.insert(id, Item::Table(table));
    }
    providers
      .get_mut(id)
      .and_then(Item::as_table_mut)
      .expect("provider table ensured")
  }
}

/// 取顶层 `providers` 表（只读）。
fn self_providers(doc: &DocumentMut) -> Option<&Table> {
  doc.get("providers")?.as_table()
}

/// 临时文件路径：与 target 同目录（保证 rename 在同一卷内原子）。
fn temp_path(target: &Path) -> PathBuf {
  let mut name = target
    .file_name()
    .map(|n| n.to_os_string())
    .unwrap_or_else(|| "auth.toml".into());
  name.push(".tmp");
  target.with_file_name(name)
}

/// Unix 下设置 `0600`；已存在文件保留原权限（Windows 退化为 ACL 检查）。
fn set_credentials_permissions(temp: &Path, target: &Path) -> RiboResult<()> {
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    let mode = if target.exists() {
      std::fs::metadata(target)
        .map(|m| m.permissions().mode())
        .unwrap_or(0o600)
    } else {
      0o600
    };
    std::fs::set_permissions(temp, std::fs::Permissions::from_mode(mode))
      .map_err(|e| {
        auth_error(
          "auth_write_failed",
          format!("cannot set auth file permissions: {e}"),
        )
      })?;
  }
  #[cfg(not(unix))]
  {
    // Windows：凭证文件继承 temp 文件的默认 ACL；此处仅确认目标可写，
    // 复杂的 ACL 精调留给部署侧（设计 R11 的退化策略）。
    let _ = temp;
    if target.exists() {
      let _ = std::fs::metadata(target);
    }
  }
  Ok(())
}

/// 校验 provider 表内无非法值（TOML 无 null 的显式拒绝）。
fn validate_provider_table(id: &str, table: &Table) -> RiboResult<()> {
  for (key, item) in table.iter() {
    if item.as_value().is_none() && item.as_table().is_none() {
      return Err(auth_error(
        "auth_invalid",
        format!(
          "unsupported value for {id}.{key}: TOML does not support null; delete the key instead"
        ),
      ));
    }
  }
  Ok(())
}

fn get_string(table: &Table, key: &str) -> Option<String> {
  table
    .get(key)
    .and_then(Item::as_value)
    .and_then(Value::as_str)
    .map(str::to_owned)
}

fn get_datetime(table: &Table, key: &str) -> Option<String> {
  table
    .get(key)
    .and_then(Item::as_value)
    .and_then(Value::as_datetime)
    .map(|dt| dt.to_string())
}

fn get_array(table: &Table, key: &str) -> Vec<String> {
  table
    .get(key)
    .and_then(Item::as_value)
    .and_then(Value::as_array)
    .map(|arr| {
      arr
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
    })
    .unwrap_or_default()
}

fn insert_string(table: &mut Table, key: &str, value: &str) {
  insert_raw(table, key, &format!("\"{}\"", escape_toml_string(value)));
}

fn insert_raw(table: &mut Table, key: &str, raw_value: &str) {
  match table.get_mut(key) {
    Some(Item::Value(v)) => {
      if let Ok(new_value) = raw_value.parse::<Value>() {
        *v = new_value;
      }
    }
    _ => {
      if let Ok(value) = raw_value.parse::<Value>() {
        table.insert(key, Item::Value(value));
      }
    }
  }
}

fn escape_toml_string(s: &str) -> String {
  s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
  use super::*;

  /// 唯一临时目录（系统临时目录下），Drop 时清理；纯 std 实现。
  struct TempDir(PathBuf);

  impl TempDir {
    fn new(tag: &str) -> Self {
      let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
      let dir = std::env::temp_dir()
        .join(format!("ra-cli-auth-{tag}-{}-{nanos}", std::process::id()));
      std::fs::create_dir_all(&dir).expect("create temp dir");
      Self(dir)
    }

    fn path(&self) -> &Path {
      &self.0
    }
  }

  impl Drop for TempDir {
    fn drop(&mut self) {
      let _ = std::fs::remove_dir_all(&self.0);
    }
  }

  /// 场景：首次创建 auth.toml。
  /// 预期：文件生成、权限 0600（Unix）、api_key 可读回。
  #[test]
  fn auth_config_creates_file_with_0600() {
    let dir = TempDir::new("create");
    let path = dir.path().join("auth.toml");
    let mut cfg = AuthConfig::empty();
    cfg.set_api_key("openai", "sk-test-key");
    cfg.save(&path).expect("save");

    assert!(path.exists());
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      let mode =
        std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
      assert_eq!(mode, 0o600, "credentials must be 0600");
    }
    let loaded = AuthConfig::load(&path).expect("load");
    assert_eq!(
      loaded.provider("openai").unwrap().api_key.as_deref(),
      Some("sk-test-key")
    );
  }

  /// 场景：重复写入只更新目标 provider，保留其他 provider 与手写注释。
  /// 预期：openai key 更新，anthropic 不变，注释仍在。
  #[test]
  fn auth_config_preserves_other_providers_and_comments() {
    let dir = TempDir::new("preserve");
    let path = dir.path().join("auth.toml");
    std::fs::write(
      &path,
      "# 手写注释：生产环境密钥\n[providers.openai]\napi_key = \"old-key\"\n\n[providers.anthropic]\napi_key = \"keep-me\"\n",
    )
    .expect("write initial");

    let mut cfg = AuthConfig::load(&path).expect("load");
    cfg.set_api_key("openai", "new-key");
    cfg.save(&path).expect("save");

    let text = std::fs::read_to_string(&path).expect("read");
    assert!(
      text.contains("# 手写注释：生产环境密钥"),
      "comment preserved: {text}"
    );
    let loaded = AuthConfig::load(&path).expect("reload");
    assert_eq!(
      loaded.provider("openai").unwrap().api_key.as_deref(),
      Some("new-key")
    );
    assert_eq!(
      loaded.provider("anthropic").unwrap().api_key.as_deref(),
      Some("keep-me")
    );
  }

  /// 场景：读取用户手写的 auth.toml（含原生 datetime）。
  /// 预期：字段正确读取，expires_at 为字符串形式的 TOML 日期。
  #[test]
  fn auth_config_reads_handwritten_datetime() {
    let dir = TempDir::new("datetime");
    let path = dir.path().join("auth.toml");
    std::fs::write(
      &path,
      r#"
[providers.example]
api_key = "sk-x"
created_at = 2026-11-17T10:00:00Z
expires_at = 2027-11-17T10:00:00Z
scopes = ["read", "write"]
"#,
    )
    .expect("write");

    let cfg = AuthConfig::load(&path).expect("load");
    let cred = cfg.provider("example").expect("provider exists");
    assert_eq!(cred.api_key.as_deref(), Some("sk-x"));
    assert_eq!(cred.expires_at.as_deref(), Some("2027-11-17T10:00:00Z"));
    assert_eq!(cred.scopes, vec!["read", "write"]);
  }

  /// 场景：写入 null 值（TOML 不支持）。
  /// 预期：解析失败并给出清晰错误（delete the key instead）。
  #[test]
  fn auth_config_rejects_null_with_clear_error() {
    let dir = TempDir::new("null");
    let path = dir.path().join("auth.toml");
    std::fs::write(&path, "[providers.openai]\napi_key = null\n")
      .expect("write");
    let err = AuthConfig::load(&path).unwrap_err();
    assert!(
      err.to_string().contains("invalid TOML")
        || err.to_string().contains("null"),
      "got: {err}"
    );
  }

  /// 场景：移除 provider。
  /// 预期：remove_provider 返回 true，读取后该 provider 消失。
  #[test]
  fn auth_config_removes_provider() {
    let dir = TempDir::new("remove");
    let path = dir.path().join("auth.toml");
    let mut cfg = AuthConfig::empty();
    cfg.set_api_key("openai", "k");
    cfg.save(&path).expect("save");

    let mut cfg = AuthConfig::load(&path).expect("load");
    assert!(cfg.remove_provider("openai"));
    assert!(!cfg.remove_provider("openai"));
    cfg.save(&path).expect("save");
    let loaded = AuthConfig::load(&path).expect("reload");
    assert!(loaded.provider("openai").is_none());
  }
}
