//! calculator 工具：数学表达式求值（基于 `meval`）。

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

/// calculator 工具：安全的数学表达式求值器。
///
/// 表达式由 `meval` 解析（纯求值，无图灵完备风险），支持四则运算、
/// 常用函数与常量；NaN / 无穷大归一为文本表示。
pub struct CalculatorTool {
  /// 构造期固化的工具声明。
  schema: ToolSchema,
}

impl CalculatorTool {
  fn new() -> Self {
    Self {
      schema: ToolSchema {
        name: "calculator".to_string(),
        description: "Evaluate a mathematical expression. Supports arithmetic \
(+, -, *, /, %, ^), parentheses, functions (sin, cos, tan, log, ln, sqrt, pow, \
abs, exp, floor, ceil, round) and constants (pi, e). Returns '<expression> = <value>'."
          .to_string(),
        input_schema: Schema(serde_json::json!({
          "type": "object",
          "properties": {
            "expression": {
              "type": "string",
              "description": "Math expression, e.g. '(2+3)*sin(0.5)'"
            }
          },
          "required": ["expression"],
          "additionalProperties": false
        })),
        constrained_sampling: None,
      },
    }
  }
}

#[async_trait::async_trait]
impl AgentTool for CalculatorTool {
  /// 返回构造期固化的工具声明。
  fn schema(&self) -> &ToolSchema {
    &self.schema
  }

  /// 展示名。
  fn label(&self) -> &str {
    "Calculator"
  }

  /// 求值表达式并回传结果。
  ///
  /// 输出三态：有限值直接展示；NaN / ±inf 归一为文本；`details`
  /// 中的数值字段在无法表示为 JSON Number 时退化为文本。
  async fn execute(
    &self,
    _tool_call_id: &str,
    args: serde_json::Value,
    _ctx: &ToolContext,
  ) -> RiboResult<ToolExecutionResult> {
    // ===== 取参：缺失或非字符串 → INVALID_ARGUMENTS（不记账） =====
    let expr =
      args
        .get("expression")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
          invalid_arguments("calculator", "field 'expression' must be a string")
        })?;
    // ===== 求值：语法 / 语义错误 → EXEC_FAILED =====
    let result = meval::eval_str(expr).map_err(|e| {
      exec_failed("calculator", format!("expression error: {e}"))
    })?;

    // ===== 结果归一：NaN / 无穷大转文本表示 =====
    let value_text = if result.is_nan() {
      "NaN".to_string()
    } else if result.is_infinite() {
      if result > 0.0 {
        "inf".to_string()
      } else {
        "-inf".to_string()
      }
    } else {
      format!("{result}")
    };

    // details 尽量给数值（JSON Number），超出可表示范围时退化为文本
    let details = serde_json::Number::from_f64(result)
      .map(|n| serde_json::json!({ "result": n }))
      .unwrap_or_else(|| serde_json::json!({ "result": value_text }));
    Ok(ToolExecutionResult {
      content: vec![ToolOutput::Text(TextContent {
        content: format!("{expr} = {value_text}"),
        signature: None,
      })],
      details: Some(details),
      ..Default::default()
    })
  }
}

/// 工厂函数：构造 calculator 工具实例。
pub fn create_calculator_tool() -> Arc<dyn AgentTool> {
  Arc::new(CalculatorTool::new())
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::*;
  use crate::error::{EXEC_FAILED, INVALID_ARGUMENTS};

  /// 取结果首个文本块内容。
  fn first_text(result: &ToolExecutionResult) -> &str {
    match &result.content[0] {
      ToolOutput::Text(t) => &t.content,
      ToolOutput::Image(_) => panic!("expected text output"),
    }
  }

  /// 场景：基础四则表达式求值。
  ///
  /// 预期：输出 "<表达式> = 3"，details 携带数值结果 3.0。
  #[tokio::test]
  async fn evaluates_arithmetic_expression() {
    let tool = create_calculator_tool();
    let result = tool
      .execute(
        "call-1",
        json!({ "expression": "1+2" }),
        &ToolContext::detached(),
      )
      .await
      .expect("1+2 should evaluate");
    assert_eq!(first_text(&result), "1+2 = 3");
    // details 尽量给数值：3 可表示为 JSON Number
    assert_eq!(result.details, Some(json!({ "result": 3.0 })));
  }

  /// 场景：NaN / 无穷大归一为文本表示。
  ///
  /// 预期：0/0 输出 "NaN"、1/0 输出 "inf"；两者的数值均无法表示为
  /// JSON Number，details 字段退化为文本。
  #[tokio::test]
  async fn normalizes_nan_and_infinity() {
    let tool = create_calculator_tool();

    let result = tool
      .execute(
        "call-1",
        json!({ "expression": "0/0" }),
        &ToolContext::detached(),
      )
      .await
      .expect("0/0 evaluates to NaN");
    assert_eq!(first_text(&result), "0/0 = NaN");
    assert_eq!(result.details, Some(json!({ "result": "NaN" })));

    let result = tool
      .execute(
        "call-1",
        json!({ "expression": "1/0" }),
        &ToolContext::detached(),
      )
      .await
      .expect("1/0 evaluates to +inf");
    assert_eq!(first_text(&result), "1/0 = inf");
    assert_eq!(result.details, Some(json!({ "result": "inf" })));
  }

  /// 场景：非法表达式。
  ///
  /// 预期：语法错误的表达式返回 `tool.exec_failed`。
  #[tokio::test]
  async fn invalid_expression_maps_to_exec_failed() {
    let tool = create_calculator_tool();
    let err = tool
      .execute(
        "call-1",
        json!({ "expression": "1+" }),
        &ToolContext::detached(),
      )
      .await
      .unwrap_err();
    assert_eq!(err.code(), EXEC_FAILED);
  }

  /// 场景：expression 缺失或非字符串。
  ///
  /// 预期：返回 `tool.invalid_arguments`。
  #[tokio::test]
  async fn missing_or_non_string_expression_is_invalid() {
    let tool = create_calculator_tool();
    for args in [json!({}), json!({ "expression": 42 })] {
      let err = tool
        .execute("call-1", args, &ToolContext::detached())
        .await
        .unwrap_err();
      assert_eq!(err.code(), INVALID_ARGUMENTS);
    }
  }
}
