//! 项目信任记录 `trusted_projects.toml` 的读写（首次使用即创建）。
//!
//! 对应设计§7.2：记录两类信任——项目代码信任（`code_trust`）与插件域信任
//! （`trusted`）。本模块用 [`toml_edit::DocumentMut`] 承载，保留用户手写
//! 注释；`ribo trust add/remove/list` 只改目标条目（命令在 Step 6 挂载）。
//!
//! 信任记录默认不存在；所有域默认未信任（`false`），只有显式记录才放行。

use std::path::Path;

use ra_core::error::{RiboError, RiboResult, Severity};
use toml_edit::{DocumentMut, Item, Table, Value};

/// 信任记录 schema 版本。
pub const SCHEMA_VERSION: i64 = 1;
const DOMAIN: &str = "config";

fn trust_error(code: &'static str, message: impl Into<String>) -> RiboError {
  RiboError::new(DOMAIN, code, message).with_severity(Severity::Error)
}

/// 项目信任条目（读视图）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectTrust {
  /// 项目路径（表键，原始字符串）。
  pub path: String,
  /// 是否授予代码信任（插件可执行）。
  pub code_trust: bool,
  /// 授予时间（RFC3339 字符串）。
  pub trusted_at: Option<String>,
  /// 授予者（如 `"user"`）。
  pub trusted_by: Option<String>,
  /// 用户备注。
  pub note: Option<String>,
}

/// 插件域信任条目（读视图）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginDomainTrust {
  /// 域 id（表键）。
  pub id: String,
  /// 是否信任该域的插件。
  pub trusted: bool,
  /// 授予时间（RFC3339 字符串）。
  pub trusted_at: Option<String>,
}

/// `trusted_projects.toml` 的读写句柄。
///
/// # Examples
///
/// ```
/// use ra_cli::config::trust::TrustedProjects;
///
/// let cfg = TrustedProjects::empty();
/// assert!(cfg.projects().is_empty());
/// assert!(!cfg.is_code_trusted("/any/path"));
/// ```
#[derive(Debug, Clone)]
pub struct TrustedProjects {
  doc: DocumentMut,
}

impl TrustedProjects {
  /// 创建空信任记录（含 `version = 1`）。
  pub fn empty() -> Self {
    let mut doc = DocumentMut::new();
    doc.insert("version", Item::Value(Value::from(SCHEMA_VERSION)));
    Self { doc }
  }

  /// 从路径读取；不存在返回空记录。
  ///
  /// # Errors
  ///
  /// 存在但解析失败 → `config.trust_invalid`。
  pub fn load(path: &Path) -> RiboResult<Self> {
    if !path.exists() {
      return Ok(Self::empty());
    }
    let text = std::fs::read_to_string(path).map_err(|e| {
      trust_error(
        "trust_unreadable",
        format!("trust file {} unreadable: {e}", path.display()),
      )
    })?;
    let doc = text.parse::<DocumentMut>().map_err(|e| {
      trust_error(
        "trust_invalid",
        format!("trust file {} invalid TOML: {e}", path.display()),
      )
    })?;
    Ok(Self { doc })
  }

  /// 写入到路径（原子写，保留注释）。
  ///
  /// # Errors
  ///
  /// IO 失败 → `config.trust_write_failed`。
  pub fn save(&self, path: &Path) -> RiboResult<()> {
    let mut name = path
      .file_name()
      .map(|n| n.to_os_string())
      .unwrap_or_else(|| "trusted_projects.toml".into());
    name.push(".tmp");
    let temp = path.with_file_name(name);
    std::fs::write(&temp, self.doc.to_string()).map_err(|e| {
      trust_error(
        "trust_write_failed",
        format!("cannot write temp trust file: {e}"),
      )
    })?;
    std::fs::rename(&temp, path).map_err(|e| {
      let _ = std::fs::remove_file(&temp);
      trust_error(
        "trust_write_failed",
        format!("cannot commit trust file: {e}"),
      )
    })?;
    tracing::info!("trusted_projects file saved");
    Ok(())
  }

