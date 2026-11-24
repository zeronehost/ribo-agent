//! calculator 工具：数学表达式求值（基于 `evalexpr`）。

use std::sync::Arc;

use evalexpr::{HashMapContext, Value, context_map, eval_with_context};
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
/// 表达式由 `evalexpr` 解析（纯求值，无图灵完备风险）。数值语义：
/// 两个整数字面量按 i64 整数运算（除法截断、除零 / 溢出报错）；
/// 任一侧为浮点则按 f64 运算（保留 IEEE 语义，除零得 inf / NaN）。
/// NaN / 无穷大结果归一为文本表示。
pub struct CalculatorTool {
  /// 构造期固化的工具声明。
  schema: ToolSchema,
  /// 构造期固化的求值上下文：常量（pi / e / tau）与 abs 函数；
  /// `math::*` 与 floor / round / ceil 等内置函数经 Context 默认
  /// 回落机制可用，无须重复注册。
  context: HashMapContext,
}

impl CalculatorTool {
  fn new() -> Self {
    // ===== 求值上下文：evalexpr 无内置 pi / e / abs，构造期注入 =====
    // 静态字面构造，失败只可能是宏用法错误，构造期 expect 合理
    let context = context_map! {
      "pi" => float core::f64::consts::PI,
      "e" => float core::f64::consts::E,
      "tau" => float core::f64::consts::TAU,
      "abs" => Function::new(
        |argument: &Value| -> evalexpr::EvalexprResult<Value> {
          let number: f64 = argument.as_number()?;
          Ok(Value::Float(number.abs()))
        },
      ),
    }
    .expect("calculator context is statically valid");

    Self {
      schema: ToolSchema {
        name: "calculator".to_string(),
        description: "Evaluate a mathematical expression. Supports arithmetic \
(+, -, *, /, %, ^ where ^ is exponentiation), parentheses, functions (math::sin, \
math::cos, math::tan, math::ln, math::log2, math::log10, math::exp, math::sqrt, \
math::pow, abs, floor, ceil, round) and constants (pi, e, tau). Integer operands \
use integer arithmetic (division truncates; division by zero fails). Returns \
'<expression> = <value>'."
          .to_string(),
        input_schema: Schema(serde_json::json!({
          "type": "object",
          "properties": {
            "expression": {
              "type": "string",
              "description": "Math expression, e.g. '(2+3)*math::sin(0.5)'"
            }
          },
          "required": ["expression"],
          "additionalProperties": false
        })),
        constrained_sampling: None,
      },
      context,
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
  /// 输出三态：整数直接展示；浮点有限值直接展示，NaN / ±inf 归一为
  /// 文本；非数值结果（布尔 / 字符串等）拒绝并回 `exec_failed`。
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
    // ===== 求值：语法 / 语义错误（含整数除零、溢出）→ EXEC_FAILED =====
    let value = eval_with_context(expr, &self.context).map_err(|e| {
      exec_failed("calculator", format!("expression error: {e}"))
    })?;

    // ===== 结果归一：仅接受数值结果；NaN / 无穷大转文本表示 =====
    let (value_text, details) = match value {
      Value::Int(i) => (format!("{i}"), serde_json::json!({ "result": i })),
      Value::Float(f) if f.is_nan() => {
        ("NaN".to_string(), serde_json::json!({ "result": "NaN" }))
      }
      Value::Float(f) if f.is_infinite() => {
        let text = if f > 0.0 { "inf" } else { "-inf" };
        (text.to_string(), serde_json::json!({ "result": text }))
      }
      Value::Float(f) => {
        // details 尽量给数值（JSON Number），超出可表示范围时退化为文本
        let details = serde_json::Number::from_f64(f)
          .map(|n| serde_json::json!({ "result": n }))
          .unwrap_or_else(|| serde_json::json!({ "result": format!("{f}") }));
        (format!("{f}"), details)
      }
      // 布尔 / 字符串 / 元组等非数值结果不符合计算器语义
      _ => {
        return Err(exec_failed(
          "calculator",
          "expression must evaluate to a number",
        ));
      }
    };

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
  /// 预期：整数字面量按整数运算，输出 "<表达式> = 3"，details 携带
  /// 整数结果 3；整数除法截断。
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
    // 整数结果以 JSON Number（i64）表示
    assert_eq!(result.details, Some(json!({ "result": 3 })));

    // 整数除法语义：7/2 截断为 3（任一侧加小数点才得 3.5）
    let result = tool
      .execute(
        "call-1",
        json!({ "expression": "7/2" }),
        &ToolContext::detached(),
      )
      .await
      .expect("7/2 should evaluate");
    assert_eq!(first_text(&result), "7/2 = 3");
  }

  /// 场景：浮点运算保留 IEEE 语义，NaN / 无穷大归一为文本。
  ///
  /// 预期：0.0/0.0 输出 "NaN"、1.0/0.0 输出 "inf"；两者的数值均无法
  /// 表示为 JSON Number，details 字段退化为文本。
  #[tokio::test]
  async fn normalizes_nan_and_infinity() {
    let tool = create_calculator_tool();

    let result = tool
      .execute(
        "call-1",
        json!({ "expression": "0.0/0.0" }),
        &ToolContext::detached(),
      )
      .await
      .expect("0.0/0.0 evaluates to NaN");
    assert_eq!(first_text(&result), "0.0/0.0 = NaN");
    assert_eq!(result.details, Some(json!({ "result": "NaN" })));

    let result = tool
      .execute(
        "call-1",
        json!({ "expression": "1.0/0.0" }),
        &ToolContext::detached(),
      )
      .await
      .expect("1.0/0.0 evaluates to +inf");
    assert_eq!(first_text(&result), "1.0/0.0 = inf");
    assert_eq!(result.details, Some(json!({ "result": "inf" })));
  }

  /// 场景：整数除零报错（evalexpr 整数运算用 checked_div）。
  ///
  /// 预期：`1/0` 返回 `tool.exec_failed`；与浮点除零（得 inf）语义区分。
  #[tokio::test]
  async fn integer_division_by_zero_maps_to_exec_failed() {
    let tool = create_calculator_tool();
    let err = tool
      .execute(
        "call-1",
        json!({ "expression": "1/0" }),
        &ToolContext::detached(),
      )
      .await
      .unwrap_err();
    assert_eq!(err.code(), EXEC_FAILED);
  }

  /// 场景：常量、幂运算与注入的 abs 函数。
  ///
  /// 预期：pi / e 可用；`^` 为幂；abs 经上下文注入可用；
  /// 浮点结果以最短文本表示。
  #[tokio::test]
  async fn constants_power_and_abs_are_available() {
    let tool = create_calculator_tool();

    let result = tool
      .execute(
        "call-1",
        json!({ "expression": "2^10" }),
        &ToolContext::detached(),
      )
      .await
      .expect("2^10 should evaluate");
    assert_eq!(first_text(&result), "2^10 = 1024");

    let result = tool
      .execute(
        "call-1",
        json!({ "expression": "abs(-2.5)" }),
        &ToolContext::detached(),
      )
      .await
      .expect("abs should evaluate");
    assert_eq!(first_text(&result), "abs(-2.5) = 2.5");

    // 浮点与常量：floor(pi) = 3（floor 返回 Float，文本展示为整数形态）
    let result = tool
      .execute(
        "call-1",
        json!({ "expression": "floor(pi)" }),
        &ToolContext::detached(),
      )
      .await
      .expect("floor(pi) should evaluate");
    assert_eq!(first_text(&result), "floor(pi) = 3");
  }

  /// 场景：非数值结果（布尔 / 字符串）拒绝。
  ///
  /// 预期：表达式产出布尔或字符串时返回 `tool.exec_failed`（计算器
  /// 语义只接受数值结果）。
  #[tokio::test]
  async fn non_numeric_result_maps_to_exec_failed() {
    let tool = create_calculator_tool();
    for expression in ["1 < 2", "\"hello\""] {
      let err = tool
        .execute(
          "call-1",
          json!({ "expression": expression }),
          &ToolContext::detached(),
        )
        .await
        .unwrap_err();
      assert_eq!(err.code(), EXEC_FAILED);
    }
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
