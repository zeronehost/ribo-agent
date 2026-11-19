//! 内置工具集：echo / calculator / time 与沙箱文件系统工具族。
//!
//! 文件系统工具共享同一个 [`FsRoot`] 沙箱根：所有路径在沙箱内
//! 解析，越界即拒绝。`register_builtin` 一次性把全部内置工具注册进
//! 管理器，是部署侧最常用的装配入口。

mod calculator;
mod echo;
mod fs;
mod time;

pub use self::{
  calculator::create_calculator_tool,
  echo::create_echo_tool,
  fs::{
    FsRoot, create_edit_tool, create_glob_tool, create_grep_tool,
    create_ls_tool, create_read_tool, create_write_tool,
  },
  time::create_current_time_tool,
};

use std::{path::Path, sync::Arc};

use ra_core::error::RiboResult;

use crate::{manager::ToolManager, traits::AgentTool};

/// 用已构造的 [`FsRoot`] 构造全部内置工具实例（不注册）。
///
/// 与 [`builtin_tools`] 的区别：允许注入带具名域根表的 `FsRoot`
///（19_ Step 3 的 `scheme://` 前缀解析）。
pub fn builtin_tools_with_root(fs_root: FsRoot) -> Vec<Arc<dyn AgentTool>> {
  vec![
    echo::create_echo_tool(),
    calculator::create_calculator_tool(),
    time::create_current_time_tool(),
    fs::create_read_tool(&fs_root),
    fs::create_write_tool(&fs_root),
    fs::create_edit_tool(&fs_root),
    fs::create_ls_tool(&fs_root),
    fs::create_glob_tool(&fs_root),
    fs::create_grep_tool(&fs_root),
  ]
}

/// 构造全部内置工具实例（不注册）。
///
/// # 参数
///
/// - `fs_root`：文件系统工具族的沙箱根目录；不存在即失败。
///
/// # Errors
///
/// `fs_root` 无法解析（目录不存在）→ `tool.invalid_arguments`。
pub fn builtin_tools(fs_root: &Path) -> RiboResult<Vec<Arc<dyn AgentTool>>> {
  let fs_root = fs::FsRoot::new(fs_root)?;
  Ok(builtin_tools_with_root(fs_root))
}

/// 用已构造的 [`FsRoot`] 注册全部内置工具（含具名域根表）。
pub fn register_builtin_with_root(
  manager: &ToolManager,
  fs_root: FsRoot,
) -> RiboResult<()> {
  let tools = builtin_tools_with_root(fs_root);
  let total = tools.len();
  for tool in tools {
    manager.register(tool)?;
  }
  tracing::info!(count = total, "builtin tools registered");
  Ok(())
}

/// 把全部内置工具注册进管理器（含 Schema 编译校验）。
///
/// # Errors
///
/// 任一工具注册失败（Schema 非法 / 同名冲突）→ 原样透传，
/// 已注册的工具保持有效（部分注册不回滚）。
pub fn register_builtin(
  manager: &ToolManager,
  fs_root: &Path,
) -> RiboResult<()> {
  let tools = builtin_tools(fs_root)?;
  let total = tools.len();
  for tool in tools {
    manager.register(tool)?;
  }
  tracing::info!(count = total, "builtin tools registered");
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{builtin::fs::test_support::TempRoot, error::INVALID_ARGUMENTS};

  /// 场景：register_builtin 一次性注册全部内置工具。
  ///
  /// 预期：get_tool_schemas 恰为 9 个工具，名称按字典序输出：
  /// calculator / current_time / echo / edit / glob / grep / ls / read / write。
  #[test]
  fn register_builtin_yields_full_sorted_toolset() {
    let tmp = TempRoot::new("builtin-register");
    let manager = ToolManager::new();
    register_builtin(&manager, tmp.path())
      .expect("builtin registration should succeed");

    let names: Vec<String> = manager
      .get_tool_schemas()
      .into_iter()
      .map(|s| s.name)
      .collect();
    assert_eq!(
      names,
      vec![
        "calculator",
        "current_time",
        "echo",
        "edit",
        "glob",
        "grep",
        "ls",
        "read",
        "write"
      ]
    );
  }

  /// 场景：沙箱根目录不存在时装配失败。
  ///
  /// 预期：register_builtin 返回 `tool.invalid_arguments`。
  #[test]
  fn register_builtin_rejects_missing_root() {
    // 唯一命名保证目录确实不存在
    let missing = std::env::temp_dir().join(format!(
      "ra-tools-missing-root-{}-{}",
      std::process::id(),
      std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_nanos()
    ));
    let manager = ToolManager::new();
    let err = register_builtin(&manager, &missing).unwrap_err();
    assert_eq!(err.code(), INVALID_ARGUMENTS);
  }
}
