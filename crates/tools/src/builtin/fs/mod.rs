//! 文件系统工具族（read/write/edit/ls/glob/grep）的共享设施。
//!
//! [`FsRoot`] 提供以工作区根为基准的路径解析与越界防护；其余为
//! 图片魔数探测与取消检查的公共辅助函数。

mod constants;
mod edit;
mod glob;
mod grep;
mod ls;
mod read;
mod write;

use std::{
  collections::HashMap,
  fs::canonicalize,
  path::{Component, Path, PathBuf},
};

use ra_core::{
  error::RiboResult,
  path::{is_lexically_safe, is_within, path_invalid},
};

use crate::{
  context::ToolContext,
  error::{cancelled, invalid_arguments, plugin_forbidden},
};

pub use self::{
  edit::create_edit_tool, glob::create_glob_tool, grep::create_grep_tool,
  ls::create_ls_tool, read::create_read_tool, write::create_write_tool,
};

/// 文件系统沙箱根：fs 工具族共用的路径解析基准。
#[derive(Debug, Clone, Default)]
pub struct FsRoot {
  /// 规范化后的工作区根目录。
  root: PathBuf,
  /// 具名根表（v3 fs.mounts）：scheme → 独立 FsRoot（只读）。
  /// 与主根同等的 校验（词法 + canonicalize 归属），域间不渗透。
  /// 空表 = 无挂载点，行为与单根一致。
  named_roots: HashMap<String, FsRoot>,
  /// 本域是否只读（v3 挂载点恒为 true）。仅对具名域有意义；
  /// 主根恒为可写（保持既有 fs 行为）。
  read_only: bool,
  /// 域标识（错误信息用）；主根为 `project`。
  domain_id: String,
}

impl FsRoot {
  /// 创建沙箱根：canonicalize 规范化根目录。
  ///
  /// 域根本身是"域选择器"（允许绝对路径，如 CI 的 `--root /tmp/build`），
  /// 不参与域内路径的 词法校验；此处只做 canonicalize + 存在性检查。
  ///
  /// # Errors
  ///
  /// 根目录不存在或不可访问时返回 `tool.invalid_arguments`。
  pub fn new(root: &Path) -> RiboResult<Self> {
    Ok(Self {
      root: canonicalize(root).map_err(|e| {
        invalid_arguments(
          "fs",
          format!("workspace root does not exist: {}: {e}", root.display()),
        )
      })?,
      named_roots: HashMap::new(),
      read_only: false,
      domain_id: "project".to_string(),
    })
  }

  /// 构造具名域根（域选择器校验已由装配层完成：绝对 / 存在 / 是目录）。
  ///
  /// 域内路径校验与主根完全一致（canonicalize + 归属），仅 `read_only`
  /// 与 `domain_id` 不同。`read_only = true` 时写入类工具拒绝操作。
  pub fn named(
    domain_id: &str,
    root: &Path,
    read_only: bool,
  ) -> RiboResult<Self> {
    Ok(Self {
      root: canonicalize(root).map_err(|e| {
        invalid_arguments(
          "fs",
          format!(
            "domain `{domain_id}` root does not exist: {}: {e}",
            root.display()
          ),
        )
      })?,
      named_roots: HashMap::new(),
      read_only,
      domain_id: domain_id.to_string(),
    })
  }

  /// 注册具名域根（scheme → FsRoot）。同名覆盖（装配层已保证 id 唯一）。
  pub fn add_named_root(&mut self, scheme: impl Into<String>, root: FsRoot) {
    self.named_roots.insert(scheme.into(), root);
  }

  /// 注册多少个具名域（装配摘要展示用）。
  pub fn named_root_count(&self) -> usize {
    self.named_roots.len()
  }

  /// 返回规范化后的主根目录。
  pub fn root(&self) -> &Path {
    &self.root
  }

  /// 返回本域标识。
  pub fn domain_id(&self) -> &str {
    &self.domain_id
  }

