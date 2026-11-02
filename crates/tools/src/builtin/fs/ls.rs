use std::{path::Path, sync::Arc};

use ra_core::{
  error::RiboResult,
  message::{Schema, TextContent, ToolSchema},
};

use crate::{
  context::ToolContext,
  error::exec_failed,
  traits::AgentTool,
  types::{ToolExecutionResult, ToolOutput},
};

use super::{
  FsRoot, check_cancel,
  constants::{LS_MAX_DEPTH, LS_MAX_ENTRIES},
};

/// ls 工具：目录列举。
pub struct LsTool {
  /// 构造期固化的工具声明。
  schema: ToolSchema,
  /// 沙箱根。
  root: FsRoot,
}

impl LsTool {
  /// 构造并固化 schema 与沙箱根。
  pub(super) fn new(root: FsRoot) -> Self {
    Self {
      schema: ToolSchema {
        name: "ls".to_string(),
        description: "List directory entries. Directories are suffixed with \
'/'. Supports recursive listing up to depth 5."
          .to_string(),
        input_schema: Schema(serde_json::json!({
          "type": "object",
          "properties": {
            "path": { "type": "string", "description": "Directory to list (default workspace root)" },
            "depth": { "type": "integer", "minimum": 1, "maximum": 5, "description": "Recursion depth (default 1)" },
            "max_entries": { "type": "integer", "minimum": 1, "description": "Output entry limit (default 500)" }
          },
          "additionalProperties": false
        })),
        constrained_sampling: None,
      },
      root,
    }
  }
}

/// 递归遍历状态（聚合参数，避免 walk 签名过长）。
struct WalkState {
  /// 条目输出上限。
  max_entries: usize,
  /// 输出行（目录带 `/` 后缀，递归条目带相对前缀）。
  lines: Vec<String>,
  /// 已输出条目数。
  total: usize,
  /// 是否发生截断。
  truncated: bool,
}

/// 递归遍历目录（async 递归经 Box::pin；符号链接不跟随）。
async fn walk(
  dir: &Path,
  depth: usize,
  prefix: &str,
  st: &mut WalkState,
  ctx: &ToolContext,
) -> RiboResult<()> {
  // 截断即短路
  if st.total >= st.max_entries {
    st.truncated = true;
    return Ok(());
  }
  check_cancel(ctx, "ls")?;
  // ===== 收集条目（read_dir 逐项） =====
  let mut entries: Vec<(String, bool)> = Vec::new();
  let mut rd = tokio::fs::read_dir(dir)
    .await
    .map_err(|e| exec_failed("ls", format!("read_dir failed: {e}")))?;
  while let Some(entry) = rd
    .next_entry()
    .await
    .map_err(|e| exec_failed("ls", format!("read_dir failed: {e}")))?
  {
    let is_dir = entry
      .file_type()
      .await
      .map_err(|e| exec_failed("ls", format!("file_type failed: {e}")))?
      .is_dir();
    entries.push((entry.file_name().to_string_lossy().into_owned(), is_dir));
  }
  // ===== 排序：目录优先、组内字典序 =====
  entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
  // ===== 输出与下钻 =====
  for (name, is_dir) in entries {
    if st.total >= st.max_entries {
      st.truncated = true;
      return Ok(());
    }
    st.lines.push(if is_dir {
      format!("{prefix}{name}/")
    } else {
      format!("{prefix}{name}")
    });
    st.total += 1;
    if is_dir && depth > 1 {
      Box::pin(walk(
        &dir.join(&name),
        depth - 1,
        &format!("{prefix}{name}/"),
        st,
        ctx,
      ))
      .await?;
    }
  }
  Ok(())
}

#[async_trait::async_trait]
impl AgentTool for LsTool {
  fn schema(&self) -> &ToolSchema {
    &self.schema
  }

  fn label(&self) -> &str {
    "List"
  }

