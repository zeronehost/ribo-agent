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
  fs::canonicalize,
  path::{Component, Path, PathBuf},
};

use ra_core::error::RiboResult;

use crate::{
  context::ToolContext,
  error::{cancelled, invalid_arguments},
};

pub use self::{
  edit::create_edit_tool, glob::create_glob_tool, grep::create_grep_tool,
  ls::create_ls_tool, read::create_read_tool, write::create_write_tool,
};

/// 文件系统沙箱根：fs 工具族共用的路径解析基准。
#[derive(Debug, Clone)]
pub struct FsRoot {
  /// 规范化后的工作区根目录。
  root: PathBuf,
}

impl FsRoot {
  /// 创建沙箱根：canonicalize 规范化根目录。
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
    })
  }
  /// 返回规范化后的根目录。
  pub fn root(&self) -> &Path {
    &self.root
  }
  /// 解析用户路径：相对路径拼接到根目录下，绝对路径按原样保留；
  /// 随后逐段消除 `.` / `..`（`..` 弹空即越界），最终结果必须仍在
  /// 根目录内，杜绝符号路径逃逸。
  ///
  /// # Errors
  ///
  /// `..` 越出根目录或最终结果不在根目录内时返回 `tool.invalid_arguments`。
  pub fn resolve(&self, raw: &str) -> RiboResult<PathBuf> {
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
    Ok(result)
  }
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
