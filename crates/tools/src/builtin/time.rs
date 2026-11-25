//! current_time 工具：时区感知的当前时间查询。

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

/// current_time 工具：返回 RFC 3339 格式的当前时间，支持 IANA 时区名。
pub struct CurrentTimeTool {
  /// 构造期固化的工具声明。
  schema: ToolSchema,
}

impl CurrentTimeTool {
  fn new() -> Self {
    Self {
      schema: ToolSchema {
        name: "current_time".to_string(),
        description: "Get the current local time in RFC 3339 format. \
Optionally accepts an IANA timezone name (e.g. 'Asia/Shanghai', 'UTC'); \
defaults to the system timezone."
          .to_string(),
        input_schema: Schema(serde_json::json!({
          "type": "object",
          "properties": {
            "timezone": {
              "type": "string",
              "description": "IANA timezone name, e.g. 'Asia/Shanghai'"
            }
          },
          "additionalProperties": false
        })),
        constrained_sampling: None,
      },
    }
  }
}

#[async_trait::async_trait]
impl AgentTool for CurrentTimeTool {
  /// 返回构造期固化的工具声明。
  fn schema(&self) -> &ToolSchema {
    &self.schema
  }

  /// 展示名。
  fn label(&self) -> &str {
    "Current Time"
  }

  /// 查询当前时间：先解析时区（非法时区名 → 入参错误，不记账），
  /// 再按 RFC 3339 纯格式输出。
  async fn execute(
    &self,
    _tool_call_id: &str,
    args: serde_json::Value,
    _ctx: &ToolContext,
  ) -> RiboResult<ToolExecutionResult> {
    // 时区解析
    // IANA 名非法属入参问题（模型可修正后重试）→ INVALID_ARGUMENTS，
    // 不计入熔断记账
    let tz_name = args.get("timezone").and_then(|v| v.as_str());
    let now = match tz_name {
      Some(name) => {
        let tz = jiff::tz::TimeZone::get(name).map_err(|e| {
          invalid_arguments(
            "current_time",
            format!("unknown IANA timezone '{name}': {e}"),
          )
        })?;
        jiff::Zoned::now().with_time_zone(tz)
      }
      // 缺省用系统时区
      None => jiff::Zoned::now(),
    };
    // 格式化输出
    // 纯 RFC 3339（jiff 默认 Display 会带 " [2025]" 类注释后缀，
    // 用 strftime 排除，保证模型输出解析稳定）
    let text = now.strftime("%Y-%m-%dT%H:%M:%S%:z").to_string();
    Ok(ToolExecutionResult {
      content: vec![ToolOutput::Text(TextContent {
        content: text,
        signature: None,
      })],
      details: Some(serde_json::json!({
        "unix_ms": now.timestamp().as_millisecond(),
        "timezone": tz_name.unwrap_or("system"),
      })),
      ..Default::default()
    })
  }
}

/// 工厂函数：构造 current_time 工具实例。
pub fn create_current_time_tool() -> Arc<dyn AgentTool> {
  Arc::new(CurrentTimeTool::new())
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::*;
  use crate::error::INVALID_ARGUMENTS;

  /// 取结果首个文本块内容。
  fn first_text(result: &ToolExecutionResult) -> String {
    match &result.content[0] {
      ToolOutput::Text(t) => t.content.clone(),
      ToolOutput::Image(_) => panic!("expected text output"),
    }
  }

  /// 场景：缺省时区输出 RFC 3339 纯格式。
  ///
  /// 预期：内容可按 "%Y-%m-%dT%H:%M:%S%:z" 回解析（无 jiff Display
  /// 的注释后缀），details 标记时区为 system。
  #[tokio::test]
  async fn default_output_is_rfc3339() {
    let tool = create_current_time_tool();
    let result = tool
      .execute("call-1", json!({}), &ToolContext::detached())
      .await
      .expect("current_time should succeed");
    let text = first_text(&result);

    // 纯 RFC 3339：jiff 原生严格解析可通过（对注释后缀等尾部
    // 多余内容会直接报错，因此同时验证了无 Display 注释后缀）
    text
      .parse::<jiff::Timestamp>()
      .unwrap_or_else(|e| panic!("output {text:?} is not plain RFC 3339: {e}"));
    assert_eq!(result.details.unwrap()["timezone"], "system");
  }

  /// 场景：显式 IANA 时区（UTC）。
  ///
  /// 预期：输出偏移为 +00:00，details 记录所传时区名。
  #[tokio::test]
  async fn honours_explicit_iana_timezone() {
    let tool = create_current_time_tool();
    let result = tool
      .execute(
        "call-1",
        json!({ "timezone": "UTC" }),
        &ToolContext::detached(),
      )
      .await
      .expect("UTC is a valid IANA timezone");
    let text = first_text(&result);
    assert!(
      text.ends_with("+00:00"),
      "UTC output should end with +00:00, got {text}"
    );
    assert_eq!(result.details.unwrap()["timezone"], "UTC");
  }

  /// 场景：非法 IANA 时区名。
  ///
  /// 预期：返回 `tool.invalid_arguments`，不 panic。
  #[tokio::test]
  async fn unknown_timezone_maps_to_invalid_arguments() {
    let tool = create_current_time_tool();
    let err = tool
      .execute(
        "call-1",
        json!({ "timezone": "Mars/Olympus_Mons" }),
        &ToolContext::detached(),
      )
      .await
      .unwrap_err();
    assert_eq!(err.code(), INVALID_ARGUMENTS);
  }
}
