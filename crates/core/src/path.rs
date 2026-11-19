//! 域内路径解析原语：信任域沙箱校验的唯一实现。
//!
//! 三层信任域模型要求每个域内部执行**完全相同**的沙箱检查——词法拒绝
//! 绝对路径与 `..` → `canonicalize` → `starts_with(canonical(domain_root))`。
//! 本模块把这套检查收敛为 [`resolve_in_domain`] 一个入口，并附带可组合的
//! 词法判定 [`is_lexically_safe`] 与归属判定 [`is_within`]，供需要保留自身
//! 错误码的调用方（如插件装配层）复用判定逻辑而不重复实现。
//!
//! # 不变量
//!
//! - **域根 vs 域内路径**：`domain_root` 是"域选择器"（沙箱边界），本身
//!   允许绝对路径（如 CI 传入 `--root /tmp/build`），由调用方单独校验存在
//!   性与目录类型；本模块的 P1 校验只作用于**域内相对路径** `raw`。
//! - **不做向上递归**：任何 `..` 组件一律拒绝，杜绝 `cwd` 向上渗透。
//! - **物理校验**：`canonicalize` 消解符号链接与 Windows 目录联接后再判定
//!   归属，堵住"词法合法但经链接指向域外"的逃逸面。
//!
//! # 错误域
//!
//! 本模块错误统一使用 `path` 域（见 [`PATH_INVALID`] / [`PATH_ESCAPED`] /
//! [`PATH_INTERNAL`]），不复用 `config` 或 `plugin` 域——上层可按需映射为
//! 自身领域错误码。

use std::path::{Component, Path, PathBuf};

use crate::error::{RiboError, RiboResult, Severity};

/// 错误域标识：所有路径原语错误共用的 domain 字段。
pub const DOMAIN: &str = "path";

/// 错误码：词法非法——绝对路径、`..` 组件、Windows 盘符前缀或空串。
pub const PATH_INVALID: &str = "path.invalid";
/// 错误码：物理逃逸——`canonicalize` 后路径不在域根之内。
pub const PATH_ESCAPED: &str = "path.escaped";
/// 错误码：域根不可读或 `canonicalize` IO 失败（宿主内部错误）。
pub const PATH_INTERNAL: &str = "path.internal";

/// 构造词法非法错误（Severity: Error）。
///
/// # Examples
///
/// ```
/// use ra_core::path::{path_invalid, PATH_INVALID};
///
/// let err = path_invalid("absolute path not allowed", "project.skills");
/// assert_eq!(err.code(), PATH_INVALID);
/// assert_eq!(err.domain(), "path");
/// ```
pub fn path_invalid(reason: impl Into<String>, ctx: &str) -> RiboError {
  let reason = reason.into();
  tracing::warn!(ctx = %ctx, reason = %reason, "path lexical validation rejected");
  RiboError::new(
    DOMAIN,
    PATH_INVALID,
    format!("invalid path in {ctx}: {reason}"),
  )
  .with_severity(Severity::Error)
  .with_context("ctx", ctx)
  .with_context("reason", reason)
}

/// 构造物理逃逸错误（Severity: Error，加载期致命）。
///
/// # Examples
///
/// ```
/// use ra_core::path::{path_escaped, PATH_ESCAPED};
///
/// let err = path_escaped("project.plugins.roots[0]");
/// assert_eq!(err.code(), PATH_ESCAPED);
/// ```
pub fn path_escaped(ctx: &str) -> RiboError {
  tracing::warn!(ctx = %ctx, "path escaped domain root");
  RiboError::new(
    DOMAIN,
    PATH_ESCAPED,
    format!("path escapes domain root in {ctx}"),
  )
  .with_severity(Severity::Error)
  .with_context("ctx", ctx)
}

/// 构造宿主内部路径错误（Severity: Critical）。
///
/// # Examples
///
/// ```
/// use ra_core::path::{path_internal, PATH_INTERNAL};
///
/// let err = path_internal("domain root canonicalize", "permission denied");
/// assert_eq!(err.code(), PATH_INTERNAL);
/// assert_eq!(err.severity(), &ra_core::error::Severity::Critical);
/// ```
pub fn path_internal(stage: &str, cause: impl core::fmt::Display) -> RiboError {
  RiboError::new(
    DOMAIN,
    PATH_INTERNAL,
    format!("path internal error at {stage}"),
  )
  .with_severity(Severity::Critical)
  .with_context("stage", stage)
  .with_context("reason", cause.to_string())
}

