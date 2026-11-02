//! echo 工具：原样回显文本（连通性自检）。

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

/// echo 工具：把输入文本原样回显。
///
/// 主要用于端到端联调时验证工具调用链路（注册 → 校验 → 执行 → 回灌）
/// 是否通畅。
pub struct EchoTool {
  /// 构造期固化的工具声明。
  schema: ToolSchema,
}

impl EchoTool {
  fn new() -> Self {
    Self {
      schema: ToolSchema {
        name: "echo".to_string(),
        description: "Echo the given text back verbatim. Useful for testing tool connectivity.".to_string(),
        input_schema: Schema(serde_json::json!({
          "type": "object",
          "properties": {
            "text": { "type": "string", "description": "Text to echo back" }
          },
          "required": ["text"],
          "additionalProperties": false
        })),
        constrained_sampling: None
      }
    }
  }
}

#[async_trait::async_trait]
impl AgentTool for EchoTool {
  /// 返回构造期固化的工具声明。
  fn schema(&self) -> &ToolSchema {
    &self.schema
  }

  /// 展示名。
  fn label(&self) -> &str {
    "Echo"
  }

  /// 回显输入文本。
  async fn execute(
    &self,
    _tool_call_id: &str,
    args: serde_json::Value,
    _ctx: &ToolContext,
  ) -> RiboResult<ToolExecutionResult> {
    // 取参：缺失或非字符串 → INVALID_ARGUMENTS（不记账）
    let text = args.get("text").and_then(|v| v.as_str()).ok_or_else(|| {
      invalid_arguments("echo", "field 'text' must be a string")
    })?;
    Ok(ToolExecutionResult {
      content: vec![ToolOutput::Text(TextContent {
        content: text.to_string(),
        signature: None,
      })],
      ..Default::default()
    })
  }
}

/// 工厂函数：构造 echo 工具实例。
pub fn create_echo_tool() -> Arc<dyn AgentTool> {
  Arc::new(EchoTool::new())
}

#[cfg(test)]
mod tests {
  use ra_core::message::ToolCall;
  use serde_json::json;

  use super::*;
  use crate::error::INVALID_ARGUMENTS;

  /// 取结果首个文本块内容。
  fn first_text(result: &ToolExecutionResult) -> &str {
    match &result.content[0] {
      ToolOutput::Text(t) => &t.content,
      ToolOutput::Image(_) => panic!("expected text output"),
    }
  }

  /// 场景：echo 原样回显输入文本。
  ///
  /// 预期：输出文本与入参 text 完全一致（含非 ASCII 字符）。
  #[tokio::test]
  async fn echoes_text_back_verbatim() {
    let tool = create_echo_tool();
    let result = tool
      .execute(
        "call-1",
        json!({ "text": "hello 世界" }),
        &ToolContext::detached(),
      )
      .await
      .expect("echo should succeed");
    assert_eq!(first_text(&result), "hello 世界");
  }

  /// 场景：text 缺失或非字符串。
  ///
  /// 预期：两种入参都返回 `tool.invalid_arguments`，不 panic。
  #[tokio::test]
  async fn rejects_missing_or_non_string_text() {
    let tool = create_echo_tool();
    // 缺字段 / 类型错误：均属入参问题
    for args in [json!({}), json!({ "text": 123 })] {
      let call = ToolCall {
        id: "call-1".to_string(),
        name: "echo".to_string(),
        arguments: args,
        signature: None,
        namespace: None,
      };
      let err = tool
        .execute(&call.id, call.arguments, &ToolContext::detached())
        .await
        .unwrap_err();
      assert_eq!(err.code(), INVALID_ARGUMENTS);
    }
  }
}