  /// 本域是否只读。
  pub fn is_read_only(&self) -> bool {
    self.read_only
  }
  /// 解析用户路径：相对路径拼接到根目录下，绝对路径按原样保留；
  /// 随后逐段消除 `.` / `..`（`..` 弹空即越界），最终结果必须仍在
  /// 根目录内，杜绝符号路径逃逸。
  ///
  /// 在词法归一化之后追加**物理归属校验**：从候选路径的最近已存在祖先
  /// 开始 canonicalize，再拼回剩余组件，确保符号链接 / Windows 目录联接
  /// 不能把路径导向沙箱外（详见 [`Self::resolve_canonical`]）。
  ///
  /// **`scheme://` 前缀**：`dataset://a/b.jpg` 取 scheme
  /// `dataset` 查具名域根，剩余相对部分按该域的 规则解析；无前缀路径
  /// 仍走主根，行为不变。具名域内部不再解析前缀（防串联）。
  ///
  /// # Errors
  ///
  /// `..` 越出根目录、符号链接逃逸、未知 scheme 或最终结果不在对应
  /// 根目录内时返回错误（未知 scheme / 域内非法词法为 `path.invalid`，
  /// 其余越界维持 `tool.invalid_arguments` 的稳定消息）。
  pub fn resolve(&self, raw: &str) -> RiboResult<PathBuf> {
    // scheme 前缀分派（仅主根持有具名域表）
    if let Some((scheme, rest)) = split_scheme(raw) {
      let domain = self.named_roots.get(scheme).ok_or_else(|| {
        path_invalid(
          format!("unknown domain scheme `{scheme}`"),
          "fs.resolve.scheme",
        )
      })?;
      // 域内路径必须词法安全（拒绝绝对 / `..` / 空）——每域一遍
      if !is_lexically_safe(Path::new(rest)) {
        return Err(path_invalid(
          format!("invalid path inside domain `{scheme}`: {rest}"),
          "fs.resolve.domain",
        ));
      }
      // 具名域无嵌套前缀：直接走其本地解析（不经 scheme 分派）
      return domain.resolve_local(rest);
    }
    self.resolve_local(raw)
  }

  /// 写入类工具的路径解析：在 [`Self::resolve`] 之上追加只读域检查。
  ///
  /// `kind = "data"` 等只读域拒绝写入，返回 `plugin.plugin_forbidden`
  ///（复用 plugin 域既有码）；主根与可写域行为与 [`Self::resolve`] 一致。
  ///
  /// # Errors
  ///
  /// 只读域写入 → `plugin.plugin_forbidden`；其余同 [`Self::resolve`]。
  pub fn resolve_for_write(
    &self,
    raw: &str,
    tool_name: &str,
  ) -> RiboResult<PathBuf> {
    // 自守卫：read_only 是域属性——即使调用方未走 scheme 前缀分派
    //（直接在具名根实例上操作），写入同样被拒绝（纵深防御）。
    if self.read_only {
      return Err(plugin_forbidden(tool_name, &self.domain_id));
    }
    if let Some((scheme, _)) = split_scheme(raw)
      && let Some(domain) = self.named_roots.get(scheme)
      && domain.is_read_only()
    {
      return Err(plugin_forbidden(tool_name, scheme));
    }
    self.resolve(raw)
  }

  /// 本地路径解析（不做 scheme 分派）：供主根与具名域共用。
  fn resolve_local(&self, raw: &str) -> RiboResult<PathBuf> {
    // v3：主沙箱路径禁止访问 .ribo/ 内部目录（签名公钥/会话/插件）；
    // 具名挂载域（domain_id != "project"）不受此限——它们由用户全局声明。
    if self.domain_id == "project" {
      let candidate = Path::new(raw);
      let effective = if candidate.is_absolute() {
        candidate.to_path_buf()
      } else {
        self.root.join(candidate)
      };
      if effective
        .components()
        .any(|c| c.as_os_str() == std::ffi::OsStr::new(".ribo"))
      {
        // 安全审计点：内部子树访问尝试留痕（签名公钥/会话/插件等敏感目录）
        tracing::warn!(
          path = %raw,
          "fs path rejected: .ribo internal subtree is forbidden"
        );
        return Err(ra_core::error::RiboError::new(
          "path",
          "internal_forbidden",
          ".ribo is internal and not accessible through fs tools",
        ));
      }
    }
    let candidate = Path::new(raw);
    let joined = if candidate.is_absolute() {
      candidate.to_path_buf()
    } else {
      self.root.join(candidate)
    };

    let mut parts: Vec<std::ffi::OsString> = Vec::new();
    for comp in joined.components() {
      match comp {
        Component::CurDir => {}
        Component::ParentDir => {
          if parts.pop().is_none() {
            return Err(invalid_arguments("fs", "path escapes workspace root"));
          }
        }
        other => parts.push(other.as_os_str().to_os_string()),
      }
    }
    let result: PathBuf = parts.into_iter().collect();
    if !result.starts_with(&self.root) {
      return Err(invalid_arguments("fs", "path escapes workspace root"));
    }
    // 物理归属校验：堵符号链接 / 目录联接逃逸（词法层无法识别）。
    self.resolve_canonical(&result)
  }