/// 词法判定：`raw` 是否为安全的域内相对路径。
///
/// 安全条件：非空、无绝对路径语义、无盘符前缀、无根组件、无 `..` 组件。
/// 同时检查 [`Component::RootDir`] 是跨平台的必要防御——Windows 下
/// `/absolute/path` 的 `is_absolute()` 为 false（缺盘符），但其 `RootDir`
/// 组件会在 `join` 时替换基址到分区根，构成逃逸。
/// 本判定**不做 IO**，适用于路径尚未创建时的加载期校验。
///
/// # Examples
///
/// ```
/// use std::path::Path;
/// use ra_core::path::is_lexically_safe;
///
/// assert!(is_lexically_safe(Path::new("skills/review.md")));
/// assert!(!is_lexically_safe(Path::new("../etc/passwd")));
/// assert!(!is_lexically_safe(Path::new("/absolute/path")));
/// assert!(!is_lexically_safe(Path::new("")));
/// ```
pub fn is_lexically_safe(raw: &Path) -> bool {
  !raw.as_os_str().is_empty()
    && !raw.is_absolute()
    && !raw.components().any(|c| {
      matches!(
        c,
        Component::ParentDir | Component::Prefix(_) | Component::RootDir
      )
    })
}

/// 归属判定：已 `canonicalize` 的候选路径是否位于已 `canonicalize` 的域根内。
///
/// 供需要保留自身错误码的调用方复用（如插件装配层把逃逸映射为
/// `config.plugin_load_failed`）。传入的两个路径**必须都已 canonicalize**，
/// 否则符号链接逃逸可能无法被识别。
///
/// # Examples
///
/// ```
/// use std::path::Path;
/// use ra_core::path::is_within;
///
/// let root = std::fs::canonicalize(".").expect("cwd exists");
/// assert!(is_within(&root, &root.join("sub/file")));
/// assert!(!is_within(&root, &root.parent().expect("has parent").join("outside")));
/// ```
pub fn is_within(canonical_root: &Path, canonical_candidate: &Path) -> bool {
  canonical_candidate.starts_with(canonical_root)
}

/// 域内相对路径解析（P1 权威实现）：词法拒绝 → canonicalize → 归属校验。
///
/// # 双层校验
///
/// 1. **词法**：拒绝绝对路径、`..` 组件、Windows 盘符前缀与空串（先判后
///    解析，避免对非法路径做 IO）；
/// 2. **物理**：`canonicalize` 候选后必须 `starts_with(canonical(domain_root))`
///    ——专门堵符号链接与 Windows 目录联接逃逸。
///
/// # Arguments
///
/// - `domain_root`：域根（其本身须已由域选择器校验：存在 + 是目录；允许绝对）；
/// - `raw`：域内相对路径；
/// - `ctx`：错误上下文标记（如 `project.skills`），用于带域标记报错。
///
/// # Errors
///
/// - [`PATH_INVALID`]：词法非法；
/// - [`PATH_ESCAPED`]：canonicalize 后逃出域根；
/// - [`PATH_INTERNAL`]：域根不可读或候选路径无法 canonicalize。
///
/// # 不变量
///
/// 返回的路径已 canonicalize（消解所有符号链接）；调用方**不得**再对其
/// 拼接 `..` 或相对路径做归属判断（会重新引入逃逸面）。
///
/// # Examples
///
/// ```no_run
/// use std::path::Path;
/// use ra_core::path::resolve_in_domain;
///
/// let root = Path::new("/tmp/project");
/// let resolved =
///   resolve_in_domain(root, Path::new("skills/review.md"), "project.skills")?;
/// assert!(resolved.starts_with(root.canonicalize()?));
/// # Ok::<_, Box<dyn std::error::Error>>(())
/// ```
pub fn resolve_in_domain(
  domain_root: &Path,
  raw: &Path,
  ctx: &str,
) -> RiboResult<PathBuf> {
  // ===== ① 词法拒绝（先判后解析，避免对非法路径做 IO）=====
  if !is_lexically_safe(raw) {
    let reason = if raw.as_os_str().is_empty() {
      "empty path"
    } else if raw.is_absolute() {
      "absolute path not allowed"
    } else {
      "`..` or prefix component not allowed"
    };
    return Err(path_invalid(reason, ctx));
  }

  // ===== ② 物理校验（域根与候选各自 canonicalize 后判归属）=====
  let canonical_root = domain_root
    .canonicalize()
    .map_err(|e| path_internal("domain root unreadable", e))?;
  let candidate = domain_root.join(raw);
  let canonical_candidate = candidate
    .canonicalize()
    .map_err(|e| path_internal("path unreadable", e))?;
  if !is_within(&canonical_root, &canonical_candidate) {
    return Err(path_escaped(ctx));
  }
  Ok(canonical_candidate)
}

