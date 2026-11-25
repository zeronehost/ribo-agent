//! 两档信任闸门：配置信任默认开、代码信任须显式。
//!
//! Project 的 `.wasm` 属可执行资源，默认**不加载**——须用户在
//! `global.root/trusted_projects.toml` 显式授予 `code_trust`。
//! 资源域已移除，域级信任判定一并删除；项目代码信任原样保留。
//!
//! 本模块把判定所需的全部上下文（信任记录 + 来源归属）封装为
//! [`CodeTrustContext`]，装配期据此决定每条插件声明是否放行。

use std::path::{Path, PathBuf};

use super::trust::TrustedProjects;

/// 代码信任判定上下文。
///
/// `None`（无 Global 域）时装配层退化为「自动加载」的旧语义——
/// 这是设计的回滚开关：零配置 / 无 Global 目录时行为不变。
#[derive(Debug, Clone)]
pub struct CodeTrustContext {
  /// 用户显式信任记录（来自 `global.root/trusted_projects.toml`）。
  trust: TrustedProjects,
  /// Global 域根目录（供技能 / AGENTS.md 定位）；None = 无 Global 域。
  global_root: Option<PathBuf>,
}

impl CodeTrustContext {
  /// 创建信任上下文。
  pub fn new(trust: TrustedProjects) -> Self {
    Self {
      trust,
      global_root: None,
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

  /// 场景：项目代码信任判定（域信任已删除，只保留项目维度）。
  #[test]
  fn code_trust_project_only() {
    let project = Path::new("/home/user/proj-a");
    let mut trust = TrustedProjects::empty();
    trust.add_project(&normalize_project_key(project), Some("reviewed"));
    let ctx = CodeTrustContext::new(trust);

    assert!(ctx.is_project_code_trusted(project));
    assert!(!ctx.is_project_code_trusted(Path::new("/other")));
  }
}