  /// 物理归属校验：把词法归一化后的候选路径落到真实文件系统上复核。
  ///
  /// 算法（兼顾"新建文件目标尚不存在"与"符号链接逃逸"两难）：
  /// 1. 从 `candidate` 逐级向上找到**最近的存在祖先**——用 `symlink_metadata`
  ///    判定，使符号链接本身也算"存在"（否则会把链接当成缺失组件跳过，
  ///    恰好漏掉逃逸面）；
  /// 2. `canonicalize` 该祖先（消解符号链接与 Windows 目录联接），并以
  ///    [`is_within`] 判定是否仍在 `self.root` 内；
  /// 3. 把向上收集到的缺失尾部组件按原顺序拼回，得到最终可 IO 的路径。
  ///
  /// 该设计让 `write` 的新建文件（目标不存在）仍能通过——它只校验最近的
  /// 已存在父目录，而非要求目标文件存在。
  ///
  /// # Errors
  ///
  /// 祖先不存在、canonicalize 失败或祖先逃出沙箱时返回
  /// `tool.invalid_arguments`，对外消息固定为 `"path escapes workspace root"`，
  /// 保持零行为变化的承诺。
  fn resolve_canonical(&self, candidate: &Path) -> RiboResult<PathBuf> {
    let mut probe = candidate;
    // ① 向上找到最近的存在祖先（含符号链接本身）
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    while std::fs::symlink_metadata(probe).is_err() {
      let name = probe
        .file_name()
        .ok_or_else(|| invalid_arguments("fs", "path escapes workspace root"))?
        .to_os_string();
      tail.push(name);
      probe = probe.parent().ok_or_else(|| {
        invalid_arguments("fs", "path escapes workspace root")
      })?;
    }

    // ② canonicalize 祖先并判定归属（复用统一原语）
    let canonical = canonicalize(probe)
      .map_err(|_| invalid_arguments("fs", "path escapes workspace root"))?;
    if !is_within(&self.root, &canonical) {
      return Err(invalid_arguments("fs", "path escapes workspace root"));
    }

    // ③ 拼回缺失尾部（新建文件场景）
    let mut resolved = canonical;
    for name in tail.into_iter().rev() {
      resolved.push(name);
    }
    Ok(resolved)
  }
}

/// 拆分 `scheme://rest` 前缀。
///
/// scheme 须匹配 `^[a-z][a-z0-9_]*$`（与域 id 约束一致，长度上限由装配层
/// 校验）；无前缀 / 格式不合法返回 `None`（按普通相对路径处理）。
/// `rest` 为空返回 `Some((scheme, ""))`，由调用方的词法校验判为非法。
fn split_scheme(raw: &str) -> Option<(&str, &str)> {
  let (scheme, rest) = raw.split_once("://")?;
  let valid = !scheme.is_empty()
    && scheme.starts_with(|c: char| c.is_ascii_lowercase())
    && scheme
      .chars()
      .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
  valid.then_some((scheme, rest))
}

/// 按魔数探测图片 MIME（PNG/JPEG/GIF/WebP）；未识别返回 `None`。
pub(crate) fn detect_image_mime(bytes: &[u8]) -> Option<&'static str> {
  if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
    return Some("image/png");
  }
  if bytes.starts_with(b"\xFF\xD8\xFF") {
    return Some("image/jpeg");
  }
  if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
    return Some("image/gif");
  }
  if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
    return Some("image/webp");
  }
  None
}

/// 取消检查：信号已取消时返回 `tool.cancelled` 错误。
pub(crate) fn check_cancel(
  ctx: &ToolContext,
  tool_name: &str,
) -> RiboResult<()> {
  if ctx.signal().is_cancelled() {
    return Err(cancelled(tool_name));
  }
  Ok(())
}

