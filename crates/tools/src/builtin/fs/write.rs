use std::sync::Arc;

use ra_core::{
  error::RiboResult,
  message::{Schema, TextContent, ToolSchema},
};

use crate::{
  context::ToolContext,
  error::{exec_failed, invalid_arguments},
  traits::AgentTool,
  types::{ToolExecutionResult, ToolOutput},
};

use super::{FsRoot, check_cancel};

/// write 工具：写入文件（覆盖）。
pub struct WriteTool {
  /// 构造期固化的工具声明。
  schema: ToolSchema,
  /// 沙箱根。
  root: FsRoot,
}

impl WriteTool {
  /// 构造并固化 schema 与沙箱根。
  pub(super) fn new(root: FsRoot) -> Self {
    Self {
      schema: ToolSchema {
        name: "write".to_string(),
        description: "Write full content to a file (overwrites existing \
content). Set create_dirs=true when the parent directory may not exist."
          .to_string(),
        input_schema: Schema(serde_json::json!({
          "type": "object",
          "properties": {
            "path": { "type": "string", "description": "Target file path relative to workspace root" },
            "content": { "type": "string", "description": "Full file content" },
            "create_dirs": { "type": "boolean", "description": "Create missing parent directories (default false)" }
          },
          "required": ["path", "content"],
          "additionalProperties": false
        })),
        constrained_sampling: None,
      },
      root,
    }
  }
}

#[async_trait::async_trait]
impl AgentTool for WriteTool {
  fn schema(&self) -> &ToolSchema {
    &self.schema
  }

  fn label(&self) -> &str {
    "Write"
  }

  async fn execute(
    &self,
    _tool_call_id: &str,
    args: serde_json::Value,
    ctx: &ToolContext,
  ) -> RiboResult<ToolExecutionResult> {
    // 取参与路径解析
    let raw = args["path"].as_str().ok_or_else(|| {
      invalid_arguments("write", "field 'path' must be a string")
    })?;
    let content = args["content"].as_str().ok_or_else(|| {
      invalid_arguments("write", "field 'content' must be a string")
    })?;
    let create_dirs = args["create_dirs"].as_bool().unwrap_or(false);
    let path = self.root.resolve_for_write(raw, "write")?;
    check_cancel(ctx, "write")?;
    // 父目录保障（目录创建必须显式）
    let parent = path
      .parent()
      .ok_or_else(|| exec_failed("write", "path has no parent directory"))?;
    if !parent.exists() {
      if create_dirs {
        tokio::fs::create_dir_all(&parent).await.map_err(|e| {
          exec_failed("write", format!("create_dir_all failed: {e}"))
        })?;
      } else {
        return Err(exec_failed(
          "write",
          format!("parent directory does not exist: {}", parent.display()),
        ));
      }
    }
    // 写入（覆盖语义；目标为目录时由 io 错误统一映射 EXEC_FAILED）
    tokio::fs::write(&path, content).await.map_err(|e| {
      exec_failed("write", format!("write failed ({:?}): {raw}", e.kind()))
    })?;
    tracing::info!(path = %raw, bytes = content.len(), "write tool: file written");
    Ok(ToolExecutionResult {
      content: vec![ToolOutput::Text(TextContent {
        content: format!("Wrote {} bytes to {raw}", content.len()),
        signature: None,
      })],
      ..Default::default()
    })
  }
}

/// 工厂函数。
pub fn create_write_tool(root: &FsRoot) -> Arc<dyn AgentTool> {
  Arc::new(WriteTool::new(root.clone()))
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use serde_json::json;

  use super::*;
  use crate::{builtin::fs::test_support::TempRoot, error::EXEC_FAILED};

  /// 在唯一临时沙箱根上构造 write 工具（返回句柄以维持目录存活）。
  fn write_tool(tag: &str) -> (TempRoot, Arc<dyn AgentTool>) {
    let tmp = TempRoot::new(tag);
    let root = FsRoot::new(tmp.path()).expect("temp sandbox root exists");
    (tmp, create_write_tool(&root))
  }

  /// 取结果首个文本块内容。
  fn first_text(result: &ToolExecutionResult) -> &str {
    match &result.content[0] {
      ToolOutput::Text(t) => &t.content,
      ToolOutput::Image(_) => panic!("expected text output"),
    }
  }

  /// 场景：写入文件成功。
  ///
  /// 预期：消息含字节数（"Wrote 5 bytes to a.txt"）且内容落盘。
  #[tokio::test]
  async fn writes_file_and_reports_byte_count() {
    let (tmp, tool) = write_tool("write-ok");
    let result = tool
      .execute(
        "call-1",
        json!({ "path": "a.txt", "content": "hello" }),
        &ToolContext::detached(),
      )
      .await
      .expect("write into sandbox root should succeed");
    assert_eq!(first_text(&result), "Wrote 5 bytes to a.txt");

    // 内容确实落盘
    let written = std::fs::read_to_string(tmp.path().join("a.txt"))
      .expect("written file exists");
    assert_eq!(written, "hello");
  }

  /// 场景：父目录缺失且未开 create_dirs。
  ///
  /// 预期：返回 `tool.exec_failed`，且不创建任何目录。
  #[tokio::test]
  async fn missing_parent_without_create_dirs_fails() {
    let (tmp, tool) = write_tool("write-no-parent");
    let err = tool
      .execute(
        "call-1",
        json!({ "path": "no/such/dir/a.txt", "content": "x" }),
        &ToolContext::detached(),
      )
      .await
      .unwrap_err();
    assert_eq!(err.code(), EXEC_FAILED);
    // 失败路径不应创建任何目录
    assert!(
      !tmp.path().join("no").exists(),
      "no directory should be created on failure"
    );
  }

  /// 场景：create_dirs=true 时自动补齐缺失父目录。
  ///
  /// 预期：写入成功且深层文件内容正确。
  #[tokio::test]
  async fn create_dirs_allows_deep_path() {
    let (tmp, tool) = write_tool("write-create-dirs");
    let result = tool
      .execute(
        "call-1",
        json!({ "path": "deep/nested/a.txt", "content": "x", "create_dirs": true }),
        &ToolContext::detached(),
      )
      .await
      .expect("create_dirs should create missing parents");
    assert_eq!(first_text(&result), "Wrote 1 bytes to deep/nested/a.txt");

    let written = std::fs::read_to_string(tmp.path().join("deep/nested/a.txt"))
      .expect("file exists after create_dirs");
    assert_eq!(written, "x");
  }
}