  /// 查询项目是否被授予代码信任（缺省 false）。
  pub fn is_code_trusted(&self, project_path: &str) -> bool {
    self
      .projects_table()
      .and_then(|t| t.get(project_path))
      .and_then(Item::as_table)
      .and_then(|t| t.get("code_trust"))
      .and_then(Item::as_value)
      .and_then(Value::as_bool)
      .unwrap_or(false)
  }

  /// 查询插件域是否被信任（缺省 false）。
  pub fn is_plugin_domain_trusted(&self, id: &str) -> bool {
    self
      .domains_table()
      .and_then(|t| t.get(id))
      .and_then(Item::as_table)
      .and_then(|t| t.get("trusted"))
      .and_then(Item::as_value)
      .and_then(Value::as_bool)
      .unwrap_or(false)
  }

  /// 列出所有项目信任条目（按路径字典序）。
  pub fn projects(&self) -> Vec<ProjectTrust> {
    self
      .projects_table()
      .map(|t| {
        let mut out: Vec<_> = t
          .iter()
          .filter_map(|(path, item)| {
            let table = item.as_table()?;
            Some(ProjectTrust {
              path: unquote_key(path),
              code_trust: table
                .get("code_trust")
                .and_then(Item::as_value)
                .and_then(Value::as_bool)
                .unwrap_or(false),
              trusted_at: get_string(table, "trusted_at"),
              trusted_by: get_string(table, "trusted_by"),
              note: get_string(table, "note"),
            })
          })
          .collect();
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out
      })
      .unwrap_or_default()
  }

  /// 列出所有插件域信任条目（按 id 字典序）。
  pub fn plugin_domains(&self) -> Vec<PluginDomainTrust> {
    self
      .domains_table()
      .map(|t| {
        let mut out: Vec<_> = t
          .iter()
          .filter_map(|(id, item)| {
            let table = item.as_table()?;
            Some(PluginDomainTrust {
              id: id.to_owned(),
              trusted: table
                .get("trusted")
                .and_then(Item::as_value)
                .and_then(Value::as_bool)
                .unwrap_or(false),
              trusted_at: get_string(table, "trusted_at"),
            })
          })
          .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
      })
      .unwrap_or_default()
  }

  /// 授予 / 更新项目代码信任（只动目标条目）。
  pub fn add_project(&mut self, path: &str, note: Option<&str>) {
    let table = self.ensure_project(path);
    insert_bool(table, "code_trust", true);
    insert_string(table, "trusted_at", &now_rfc3339());
    insert_string(table, "trusted_by", "user");
    if let Some(n) = note {
      insert_string(table, "note", n);
    }
    tracing::info!(project = %path, "project code trust granted");
  }

  /// 撤销项目代码信任（置 false，保留条目与注释）。
  pub fn revoke_project(&mut self, path: &str) -> bool {
    let ok = self
      .projects_table_mut()
      .and_then(|t| t.get_mut(path))
      .and_then(Item::as_table_mut);
    match ok {
      Some(table) => {
        insert_bool(table, "code_trust", false);
        tracing::info!(project = %path, "project code trust revoked");
        true
      }
      None => false,
    }
  }

  /// 删除项目条目（整段移除）。
  pub fn remove_project(&mut self, path: &str) -> bool {
    self
      .projects_table_mut()
      .map(|t| t.remove(path).is_some())
      .unwrap_or(false)
  }

  /// 授予 / 更新插件域信任（只动目标条目）。
  pub fn add_plugin_domain(&mut self, id: &str) {
    let table = self.ensure_domain(id);
    insert_bool(table, "trusted", true);
    insert_string(table, "trusted_at", &now_rfc3339());
    tracing::info!(domain = %id, "plugin domain trust granted");
  }

  /// 撤销插件域信任。
  pub fn revoke_plugin_domain(&mut self, id: &str) -> bool {
    match self
      .domains_table_mut()
      .and_then(|t| t.get_mut(id))
      .and_then(Item::as_table_mut)
    {
      Some(table) => {
        insert_bool(table, "trusted", false);
        true
      }
      None => false,
    }
  }