#[cfg(test)]
pub(crate) mod test_support {
  //! fs 工具族共享的测试辅助（仅测试构建参与编译）：供 fs 各工具
  //! 文件的 `tests` 模块复用，提供唯一临时沙箱目录，每个测试独占
  //! 一个目录，Drop 时递归清理，避免相互干扰与残留。

  use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
  };

  /// 唯一临时沙箱目录（系统临时目录下的子目录），Drop 时递归删除。
  pub(crate) struct TempRoot(PathBuf);

  impl TempRoot {
    /// 以 `tag` 参与命名创建唯一沙箱目录并返回句柄。
    pub(crate) fn new(tag: &str) -> Self {
      // 唯一性：tag + 进程号 + 纳秒时间戳，避免并行测试互踩
      let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_nanos();
      let dir = std::env::temp_dir()
        .join(format!("ra-tools-{tag}-{}-{nanos}", std::process::id()));
      std::fs::create_dir_all(&dir).expect("create temp sandbox root");
      Self(dir)
    }

    /// 沙箱根目录路径。
    pub(crate) fn path(&self) -> &Path {
      &self.0
    }
  }

  impl Drop for TempRoot {
    fn drop(&mut self) {
      // 清理失败不 panic：不影响测试结论（Windows 偶发句柄延迟释放）
      let _ = std::fs::remove_dir_all(&self.0);
    }
  }
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use tokio_util::sync::CancellationToken;

  use super::{test_support::TempRoot, *};
  use crate::error::{CANCELLED, INVALID_ARGUMENTS};

  /// 在唯一临时沙箱根上构造 FsRoot（返回句柄以维持目录存活）。
  fn fs_root(tag: &str) -> (TempRoot, FsRoot) {
    let tmp = TempRoot::new(tag);
    let root = FsRoot::new(tmp.path()).expect("temp sandbox root exists");
    (tmp, root)
  }

  /// 场景：resolve 接受根内相对路径（含 `.` 与段内 `..` 中和）。
  ///
  /// 预期："."、"sub"、"sub/deeper"、"a/../b" 均解析成功且仍在根内。
  #[test]
  fn resolve_allows_paths_inside_root() {
    let (_tmp, root) = fs_root("resolve-ok");
    for raw in [".", "sub", "sub/deeper", "a/../b"] {
      let resolved = root
        .resolve(raw)
        .unwrap_or_else(|e| panic!("in-root path {raw:?} should resolve: {e}"));
      assert!(
        resolved.starts_with(root.root()),
        "resolved {resolved:?} must stay inside root"
      );
    }
  }

  /// 场景：resolve 拒绝越界路径。
  ///
  /// 预期："../x"、"a/../../x" 与指向根外的绝对路径均返回
  /// `tool.invalid_arguments`。
  #[test]
  fn resolve_rejects_paths_escaping_root() {
    let (_tmp, root) = fs_root("resolve-escape");

    // 相对路径逐级跳出根目录
    for raw in ["../x", "a/../../x"] {
      let err = root.resolve(raw).unwrap_err();
      assert_eq!(err.code(), INVALID_ARGUMENTS, "raw = {raw}");
    }

    // 绝对路径指向根外同样拒绝
    let outside = std::env::temp_dir().join("ra-tools-outside-sandbox-target");
    let err = root.resolve(&outside.to_string_lossy()).unwrap_err();
    assert_eq!(err.code(), INVALID_ARGUMENTS);
  }

  /// 场景（T4b）：主沙箱内 `.ribo/` 内部子树（trusted_keys.json、
  /// sessions/ 等）读写均被拒——黑名单，签名公钥 / 会话 / 插件
  /// 对模型不可见。
  #[test]
  fn t4b_ribo_internal_subtree_is_forbidden() {
    let (_tmp, root) = fs_root("ribo-forbidden");

    // 相对路径读
    let err = root
      .resolve(".ribo/trusted_keys.json")
      .expect_err("read .ribo must be denied");
    assert_eq!(err.code(), "internal_forbidden", "got: {err}");

    // 相对路径写（resolve_for_write 走同一黑名单）
    let err = root
      .resolve_for_write(".ribo/sessions/x.jsonl", "write")
      .expect_err("write .ribo must be denied");
    assert_eq!(err.code(), "internal_forbidden", "got: {err}");

    // 绝对路径绕不过
    let abs = root.root().join(".ribo/trusted_keys.json");
    let err = root
      .resolve(&abs.to_string_lossy())
      .expect_err("absolute .ribo must be denied");
    assert_eq!(err.code(), "internal_forbidden", "got: {err}");
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

  /// 场景：新建文件（目标不存在）路径解析。
  ///
  /// 预期：最近存在祖先为沙箱根，尾部拼回后仍在根内，解析成功。
  #[test]
  fn resolve_allows_new_file_under_existing_parent() {
    let (_tmp, root) = fs_root("new-file");
    let resolved = root
      .resolve("not-yet-created.txt")
      .expect("new file should resolve via existing parent");
    assert_eq!(resolved, root.root().join("not-yet-created.txt"));
  }

  /// 场景：沙箱内的符号链接指向沙箱外目录（物理逃逸）。
  ///
  /// 预期：词法合法但 canonicalize 后逃出根 → `tool.invalid_arguments`。
  #[test]
  fn resolve_rejects_symlink_escape() {
    let tmp = TempRoot::new("symlink-escape");
    let outside = std::env::temp_dir().join(format!(
      "ra-tools-outside-{}-{}",
      std::process::id(),
      std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos()
    ));
    std::fs::create_dir_all(&outside).expect("create outside dir");
    std::fs::write(outside.join("secret.txt"), "secret").expect("write secret");

    let root = FsRoot::new(tmp.path()).expect("root exists");
    let link = tmp.path().join("escape");
    if !link_dir(&outside, &link) {
      let _ = std::fs::remove_dir_all(&outside);
      eprintln!("skip: host cannot create directory links");
      return;
    }

    let err = root.resolve("escape/secret.txt").unwrap_err();
    assert_eq!(err.code(), INVALID_ARGUMENTS);
    assert!(
      err.to_string().contains("path escapes workspace root"),
      "stable message preserved: {err}"
    );
    let _ = std::fs::remove_dir_all(&outside);
  }

  /// 场景：`dataset://` 前缀经具名域根解析，域间不渗透。
  ///
  /// 预期：正常相对路径解析到具名域内；未知 scheme 与域间交叉引用
  /// （在 dataset 域用 `company://`）均被拒绝。
  #[test]
  fn named_root_scheme_resolves_within_domain() {
    let main_tmp = TempRoot::new("named-main");
    let data_tmp = TempRoot::new("named-data");
    let company_tmp = TempRoot::new("named-company");
    std::fs::write(data_tmp.path().join("a.txt"), "data").expect("seed");

    let mut root = FsRoot::new(main_tmp.path()).expect("main root");
    root.add_named_root(
      "dataset",
      FsRoot::named("dataset", data_tmp.path(), true).expect("data domain"),
    );
    root.add_named_root(
      "company",
      FsRoot::named("company", company_tmp.path(), false)
        .expect("company domain"),
    );
    assert_eq!(root.named_root_count(), 2);

    let resolved = root.resolve("dataset://a.txt").expect("in-domain read");
    // Windows canonicalize 会加 \\?\ 前缀，统一规范化后比较
    let expected = std::fs::canonicalize(data_tmp.path().join("a.txt"))
      .expect("seed file exists");
    assert_eq!(resolved, expected);
    // 无前缀仍走主根
    let main_root_canon =
      std::fs::canonicalize(main_tmp.path()).expect("main root exists");
    assert!(
      root
        .resolve("local.txt")
        .expect("main read")
        .starts_with(&main_root_canon)
    );
    // 未知 scheme → path.invalid
    assert_eq!(
      root.resolve("ghost://a.txt").unwrap_err().code(),
      ra_core::path::PATH_INVALID
    );
  }

  /// 场景：具名域内 `..` / 绝对路径词法拒绝（每域一遍）。
  #[test]
  fn named_root_rejects_escaping_domain_paths() {
    let main_tmp = TempRoot::new("named-escape-main");
    let data_tmp = TempRoot::new("named-escape-data");
    let mut root = FsRoot::new(main_tmp.path()).expect("main root");
    root.add_named_root(
      "dataset",
      FsRoot::named("dataset", data_tmp.path(), true).expect("data domain"),
    );
    for raw in [
      "dataset://../etc/passwd",
      "dataset://a/../../b",
      "dataset:///absolute",
      "dataset://",
    ] {
      let err = root.resolve(raw).unwrap_err();
      assert_eq!(err.code(), ra_core::path::PATH_INVALID, "raw = {raw}");
    }
  }

  /// 场景：只读域写入被拒（`plugin.plugin_forbidden`），可写域放行。
  #[test]
  fn named_root_read_only_rejects_writes() {
    let main_tmp = TempRoot::new("named-ro-main");
    let data_tmp = TempRoot::new("named-ro-data");
    let company_tmp = TempRoot::new("named-ro-company");
    let mut root = FsRoot::new(main_tmp.path()).expect("main root");
    root.add_named_root(
      "dataset",
      FsRoot::named("dataset", data_tmp.path(), true).expect("data domain"),
    );
    root.add_named_root(
      "company",
      FsRoot::named("company", company_tmp.path(), false)
        .expect("company domain"),
    );
    let err = root
      .resolve_for_write("dataset://out.txt", "write")
      .unwrap_err();
    assert_eq!(err.code(), crate::error::PLUGIN_FORBIDDEN);
    // 可写域与主根正常
    assert!(root.resolve_for_write("company://out.txt", "write").is_ok());
    assert!(root.resolve_for_write("local.txt", "write").is_ok());
  }

  /// 场景：具名域内符号链接指向域外 → 物理归属校验拒绝。
  #[test]
  fn named_root_rejects_symlink_escape() {
    let main_tmp = TempRoot::new("named-link-main");
    let data_tmp = TempRoot::new("named-link-data");
    let outside = std::env::temp_dir().join(format!(
      "ra-tools-named-outside-{}-{}",
      std::process::id(),
      std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos()
    ));
    std::fs::create_dir_all(&outside).expect("create outside dir");

    let mut root = FsRoot::new(main_tmp.path()).expect("main root");
    root.add_named_root(
      "dataset",
      FsRoot::named("dataset", data_tmp.path(), false).expect("data domain"),
    );
    let link = data_tmp.path().join("escape");
    if !link_dir(&outside, &link) {
      let _ = std::fs::remove_dir_all(&outside);
      eprintln!("skip: host cannot create directory links");
      return;
    }
    let err = root.resolve("dataset://escape/secret.txt").unwrap_err();
    assert_eq!(err.code(), INVALID_ARGUMENTS);
    let _ = std::fs::remove_dir_all(&outside);
  }

  /// 场景：detect_image_mime 按魔数识别 PNG/JPEG/GIF/WebP。
  ///
  /// 预期：四种魔数分别命中对应 MIME；RIFF 非 WEBP 与普通文本返回 None。
  #[test]
  fn detect_image_mime_recognizes_supported_magics() {
    assert_eq!(detect_image_mime(b"\x89PNG\r\n\x1a\n"), Some("image/png"));
    assert_eq!(detect_image_mime(b"\xFF\xD8\xFF\xE0"), Some("image/jpeg"));
    assert_eq!(detect_image_mime(b"GIF87a-rest"), Some("image/gif"));
    assert_eq!(detect_image_mime(b"GIF89a-rest"), Some("image/gif"));

    // RIFF 容器 + 偏移 8..12 的 WEBP 标识才算 WebP
    let mut webp = b"RIFF".to_vec();
    webp.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
    webp.extend_from_slice(b"WEBP");
    assert_eq!(detect_image_mime(&webp), Some("image/webp"));

    // RIFF 容器但非 WEBP → 不识别
    assert_eq!(detect_image_mime(b"RIFF____WAVE"), None);
    // 普通文本 → 不识别
    assert_eq!(detect_image_mime(b"plain text file"), None);
  }

  /// 场景：check_cancel 对已取消上下文返回取消错误。
  ///
  /// 预期：取消态 ctx → `tool.cancelled`；未取消 ctx → Ok(())。
  #[test]
  fn check_cancel_maps_cancelled_signal_to_error() {
    let token = CancellationToken::new();
    token.cancel();
    let ctx = ToolContext::new(token, Arc::new(|_| {}));
    let err = check_cancel(&ctx, "read").unwrap_err();
    assert_eq!(err.code(), CANCELLED);

    // 未取消：直接放行
    check_cancel(&ToolContext::detached(), "read").expect("not cancelled");
  }
}
