//! Context → Anthropic Messages API request body 折叠。
//!
//! 阶段 1 支持：
//! - messages：历史消息（User / Assistant / ToolResult → Anthropic 格式）
//! - tools：可用工具列表（ToolSchema → Anthropic tool format）
//! - tool_choice：工具选择策略映射
//! - system：系统提示
//!
//! 不支持：thinking blocks / 多模态 blocks（静默丢弃 + warn）。

use ra_core::message::{
  AssistantMessage, Content, Message, ToolChoice, ToolResultMessage,
  ToolSchema, UserContent, UserMessage,
};

use crate::{
  adapters::common::join_text, context::Context, model::Model,
  options::StreamOptions,
};

/// 构建 Anthropic Messages API 请求体。
pub fn build(
  model: &Model,
  context: &Context,
  options: &StreamOptions,
) -> serde_json::Value {
  let mut body = serde_json::json!({
    "model": model.id.as_str(),
    "max_tokens": model.max_tokens,
    "stream": true,
  });

  // ===== messages 字段 =====
  let messages = collect_messages(context);
  body["messages"] = messages;

  // ===== system 字段 =====
  if let Some(system) = collect_system(&context.system_prompt) {
    body["system"] = serde_json::Value::String(system);
  }

  // ===== tools / tool_choice =====
  // Anthropic 不支持 "none" 工具策略——
  // tool_choice == None 时跳过 tools 字段（等效禁用工具）。
  let tc = tool_choice_to_anthropic(&context.tool_choice);
  match tc {
    Some(Some(v)) => {
      // 正常发出 tools + tool_choice
      if !context.tools.is_empty() {
        body["tools"] = serde_json::Value::Array(
          context.tools.iter().map(tool_to_anthropic).collect(),
        );
      }
      body["tool_choice"] = v;
    }
    Some(None) | None => {
      // 发出 tools 但不发 tool_choice（auto 或缺省），
      // 或明确禁用工具（None）——后者 tools 也跳过
      if tc.is_some() && !context.tools.is_empty() {
        body["tools"] = serde_json::Value::Array(
          context.tools.iter().map(tool_to_anthropic).collect(),
        );
      }
    }
  }

  // ===== sampling =====
  if let Some(t) = options.sampling.temperature {
    body["temperature"] = serde_json::Value::Number(
      serde_json::Number::from_f64(t).expect("valid f64"),
    );
  }

  // ===== thinking 预算（Anthropic extended thinking）=====
  // 映射策略：取用户已配置的最高档作为 budget_tokens（预算语义是
  // 「上限」，最高档代表最强意图）；四档全空则不发 thinking 字段。
  if let Some(budgets) = options.thinking_budgets.as_ref() {
    let budget_tokens = budgets
      .high
      .or(budgets.medium)
      .or(budgets.low)
      .or(budgets.minimal);
    if let Some(tokens) = budget_tokens {
      body["thinking"] = serde_json::json!({
        "type": "enabled",
        "budget_tokens": tokens,
      });
    }
  }
  if let Some(p) = options.sampling.top_p {
    body["top_p"] = serde_json::Value::Number(
      serde_json::Number::from_f64(p as f64).expect("valid f64"),
    );
  }

  body
}

// ===== 内部辅助：messages 收集 =====

fn collect_messages(context: &Context) -> serde_json::Value {
  let mut out: Vec<serde_json::Value> = Vec::new();
  for msg in &context.message {
    match msg {
      Message::User(u) => {
        if let Some(v) = user_to_anthropic(u) {
          out.push(v);
        }
      }
      Message::Assistant(a) => {
        if let Some(v) = assistant_to_anthropic(a) {
          out.push(v);
        }
      }
      Message::ToolResult(t) => {
        out.push(tool_result_to_anthropic(t));
      }
    }
  }
  serde_json::Value::Array(out)
}

fn collect_system(prompts: &[Content]) -> Option<String> {
  let mut text = String::new();
  for c in prompts {
    if let Content::Text(t) = c {
      text.push_str(&t.content);
    }
  }
  if text.is_empty() { None } else { Some(text) }
}

fn user_to_anthropic(u: &UserMessage) -> Option<serde_json::Value> {
  let content = match &u.content {
    UserContent::Text(t) => {
      vec![serde_json::json!({
        "type": "text",
        "text": t.as_str(),
      })]
    }
    UserContent::Blocks(blocks) => collect_text_blocks(blocks),
  };
  if content.is_empty() {
    return None;
  }
  Some(serde_json::json!({
    "role": "user",
    "content": serde_json::Value::Array(content),
  }))
}

fn assistant_to_anthropic(a: &AssistantMessage) -> Option<serde_json::Value> {
  let mut content: Vec<serde_json::Value> = Vec::new();
  for c in &a.content {
    match c {
      Content::Text(t) => {
        if !t.content.is_empty() {
          content.push(serde_json::json!({
            "type": "text",
            "text": t.content,
          }));
        }
      }
      Content::ToolCall(call) => {
        content.push(serde_json::json!({
          "type": "tool_use",
          "id": call.id,
          "name": call.name,
          "input": call.arguments,
        }));
      }
      Content::Thinking(_) => {
        tracing::warn!(
          "anthropic build_params: dropping thinking block (stage 1 text-only)"
        );
      }
      _ => {}
    }
  }
  if content.is_empty() {
    return None;
  }
  Some(serde_json::json!({
    "role": "assistant",
    "content": serde_json::Value::Array(content),
  }))
}

