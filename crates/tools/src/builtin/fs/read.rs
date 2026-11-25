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

use super::{
  FsRoot, check_cancel,
  constants::{MAX_BYTES, MAX_LINES},
  detect_image_mime,
};

/// read 工具：按行窗口读取 UTF-8 文本文件。
pub struct ReadTool {
  /// 构造期固化的工具声明。
  schema: ToolSchema,
  /// 沙箱根。
  root: FsRoot,
}

impl ReadTool {
  /// 构造并固化 schema 与沙箱根。
  pub(super) fn new(root: FsRoot) -> Self {
    Self {
      schema: ToolSchema {
        name: "read".to_string(),
        description: "Read a text file as UTF-8. Supports line-window paging \
  via 1-indexed offset and limit. Large files return a truncated preview with \
  a continuation hint."
          .to_string(),
        input_schema: Schema(serde_json::json!({
          "type": "object",
          "properties": {
            "path": { "type": "string", "description": "File path relative to workspace root" },
            "offset": { "type": "integer", "minimum": 1, "description": "1-indexed start line" },
            "limit": { "type": "integer", "minimum": 1, "description": "Max lines to read" }
          },
          "required": ["path"],
          "additionalProperties": false
        })),
        constrained_sampling: None,
      },
      root,
    }
  }
}

#[async_trait::async_trait]
impl AgentTool for ReadTool {
  /// 返回构造期固化的工具声明。
  fn schema(&self) -> &ToolSchema {
    &self.schema
  }

  /// 展示名。
  fn label(&self) -> &str {
    "Read"
  }

  /// 按行窗口读取文件内容：支持 1 起始的 offset/limit 分页，
  /// 超出行数与字节双上限时截断并附续读提示。
  async fn execute(
    &self,
    _tool_call_id: &str,
    args: serde_json::Value,
    ctx: &ToolContext,
  ) -> RiboResult<ToolExecutionResult> {
    let raw = args["path"].as_str().ok_or_else(|| {
      invalid_arguments("read", "field 'path' must be a string")
    })?;
    let path = self.root.resolve(raw)?;
    check_cancel(ctx, "read")?;
    let bytes = tokio::fs::read(&path).await.map_err(|e| {
      // 不存在的文件对模型是可修正问题：warn 记录（日志表）
      if e.kind() == std::io::ErrorKind::NotFound {
        tracing::warn!(path = %raw, "read tool: file not found");
      }
      let reason = if e.kind() == std::io::ErrorKind::NotFound {
        format!("file not found: {raw}")
      } else {
        format!("io error ({:?}): {raw}", e.kind())
      };
      exec_failed("read", &reason)
    })?;
    // 图片文件：本期不返回图片内容，给出明确提示（收敛决策 3）
    if detect_image_mime(&bytes).is_some() {
      return Err(exec_failed(
        "read",
        "file is an image; image output is not supported yet",
      ));
    }
    let text = String::from_utf8(bytes)
      .map_err(|_| exec_failed("read", "file is not valid utf-8"))?;
    // 行窗口
    // 按行切分（\n 语义对齐 pi-tools read.rs 的 split 行为）
    let lines: Vec<&str> = text.lines().collect();
    let total_lines = lines.len();
    if total_lines == 0 {
      // 空文件：返回空文本，不视为错误
      return Ok(ToolExecutionResult {
        content: vec![ToolOutput::Text(TextContent {
          content: String::new(),
          signature: None,
        })],
        details: Some(serde_json::json!({ "total_lines": 0 })),
        ..Default::default()
      });
    }
    let offset = args["offset"].as_u64().unwrap_or(1) as usize;
    if offset > total_lines {
      return Err(exec_failed(
        "read",
        format!("offset {offset} beyond end of file ({total_lines} lines)"),
      ));
    }
    let start = offset - 1;
    let limit = args["limit"]
      .as_u64()
      .map(|v| v as usize)
      .unwrap_or(usize::MAX);
    // 窗口 + 双上限截断（行数 / 字节，先到者生效）
    let mut window = String::new();
    let mut taken = 0usize;
    for line in lines.iter().skip(start).take(limit).take(MAX_LINES) {
      // 字节上限按"行内容 + 换行"计
      if window.len() + line.len() + 1 > MAX_BYTES {
        break;
      }
      window.push_str(line);
      window.push('\n');
      taken += 1;
    }
    let end = start + taken;
    // 续读提示（窗口未及文末时附加）
    if end < total_lines {
      window.push_str(&format!(
        "\n[Showing lines {}-{} of {}. Use offset={} to continue.]",
        start + 1,
        end,
        total_lines,
        end + 1
      ));
    }
    Ok(ToolExecutionResult {
      content: vec![ToolOutput::Text(TextContent {
        content: window,
        signature: None,
      })],
      details: Some(
        serde_json::json!({ "total_lines": total_lines, "showing_lines": [start + 1, end] }),
      ),
      ..Default::default()
    })
  }
}

