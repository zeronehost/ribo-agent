//! 两档信任闸门：配置信任默认开、代码信任须显式（19_ Step 4，设计§7.1）。
//!
//! Project 域的 `.wasm` 属可执行资源，默认**不加载**——须用户在
//! `global.root/trusted_projects.toml` 显式授予 `code_trust`。Plugin 域
//! 的插件同理，且 `trusted = true` 声明必须来自 Global 域（项目自签无效）。
//! Global 域自身的插件默认信任（用户自己放的）。
//!
//! 本模块把判定所需的全部上下文（信任记录 + 来源归属）封装为
//! [`CodeTrustContext`]，装配期据此决定每条插件声明是否放行。

use std::{
  collections::HashSet,
  path::{Path, PathBuf},
};

use super::trust::TrustedProjects;

/// 代码信任判定上下文。
///
/// `None`（无 Global 域）时装配层退化为「自动加载」的旧语义——
/// 这是设计§9 的回滚开关：零配置 / 无 Global 目录时行为不变。
#[derive(Debug, Clone)]
pub struct CodeTrustContext {
  /// 用户显式信任记录（来自 `global.root/trusted_projects.toml`）。
  trust: TrustedProjects,
  /// Global 域根目录（供 Step 5 技能 / AGENTS.md 定位）；None = 无 Global 域。
  global_root: Option<PathBuf>,
  /// 来自 Global 域声明的插件域 id 集合。
  /// 只有这些域的 `trusted = true` 才生效；项目自签一律忽略。
  global_domain_ids: HashSet<String>,
}

impl CodeTrustContext {
  /// 创建信任上下文。
  pub fn new(
    trust: TrustedProjects,
    global_domain_ids: impl IntoIterator<Item = String>,
  ) -> Self {
    Self {
      trust,
      global_root: None,
      global_domain_ids: global_domain_ids.into_iter().collect(),
    }
  }

  /// 设置 Global 域根目录（装配期定位 Global 技能 / AGENTS.md）。
  pub fn with_global_root(mut self, root: &Path) -> Self {
    self.global_root = Some(root.to_path_buf());
    self
  }

  /// Global 域根目录（None = 无 Global 域）。
  pub fn global_root(&self) -> Option<&Path> {
    self.global_root.as_deref()
  }

  /// 项目是否被授予代码信任（可执行插件放行）。
  pub fn is_project_code_trusted(&self, project_root: &Path) -> bool {
    self
      .trust
      .is_code_trusted(&normalize_project_key(project_root))
  }

  /// 插件域是否被信任：须同时满足「声明来自 Global 域」与
  /// 「信任记录里 `trusted = true`」。项目自签的域恒返回 `false`。
  pub fn is_domain_code_trusted(&self, domain_id: &str) -> bool {
    self.global_domain_ids.contains(domain_id)
      && self.trust.is_plugin_domain_trusted(domain_id)
  }

  /// 域声明是否来自 Global 域（用于项目自签 warning）。
  pub fn is_global_domain(&self, domain_id: &str) -> bool {
    self.global_domain_ids.contains(domain_id)
  }

  /// 底层信任记录只读引用（供 CLI `trust list` 等复用）。
  pub fn trust(&self) -> &TrustedProjects {
    &self.trust
  }
}

/// 项目路径 → 信任记录键：统一为平台分隔符的字符串。
///
/// `trusted_projects.toml` 的键由 `ribo trust add <path>` 写入，跨平台
/// 使用 `Path::display()` 的原生形式；此处保持一致，不做大小写折叠
///（Windows 路径大小写敏感性由文件系统语义决定，避免误判）。
pub fn normalize_project_key(path: &Path) -> String {
  path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
  use super::*;

  /// 场景：项目/域信任判定（Global 声明 + 信任记录两者合取）。
  #[test]
  fn code_trust_conjunction() {
    let project = Path::new("/home/user/proj-a");
    let mut trust = TrustedProjects::empty();
    // 手动注入信任条目：通过 add_project / add_plugin_domain
    trust.add_project(&normalize_project_key(project), Some("reviewed"));
    trust.add_plugin_domain("company");
    let ctx = CodeTrustContext::new(trust, ["company".to_string()]);

    assert!(ctx.is_project_code_trusted(project));
    assert!(ctx.is_domain_code_trusted("company"));
    // 未记录的域 / 项目
    assert!(!ctx.is_project_code_trusted(Path::new("/other")));
    assert!(!ctx.is_domain_code_trusted("unknown"));
  }

  /// 场景：项目自签的域（不在 global_domain_ids 中）即使信任记录为 true 也不放行。
  #[test]
  fn project_self_declared_domain_is_never_trusted() {
    let mut trust = TrustedProjects::empty();
    trust.add_plugin_domain("selfsigned");
    // global_domain_ids 不含 selfsigned
    let ctx = CodeTrustContext::new(trust, std::iter::empty::<String>());
    // 信任记录里 true 也不放行（来源不对）
    assert!(!ctx.is_domain_code_trusted("selfsigned"));
    assert!(!ctx.is_global_domain("selfsigned"));
  }
}