fn tool_result_to_anthropic(t: &ToolResultMessage) -> serde_json::Value {
  let text = join_text(&t.content);

  serde_json::json!({
    "role": "user",
    "content": [{
      "type": "tool_result",
      "tool_use_id": t.tool_call_id,
      "content": text,
      "is_error": t.is_error,
    }],
  })
}

fn collect_text_blocks(blocks: &[Content]) -> Vec<serde_json::Value> {
  let mut out = Vec::new();
  for b in blocks {
    match b {
      Content::Text(t) => {
        out.push(serde_json::json!({
          "type": "text",
          "text": t.content,
        }));
      }
      _ => {
        tracing::warn!(
          "anthropic build_params: dropping non-text block (stage 1 text-only)"
        );
      }
    }
  }
  out
}

// ===== 工具映射 =====

fn tool_to_anthropic(tool: &ToolSchema) -> serde_json::Value {
  serde_json::json!({
    "name": tool.name,
    "description": tool.description,
    "input_schema": tool.input_schema,
  })
}

/// ToolChoice → Anthropic 三态：
/// - `None`（明确禁用工具）→ `None`（同时跳过 tools + tool_choice）
/// - `Some(None)`（auto 或缺省）→ 省略 tool_choice（Anthropic 自己决定）
/// - `Some(Some(v))`（Required/Named）→ 发出 tool_choice 值
fn tool_choice_to_anthropic(
  tc: &ToolChoice,
) -> Option<Option<serde_json::Value>> {
  match tc {
    ToolChoice::None => None,
    ToolChoice::Auto => Some(None),
    ToolChoice::Required => Some(Some(serde_json::json!({"type": "any"}))),
    ToolChoice::Named(n) => Some(Some(serde_json::json!({
      "type": "tool",
      "name": n,
    }))),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::ids::{ModelId, ProviderId};
  use crate::model::Model;
  use crate::test_support::init;
  use ra_core::message::{Message, UserContent, UserMessage};

  fn model() -> Model {
    Model::new(
      ProviderId::parse("anthropic").expect("valid"),
      ModelId::parse("anthropic/claude-3-haiku-20240307").expect("valid"),
      "claude-3-haiku-20240307",
      "https://api.anthropic.com",
    )
  }

  fn user_text(text: &str) -> UserMessage {
    UserMessage {
      content: UserContent::Text(text.to_string()),
      timestamp: 0,
    }
  }

  fn context_with_user(text: &str) -> Context {
    Context {
      message: vec![Message::User(user_text(text))],
      ..Default::default()
    }
  }

  #[test]
  fn build_serializes_messages_and_optional_params() {
    init();
    let body = build(
      &model(),
      &context_with_user("hello"),
      &StreamOptions::default(),
    );
    assert_eq!(body["model"], "anthropic/claude-3-haiku-20240307");
    assert_eq!(body["stream"], true);
    let messages = body["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"][0]["type"], "text");
    assert_eq!(messages[0]["content"][0]["text"], "hello");
  }

  /// 场景：thinking 预算注入（Anthropic extended thinking 专属字段）。
  ///
  /// 预期：取用户已配置的最高档作为 budget_tokens（medium 8192 胜过
  /// minimal 1024）；`thinking_budgets` 为 None（默认）时不发 thinking 字段。
  #[test]
  fn build_thinking_budget_uses_highest_configured_tier() {
    init();
    let options = StreamOptions {
      thinking_budgets: Some(crate::model::ThinkingBudgets {
        minimal: Some(1024),
        low: None,
        medium: Some(8192),
        high: None,
      }),
      ..StreamOptions::default()
    };

    let body = build(&model(), &context_with_user("hi"), &options);
    assert_eq!(body["thinking"]["type"], "enabled");
    assert_eq!(body["thinking"]["budget_tokens"], 8192, "应取最高已配置档");

    // 默认选项：thinking_budgets 为 None，不发 thinking 字段
    let body = build(
      &model(),
      &context_with_user("hi"),
      &StreamOptions::default(),
    );
    assert!(
      body.get("thinking").is_none(),
      "未配置预算时不应携带 thinking 字段"
    );
  }

  #[test]
  fn build_omits_tools_when_no_tool_choice() {
    init();
    let mut ctx = context_with_user("hi");
    ctx.tool_choice = ToolChoice::None;
    let body = build(&model(), &ctx, &StreamOptions::default());
    assert!(body.get("tools").is_none(), "None 策略应跳过 tools 字段");
    assert!(
      body.get("tool_choice").is_none(),
      "None 策略应跳过 tool_choice 字段"
    );
  }

  #[test]
  fn build_required_maps_to_any() {
    init();
    let mut ctx = context_with_user("hi");
    ctx.tool_choice = ToolChoice::Required;
    let body = build(&model(), &ctx, &StreamOptions::default());
    let tc = body["tool_choice"].as_object().expect("object");
    assert_eq!(tc["type"], "any");
  }
}