/// 工厂函数。
pub fn create_read_tool(root: &FsRoot) -> Arc<dyn AgentTool> {
  Arc::new(ReadTool::new(root.clone()))
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use serde_json::json;

  use super::*;
  use crate::{builtin::fs::test_support::TempRoot, error::EXEC_FAILED};

  /// 在唯一临时沙箱根上构造 read 工具（返回句柄以维持目录存活）。
  fn read_tool(tag: &str) -> (TempRoot, Arc<dyn AgentTool>) {
    let tmp = TempRoot::new(tag);
    let root = FsRoot::new(tmp.path()).expect("temp sandbox root exists");
    (tmp, create_read_tool(&root))
  }

  /// 取结果首个文本块内容。
  fn first_text(result: &ToolExecutionResult) -> &str {
    match &result.content[0] {
      ToolOutput::Text(t) => &t.content,
      ToolOutput::Image(_) => panic!("expected text output"),
    }
  }

  /// 场景：读取已写入的多行文件。
  ///
  /// 预期：内容为全部三行（各带换行），details 整体等于
  /// `{ total_lines: 3, showing_lines: [1, 3] }`。
  #[tokio::test]
  async fn reads_written_lines_with_total() {
    let (tmp, tool) = read_tool("read-ok");
    std::fs::write(tmp.path().join("a.txt"), "line1\nline2\nline3\n")
      .expect("seed file");

    let result = tool
      .execute(
        "call-1",
        json!({ "path": "a.txt" }),
        &ToolContext::detached(),
      )
      .await
      .expect("existing text file should read");
    assert_eq!(first_text(&result), "line1\nline2\nline3\n");
    // 整对象比较：行数与展示窗口一并校验
    assert_eq!(
      result.details.unwrap(),
      json!({ "total_lines": 3, "showing_lines": [1, 3] })
    );
  }

  /// 场景：offset 超出文件总行数。
  ///
  /// 预期：3 行文件请求 offset=10 → 返回 `tool.exec_failed`。
  #[tokio::test]
  async fn offset_beyond_total_lines_fails() {
    let (tmp, tool) = read_tool("read-offset");
    std::fs::write(tmp.path().join("a.txt"), "l1\nl2\nl3\n")
      .expect("seed file");

    let err = tool
      .execute(
        "call-1",
        json!({ "path": "a.txt", "offset": 10 }),
        &ToolContext::detached(),
      )
      .await
      .unwrap_err();
    assert_eq!(err.code(), EXEC_FAILED);
  }

  /// 场景：读取不存在的文件。
  ///
  /// 预期：返回 `tool.exec_failed`。
  #[tokio::test]
  async fn missing_file_maps_to_exec_failed() {
    let (_tmp, tool) = read_tool("read-missing");
    let err = tool
      .execute(
        "call-1",
        json!({ "path": "ghost.txt" }),
        &ToolContext::detached(),
      )
      .await
      .unwrap_err();
    assert_eq!(err.code(), EXEC_FAILED);
  }
}
