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

pub struct EditTool {
  /// 构造期固化的工具声明。
  schema: ToolSchema,
  /// 沙箱根。
  root: FsRoot,
}

impl EditTool {
  /// 构造并固化 schema 与沙箱根。
  pub(super) fn new(root: FsRoot) -> Self {
    Self {
      schema: ToolSchema {
        name: "edit".to_string(),
        description: "Replace exact text in an existing file. 'old' must \
match uniquely unless replace_all=true. Byte-exact match (no line-ending \
normalization)."
          .to_string(),
        input_schema: Schema(serde_json::json!({
          "type": "object",
          "properties": {
            "path": { "type": "string", "description": "Existing file path" },
            "old": { "type": "string", "description": "Exact text to replace" },
            "new": { "type": "string", "description": "Replacement text" },
            "replace_all": { "type": "boolean", "description": "Replace all occurrences (default false)" }
          },
          "required": ["path", "old", "new"],
          "additionalProperties": false
        })),
        constrained_sampling: None,
      },
      root,
    }
  }
}

#[async_trait::async_trait]
impl AgentTool for EditTool {
  fn schema(&self) -> &ToolSchema {
    &self.schema
  }

  fn label(&self) -> &str {
    "Edit"
  }

  async fn execute(
    &self,
    _tool_call_id: &str,
    args: serde_json::Value,
    ctx: &ToolContext,
  ) -> RiboResult<ToolExecutionResult> {
    // 前置校验（入参问题 → INVALID_ARGUMENTS，不记账）
    let raw = args["path"].as_str().ok_or_else(|| {
      invalid_arguments("edit", "field 'path' must be a string")
    })?;
    let old = args["old"].as_str().ok_or_else(|| {
      invalid_arguments("edit", "field 'old' must be a string")
    })?;
    let new = args["new"].as_str().ok_or_else(|| {
      invalid_arguments("edit", "field 'new' must be a string")
    })?;
    if old.is_empty() {
      return Err(invalid_arguments("edit", "'old' must not be empty"));
    }
    if old == new {
      return Err(invalid_arguments("edit", "'old' and 'new' must differ"));
    }
    let replace_all = args["replace_all"].as_bool().unwrap_or(false);
    let path = self.root.resolve_for_write(raw, "edit")?;
    check_cancel(ctx, "edit")?;
    // 读全文
    let bytes = tokio::fs::read(&path).await.map_err(|e| {
      if e.kind() == std::io::ErrorKind::NotFound {
        tracing::warn!(path = %raw, "edit tool: file not found");
        exec_failed("edit", format!("file not found: {raw}"))
      } else {
        exec_failed("edit", format!("io error ({:?}): {raw}", e.kind()))
      }
    })?;
    let text = String::from_utf8(bytes)
      .map_err(|_| exec_failed("edit", "file is not valid utf-8"))?;
    // 出现次数判定（唯一性保护）
    // 步骤：count==0 → 失败；count>1 且未开 replace_all → 失败
    let count = text.matches(old).count();
    if count == 0 {
      return Err(exec_failed("edit", format!("old text not found in {raw}")));
    }
    if count > 1 && !replace_all {
      return Err(exec_failed(
        "edit",
        format!(
          "old text matches {count} locations; set replace_all=true or narrow the match"
        ),
      ));
    }
    // 替换并写回（覆盖原文件，不备份）
    let updated = text.replace(old, new);
    tokio::fs::write(&path, updated).await.map_err(|e| {
      exec_failed("edit", format!("write back failed ({:?}): {raw}", e.kind()))
    })?;
    tracing::info!(path = %raw, occurrences = count, "edit tool: file edited");
    Ok(ToolExecutionResult {
      content: vec![ToolOutput::Text(TextContent {
        content: format!("Replaced {count} occurrence(s) in {raw}"),
        signature: None,
      })],
      ..Default::default()
    })
  }
}