  async fn execute(
    &self,
    _tool_call_id: &str,
    args: serde_json::Value,
    ctx: &ToolContext,
  ) -> RiboResult<ToolExecutionResult> {
    // ===== 取参与路径解析（缺省列工作目录根） =====
    let raw = args["path"].as_str().unwrap_or(".");
    let depth = args["depth"]
      .as_u64()
      .unwrap_or(1)
      .clamp(1, LS_MAX_DEPTH as u64) as usize;
    let max_entries = args["max_entries"]
      .as_u64()
      .unwrap_or(LS_MAX_ENTRIES as u64) as usize;
    let path = self.root.resolve(raw)?;
    check_cancel(ctx, "ls")?;
    if !path.is_dir() {
      return Err(exec_failed("ls", format!("not a directory: {raw}")));
    }
    // ===== 递归遍历 =====
    let mut st = WalkState {
      max_entries,
      lines: Vec::new(),
      total: 0,
      truncated: false,
    };
    walk(&path, depth, "", &mut st, ctx).await?;
    if st.truncated {
      st.lines.push("[... more entries truncated]".to_string());
    }
    // ===== 输出 =====
    let text = if st.lines.is_empty() {
      "(empty)".to_string()
    } else {
      st.lines.join("\n")
    };
    Ok(ToolExecutionResult {
      content: vec![ToolOutput::Text(TextContent {
        content: text,
        signature: None,
      })],
      details: Some(
        serde_json::json!({ "total_entries": st.total, "truncated": st.truncated }),
      ),
      ..Default::default()
    })
  }
}

/// 工厂函数。
pub fn create_ls_tool(root: &FsRoot) -> Arc<dyn AgentTool> {
  Arc::new(LsTool::new(root.clone()))
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use serde_json::json;

  use super::*;
  use crate::{builtin::fs::test_support::TempRoot, error::EXEC_FAILED};

  /// 在唯一临时沙箱根上构造 ls 工具（返回句柄以维持目录存活）。
  fn ls_tool(tag: &str) -> (TempRoot, Arc<dyn AgentTool>) {
    let tmp = TempRoot::new(tag);
    let root = FsRoot::new(tmp.path()).expect("temp sandbox root exists");
    (tmp, create_ls_tool(&root))
  }

  /// 取结果首个文本块内容。
  fn first_text(result: &ToolExecutionResult) -> &str {
    match &result.content[0] {
      ToolOutput::Text(t) => &t.content,
      ToolOutput::Image(_) => panic!("expected text output"),
    }
  }

  /// 场景：列目录条目，目录带 `/` 后缀且目录优先。
  ///
  /// 预期：根下含子目录 sub 与文件 file.txt 时输出恰为
  /// "sub/\nfile.txt"，details 整体等于 `{ total_entries: 2, truncated: false }`。
  #[tokio::test]
  async fn lists_entries_with_dir_suffix_first() {
    let (tmp, tool) = ls_tool("ls-basic");
    std::fs::create_dir(tmp.path().join("sub")).expect("seed dir");
    std::fs::write(tmp.path().join("file.txt"), "x").expect("seed file");

    let result = tool
      .execute("call-1", json!({ "path": "." }), &ToolContext::detached())
      .await
      .expect("listing sandbox root should succeed");
    assert_eq!(first_text(&result), "sub/\nfile.txt");
    assert_eq!(
      result.details.unwrap(),
      json!({ "total_entries": 2, "truncated": false })
    );
  }

  /// 场景：depth=2 递归下钻一层。
  ///
  /// 预期：子目录内容带相对前缀输出；深度耗尽后不再继续下钻。
  #[tokio::test]
  async fn recursive_depth_lists_nested_entries() {
    let (tmp, tool) = ls_tool("ls-depth");
    std::fs::create_dir_all(tmp.path().join("outer/inner")).expect("seed dirs");
    std::fs::write(tmp.path().join("outer/inner/x.txt"), "x")
      .expect("seed file");

    let result = tool
      .execute(
        "call-1",
        json!({ "path": ".", "depth": 2 }),
        &ToolContext::detached(),
      )
      .await
      .expect("recursive listing should succeed");
    // depth=2：列出 outer 与 outer/inner，但不再深入 inner 内部
    assert_eq!(first_text(&result), "outer/\nouter/inner/");
  }

  /// 场景：对文件路径执行 ls。
  ///
  /// 预期：返回 `tool.exec_failed`（not a directory）。
  #[tokio::test]
  async fn listing_a_file_fails() {
    let (tmp, tool) = ls_tool("ls-file");
    std::fs::write(tmp.path().join("f.txt"), "x").expect("seed file");

    let err = tool
      .execute(
        "call-1",
        json!({ "path": "f.txt" }),
        &ToolContext::detached(),
      )
      .await
      .unwrap_err();
    assert_eq!(err.code(), EXEC_FAILED);
  }
}