  /// 删除插件域条目。
  pub fn remove_plugin_domain(&mut self, id: &str) -> bool {
    self
      .domains_table_mut()
      .map(|t| t.remove(id).is_some())
      .unwrap_or(false)
  }

  /// 确保 `[projects."<path>"]` 存在（引用引号由 toml_edit 处理）。
  fn ensure_project(&mut self, path: &str) -> &mut Table {
    if self.doc.get("projects").and_then(Item::as_table).is_none() {
      let mut table = Table::new();
      table.set_implicit(false);
      self.doc.insert("projects", Item::Table(table));
    }
    let projects = self
      .doc
      .get_mut("projects")
      .and_then(Item::as_table_mut)
      .expect("projects table ensured");
    if projects.get(path).and_then(Item::as_table).is_none() {
      let mut table = Table::new();
      table.set_implicit(false);
      // 路径含 `/` 等字符，必须用 quoted key
      projects.insert(path, Item::Table(table));
    }
    projects
      .get_mut(path)
      .and_then(Item::as_table_mut)
      .expect("project table ensured")
  }

  /// 确保 `[plugin_domains.<id>]` 存在。
  fn ensure_domain(&mut self, id: &str) -> &mut Table {
    if self
      .doc
      .get("plugin_domains")
      .and_then(Item::as_table)
      .is_none()
    {
      let mut table = Table::new();
      table.set_implicit(false);
      self.doc.insert("plugin_domains", Item::Table(table));
    }
    let domains = self
      .doc
      .get_mut("plugin_domains")
      .and_then(Item::as_table_mut)
      .expect("plugin_domains table ensured");
    if domains.get(id).and_then(Item::as_table).is_none() {
      let mut table = Table::new();
      table.set_implicit(false);
      domains.insert(id, Item::Table(table));
    }
    domains
      .get_mut(id)
      .and_then(Item::as_table_mut)
      .expect("domain table ensured")
  }

  fn projects_table(&self) -> Option<&Table> {
    self.doc.get("projects")?.as_table()
  }

  fn projects_table_mut(&mut self) -> Option<&mut Table> {
    self.doc.get_mut("projects")?.as_table_mut()
  }

  fn domains_table(&self) -> Option<&Table> {
    self.doc.get("plugin_domains")?.as_table()
  }

  fn domains_table_mut(&mut self) -> Option<&mut Table> {
    self.doc.get_mut("plugin_domains")?.as_table_mut()
  }
}

/// 当前时间（RFC3339，jiff）。
fn now_rfc3339() -> String {
  jiff::Timestamp::now().to_string()
}

/// 去掉 key 外层引号（若 TOML 用了 quoted key）。
fn unquote_key(key: &str) -> String {
  if key.len() >= 2 && key.starts_with('"') && key.ends_with('"') {
    key[1..key.len() - 1].to_owned()
  } else {
    key.to_owned()
  }
}

fn get_string(table: &Table, key: &str) -> Option<String> {
  table
    .get(key)
    .and_then(Item::as_value)
    .and_then(Value::as_str)
    .map(str::to_owned)
}

fn insert_string(table: &mut Table, key: &str, value: &str) {
  let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
  insert_raw(table, key, &format!("\"{escaped}\""));
}

fn insert_bool(table: &mut Table, key: &str, value: bool) {
  insert_raw(table, key, if value { "true" } else { "false" });
}

fn insert_raw(table: &mut Table, key: &str, raw: &str) {
  if let Ok(value) = raw.parse::<Value>() {
    table.insert(key, Item::Value(value));
  }
}

#[cfg(test)]
mod tests {
  use std::path::PathBuf;

  use super::*;

  /// 唯一临时目录（纯 std，Drop 清理）。
  struct TempDir(PathBuf);