/// 工厂函数。
pub fn create_edit_tool(root: &FsRoot) -> Arc<dyn AgentTool> {
  Arc::new(EditTool::new(root.clone()))
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use serde_json::json;

  use super::*;
  use crate::{
    builtin::fs::test_support::TempRoot,
    error::{EXEC_FAILED, INVALID_ARGUMENTS},
  };

  /// 在唯一临时沙箱根上构造 edit 工具（返回句柄以维持目录存活）。
  fn edit_tool(tag: &str) -> (TempRoot, Arc<dyn AgentTool>) {
    let tmp = TempRoot::new(tag);
    let root = FsRoot::new(tmp.path()).expect("temp sandbox root exists");
    (tmp, create_edit_tool(&root))
  }

  /// 取结果首个文本块内容。
  fn first_text(result: &ToolExecutionResult) -> &str {
    match &result.content[0] {
      ToolOutput::Text(t) => &t.content,
      ToolOutput::Image(_) => panic!("expected text output"),
    }
  }

  /// 场景：唯一匹配替换成功。
  ///
  /// 预期：文件内容被替换，消息为 "Replaced 1 occurrence(s) in f.txt"。
  #[tokio::test]
  async fn replaces_unique_occurrence() {
    let (tmp, tool) = edit_tool("edit-unique");
    std::fs::write(tmp.path().join("f.txt"), "hello world").expect("seed file");

    let result = tool
      .execute(
        "call-1",
        json!({ "path": "f.txt", "old": "world", "new": "rust" }),
        &ToolContext::detached(),
      )
      .await
      .expect("unique match should replace");
    assert_eq!(first_text(&result), "Replaced 1 occurrence(s) in f.txt");

    let updated =
      std::fs::read_to_string(tmp.path().join("f.txt")).expect("file exists");
    assert_eq!(updated, "hello rust");
  }

  /// 场景：多次出现未开 replace_all → 失败；开 replace_all → 全部替换。
  ///
  /// 预期：默认替换返回 `tool.exec_failed`（唯一性保护）；开启后
  /// 返回 "Replaced 2 occurrence(s)" 且内容全部更新。
  #[tokio::test]
  async fn multiple_occurrences_require_replace_all() {
    let (tmp, tool) = edit_tool("edit-multi");
    std::fs::write(tmp.path().join("f.txt"), "ab ab").expect("seed file");

    // 未开 replace_all：唯一性保护拒绝
    let err = tool
      .execute(
        "call-1",
        json!({ "path": "f.txt", "old": "ab", "new": "cd" }),
        &ToolContext::detached(),
      )
      .await
      .unwrap_err();
    assert_eq!(err.code(), EXEC_FAILED);

    // 开 replace_all：两处全部替换
    let result = tool
      .execute(
        "call-1",
        json!({ "path": "f.txt", "old": "ab", "new": "cd", "replace_all": true }),
        &ToolContext::detached(),
      )
      .await
      .expect("replace_all should replace every occurrence");
    assert_eq!(first_text(&result), "Replaced 2 occurrence(s) in f.txt");

    let updated =
      std::fs::read_to_string(tmp.path().join("f.txt")).expect("file exists");
    assert_eq!(updated, "cd cd");
  }

  /// 场景：old 与 new 相同属入参错误。
  ///
  /// 预期：执行前置校验拦截，返回 `tool.invalid_arguments`。
  #[tokio::test]
  async fn identical_old_and_new_is_invalid() {
    let (_tmp, tool) = edit_tool("edit-same");
    let err = tool
      .execute(
        "call-1",
        json!({ "path": "f.txt", "old": "x", "new": "x" }),
        &ToolContext::detached(),
      )
      .await
      .unwrap_err();
    assert_eq!(err.code(), INVALID_ARGUMENTS);
  }

  /// 场景：old 文本在文件中不存在。
  ///
  /// 预期：返回 `tool.exec_failed`，文件内容保持不变。
  #[tokio::test]
  async fn missing_old_text_fails_without_change() {
    let (tmp, tool) = edit_tool("edit-nomatch");
    std::fs::write(tmp.path().join("f.txt"), "hello").expect("seed file");

    let err = tool
      .execute(
        "call-1",
        json!({ "path": "f.txt", "old": "zzz", "new": "y" }),
        &ToolContext::detached(),
      )
      .await
      .unwrap_err();
    assert_eq!(err.code(), EXEC_FAILED);

    // 失败后文件内容不被破坏
    let content =
      std::fs::read_to_string(tmp.path().join("f.txt")).expect("file exists");
    assert_eq!(content, "hello");
  }
}
