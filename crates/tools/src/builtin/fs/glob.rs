use std::sync::Arc;

use ra_core::{
  error::RiboResult,
  message::{Schema, TextContent, ToolSchema},
};

use crate::{
  context::ToolContext,
  error::invalid_arguments,
  traits::AgentTool,
  types::{ToolExecutionResult, ToolOutput},
};

use super::{FsRoot, check_cancel};

/// glob 工具：模式匹配文件清单。
pub struct GlobTool {
  /// 构造期固化的工具声明。
  schema: ToolSchema,
  /// 沙箱根。
  root: FsRoot,
}

impl GlobTool {
  /// 构造并固化 schema 与沙箱根。
  pub(super) fn new(root: FsRoot) -> Self {
    Self {
      schema: ToolSchema {
        name: "glob".to_string(),
        description: "Find files matching a glob pattern (*, ?, [...], ** \
for cross-directory). Returns one path per line, sorted."
          .to_string(),
        input_schema: Schema(serde_json::json!({
          "type": "object",
          "properties": {
            "pattern": { "type": "string", "description": "Glob pattern, e.g. '**/*.rs'" },
            "cwd": { "type": "string", "description": "Base directory for matching (default workspace root)" },
            "max_results": { "type": "integer", "minimum": 1, "description": "Result limit (default 200)" }
          },
          "required": ["pattern"],
          "additionalProperties": false
        })),
        constrained_sampling: None,
      },
      root,
    }
  }
}

#[async_trait::async_trait]
impl AgentTool for GlobTool {
  fn schema(&self) -> &ToolSchema {
    &self.schema
  }

  fn label(&self) -> &str {
    "Glob"
  }

  async fn execute(
    &self,
    _tool_call_id: &str,
    args: serde_json::Value,
    ctx: &ToolContext,
  ) -> RiboResult<ToolExecutionResult> {
    // ===== 取参与基准目录解析 =====
    let pattern = args["pattern"].as_str().ok_or_else(|| {
      invalid_arguments("glob", "field 'pattern' must be a string")
    })?;
    let cwd_raw = args["cwd"].as_str().unwrap_or(".");
    let max_results = args["max_results"].as_u64().unwrap_or(200) as usize;
    let cwd = self.root.resolve(cwd_raw)?;
    check_cancel(ctx, "glob")?;
    // ===== 模式编译 =====
    // 完整模式 = cwd 绝对路径 join pattern（分隔符经 Path 抽象）；
    // 模式非法属入参问题 → INVALID_ARGUMENTS
    let full = cwd.join(pattern);
    let paths = glob::glob(&full.to_string_lossy()).map_err(|e| {
      invalid_arguments("glob", format!("invalid pattern: {e}"))
    })?;
    // ===== 迭代收集（IO 项跳过不整体失败） =====
    let mut results: Vec<String> = Vec::new();
    let mut truncated = false;
    for entry in paths {
      match entry {
        Ok(p) => {
          if results.len() == max_results {
            truncated = true;
            break;
          }
          // 展示为相对 cwd 的路径
          let shown = p.strip_prefix(&cwd).unwrap_or(p.as_path());
          results.push(shown.to_string_lossy().into_owned());
        }
        Err(e) => {
          // 单个 IO 项失败：跳过并记 debug（日志表）
          tracing::debug!(reason = %e, "glob entry skipped");
        }
      }
    }
    results.sort();
    // ===== 输出（空结果不视为错误） =====
    let mut text = if results.is_empty() {
      "(no matches)".to_string()
    } else {
      results.join("\n")
    };
    if truncated {
      text.push_str("\n[... more matches truncated]");
    }
    Ok(ToolExecutionResult {
      content: vec![ToolOutput::Text(TextContent {
        content: text,
        signature: None,
      })],
      details: Some(
        serde_json::json!({ "matches": results.len(), "truncated": truncated }),
      ),
      ..Default::default()
    })
  }
}

/// 工厂函数。
pub fn create_glob_tool(root: &FsRoot) -> Arc<dyn AgentTool> {
  Arc::new(GlobTool::new(root.clone()))
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use serde_json::json;

  use super::*;
  use crate::{builtin::fs::test_support::TempRoot, error::INVALID_ARGUMENTS};

  /// 在唯一临时沙箱根上构造 glob 工具（返回句柄以维持目录存活）。
  fn glob_tool(tag: &str) -> (TempRoot, Arc<dyn AgentTool>) {
    let tmp = TempRoot::new(tag);
    let root = FsRoot::new(tmp.path()).expect("temp sandbox root exists");
    (tmp, create_glob_tool(&root))
  }

  /// 取结果首个文本块内容。
  fn first_text(result: &ToolExecutionResult) -> &str {
    match &result.content[0] {
      ToolOutput::Text(t) => &t.content,
      ToolOutput::Image(_) => panic!("expected text output"),
    }
  }

  /// 场景：模式命中文件并按名称排序输出相对路径。
  ///
  /// 预期："*.rs" 只命中 a.rs 与 b.rs，输出为排序后的相对路径。
  #[tokio::test]
  async fn pattern_matches_and_sorts_relative_paths() {
    let (tmp, tool) = glob_tool("glob-match");
    std::fs::write(tmp.path().join("b.rs"), "1").expect("seed b.rs");
    std::fs::write(tmp.path().join("a.rs"), "1").expect("seed a.rs");
    std::fs::write(tmp.path().join("note.txt"), "1").expect("seed note.txt");

    let result = tool
      .execute(
        "call-1",
        json!({ "pattern": "*.rs" }),
        &ToolContext::detached(),
      )
      .await
      .expect("glob should succeed");
    assert_eq!(first_text(&result), "a.rs\nb.rs");
  }

  /// 场景：无命中。
  ///
  /// 预期：输出 "(no matches)"，不视为错误。
  #[tokio::test]
  async fn no_match_yields_placeholder() {
    let (_tmp, tool) = glob_tool("glob-empty");
    let result = tool
      .execute(
        "call-1",
        json!({ "pattern": "zzz*.nothing" }),
        &ToolContext::detached(),
      )
      .await
      .expect("empty result is not an error");
    assert_eq!(first_text(&result), "(no matches)");
  }

  /// 场景：非法模式。
  ///
  /// 预期：未闭合的字符类 "[" → `tool.invalid_arguments`。
  #[tokio::test]
  async fn invalid_pattern_maps_to_invalid_arguments() {
    let (_tmp, tool) = glob_tool("glob-bad");
    let err = tool
      .execute(
        "call-1",
        json!({ "pattern": "[" }),
        &ToolContext::detached(),
      )
      .await
      .unwrap_err();
    assert_eq!(err.code(), INVALID_ARGUMENTS);
  }
}