  impl TempDir {
    fn new(tag: &str) -> Self {
      let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
      let dir = std::env::temp_dir()
        .join(format!("ra-cli-trust-{tag}-{}-{nanos}", std::process::id()));
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

  /// 场景：首次添加项目信任。
  /// 预期：文件创建、code_trust=true、note 记录、version=1。
  #[test]
  fn trust_add_project_creates_file() {
    let dir = TempDir::new("add");
    let path = dir.path().join("trusted_projects.toml");
    let mut cfg = TrustedProjects::empty();
    cfg.add_project("/home/user/proj-a", Some("reviewed plugins"));
    cfg.save(&path).expect("save");

    let loaded = TrustedProjects::load(&path).expect("load");
    assert!(loaded.is_code_trusted("/home/user/proj-a"));
    let projects = loaded.projects();
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].note.as_deref(), Some("reviewed plugins"));
    assert_eq!(projects[0].trusted_by.as_deref(), Some("user"));
  }

  /// 场景：追加与删除条目，保留其他条目与注释。
  /// 预期：第二个项目追加后第一个仍在；删除第二个后第一个仍保留。
  #[test]
  fn trust_add_remove_preserves_other_entries_and_comments() {
    let dir = TempDir::new("preserve");
    let path = dir.path().join("trusted_projects.toml");
    std::fs::write(
      &path,
      "# 用户手写注释\nversion = 1\n\n[projects.\"/a\"]\ncode_trust = true\n",
    )
    .expect("write initial");

    let mut cfg = TrustedProjects::load(&path).expect("load");
    cfg.add_project("/b", None);
    cfg.save(&path).expect("save");

    let text = std::fs::read_to_string(&path).expect("read");
    assert!(text.contains("# 用户手写注释"), "comment preserved: {text}");
    let loaded = TrustedProjects::load(&path).expect("reload");
    assert!(loaded.is_code_trusted("/a"));
    assert!(loaded.is_code_trusted("/b"));
    assert_eq!(loaded.projects().len(), 2);

    let mut cfg = TrustedProjects::load(&path).expect("load");
    assert!(cfg.remove_project("/b"));
    cfg.save(&path).expect("save");
    let loaded = TrustedProjects::load(&path).expect("reload");
    assert_eq!(loaded.projects().len(), 1);
    assert!(loaded.is_code_trusted("/a"));
  }

  /// 场景：撤销信任（置 false 而非删除）。
  /// 预期：is_code_trusted 返回 false，条目仍存在。
  #[test]
  fn trust_revoke_keeps_entry() {
    let dir = TempDir::new("revoke");
    let path = dir.path().join("trusted_projects.toml");
    let mut cfg = TrustedProjects::empty();
    cfg.add_project("/proj", None);
    cfg.save(&path).expect("save");

    let mut cfg = TrustedProjects::load(&path).expect("load");
    assert!(cfg.revoke_project("/proj"));
    cfg.save(&path).expect("save");
    let loaded = TrustedProjects::load(&path).expect("reload");
    assert!(!loaded.is_code_trusted("/proj"));
    assert_eq!(loaded.projects().len(), 1);
  }

  /// 场景：插件域信任记录。
  /// 预期：add_plugin_domain 后 is_plugin_domain_trusted=true。
  #[test]
  fn trust_plugin_domain_roundtrip() {
    let dir = TempDir::new("domain");
    let path = dir.path().join("trusted_projects.toml");
    let mut cfg = TrustedProjects::empty();
    cfg.add_plugin_domain("company");
    cfg.save(&path).expect("save");

    let loaded = TrustedProjects::load(&path).expect("load");
    assert!(loaded.is_plugin_domain_trusted("company"));
    assert!(!loaded.is_plugin_domain_trusted("unknown"));
    assert_eq!(loaded.plugin_domains()[0].id, "company");

    let mut cfg = TrustedProjects::load(&path).expect("load");
    assert!(cfg.remove_plugin_domain("company"));
    cfg.save(&path).expect("save");
    let loaded = TrustedProjects::load(&path).expect("reload");
    assert!(!loaded.is_plugin_domain_trusted("company"));
  }

  /// 场景：默认未信任。
  /// 预期：空记录下所有查询返回 false。
  #[test]
  fn trust_defaults_to_untrusted() {
    let cfg = TrustedProjects::empty();
    assert!(!cfg.is_code_trusted("/any"));
    assert!(!cfg.is_plugin_domain_trusted("any"));
    assert!(cfg.projects().is_empty());
  }
}