#[cfg(test)]
mod tests {
  use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
  };

  use super::*;

  /// 唯一临时沙箱目录（系统临时目录下），Drop 时递归删除；纯 std 实现，
  /// 测试模块内使用不违反"非测试代码禁用 tempfile"的约束。
  struct TempRoot(PathBuf);

  impl TempRoot {
    fn new(tag: &str) -> Self {
      let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
      let dir = std::env::temp_dir()
        .join(format!("ra-core-path-{tag}-{}-{nanos}", std::process::id()));
      std::fs::create_dir_all(&dir).expect("create temp root");
      Self(dir)
    }

    fn path(&self) -> &Path {
      &self.0
    }
  }

  impl Drop for TempRoot {
    fn drop(&mut self) {
      let _ = std::fs::remove_dir_all(&self.0);
    }
  }

  /// 在 link 处建指向 target 的目录链接；返回 false 表示宿主不支持。
  fn link_dir(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    {
      std::os::unix::fs::symlink(target, link).is_ok()
    }
    #[cfg(windows)]
    {
      let ok = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false);
      ok || std::os::windows::fs::symlink_dir(target, link).is_ok()
    }
  }

  /// 场景：词法判定识别安全与非法路径。
  /// 预期：相对且无 .. 的路径为真；绝对、空、含 .. 的为假。
  #[test]
  fn lexical_safety_classifies_paths() {
    assert!(is_lexically_safe(Path::new("skills/review.md")));
    assert!(is_lexically_safe(Path::new("a/./b")));
    assert!(!is_lexically_safe(Path::new("")));
    assert!(!is_lexically_safe(Path::new("../etc/passwd")));
    assert!(!is_lexically_safe(Path::new("a/../../b")));
    assert!(!is_lexically_safe(Path::new("/absolute/path")));
  }

  /// 场景：域内正常相对路径解析。
  /// 预期：返回一个位于 canonical 域根内、已消解符号链接的路径。
  #[test]
  fn resolve_in_domain_accepts_safe_relative_path() {
    let tmp = TempRoot::new("ok");
    std::fs::create_dir_all(tmp.path().join("skills")).expect("mkdir skills");
    std::fs::write(tmp.path().join("skills/review.md"), "x").expect("write");
    let canonical_root = tmp.path().canonicalize().expect("canonical root");

    let got = resolve_in_domain(
      tmp.path(),
      Path::new("skills/review.md"),
      "project.skills",
    )
    .expect("safe path should resolve");
    assert!(got.starts_with(&canonical_root));
    assert!(got.ends_with("skills/review.md"));
  }

  /// 场景：词法非法的域内路径被拒绝。
  /// 预期：绝对路径、含 ..、空串均返回 path.invalid。
  #[test]
  fn resolve_in_domain_rejects_lexical_violations() {
    let tmp = TempRoot::new("lexical");
    std::fs::create_dir_all(tmp.path().join("sub")).expect("mkdir sub");
    for raw in [
      Path::new("../escape"),
      Path::new("sub/../../x"),
      Path::new(""),
    ] {
      let err = resolve_in_domain(tmp.path(), raw, "ctx").unwrap_err();
      assert_eq!(err.code(), PATH_INVALID, "raw = {raw:?}");
    }
  }

  /// 场景：域内符号链接指向域外目录。
  /// 预期：canonicalize 后归属判定失败，返回 path.escaped。
  #[test]
  fn resolve_in_domain_rejects_symlink_escape() {
    let tmp = TempRoot::new("symlink");
    let inside = tmp.path().join("inside");
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&inside).expect("mkdir inside");
    std::fs::create_dir_all(&outside).expect("mkdir outside");
    std::fs::write(outside.join("secret.txt"), "secret").expect("write secret");

    let link = inside.join("escape");
    if !link_dir(&outside, &link) {
      // 宿主不支持创建链接时跳过（Windows 需开发者模式或 junction）
      eprintln!("skip symlink escape test: host cannot create links");
      return;
    }

    let err = resolve_in_domain(
      &inside,
      Path::new("escape/secret.txt"),
      "project.skills",
    )
    .unwrap_err();
    assert_eq!(err.code(), PATH_ESCAPED);
  }

  /// 场景：域根本身可为绝对路径（域选择器语义，不参与 P1）。
  /// 预期：绝对域根 + 域内相对路径正常解析。
  #[test]
  fn resolve_in_domain_allows_absolute_domain_root() {
    let tmp = TempRoot::new("absroot");
    std::fs::create_dir_all(tmp.path().join("data")).expect("mkdir data");
    let abs_root = tmp.path().canonicalize().expect("canonical root");
    let got = resolve_in_domain(&abs_root, Path::new("data"), "ctx")
      .expect("absolute domain root should be allowed");
    assert!(got.starts_with(&abs_root));
  }
}
