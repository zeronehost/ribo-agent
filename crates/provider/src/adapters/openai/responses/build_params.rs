//! OpenAI Responses API（`POST /responses`）请求体构造。
//!
//! 与 Chat Completions 的差异：
//!
//! 1. 对话历史放在 `input` 数组中，由「消息项」与「工具项」混排组成；
//! 2. 系统提示走顶层 `instructions`，而不是一条 `system` 消息；
//! 3. 工具定义为扁平结构（`{"type":"function","name":..}`），不再嵌套 `function` 对象；
//! 4. 输出上限字段名为 `max_output_tokens`（API 要求最小 16）；
//! 5. 无状态回放：固定 `store: false`，避免服务端留存与隐式上下文。
//!
//! 阶段 1 仅支持文本：非文本块（图片、思考）静默丢弃并记 `warn`。

use ra_core::message::{
  AssistantMessage, Content, Message, ToolChoice, ToolResultMessage,
  ToolSchema, UserContent, UserMessage,
};

use crate::{
  adapters::common::join_text, context::Context, model::Model,
  options::StreamOptions,
};

/// Responses API 对 `max_output_tokens` 的最小值要求。
const MIN_OUTPUT_TOKENS: u32 = 16;

/// 构造一次 Responses API 流式请求的 JSON 请求体。
///
/// # 参数
///
/// - `model`：本次调用的模型元信息（取 `id` 作为模型名）；
/// - `context`：系统提示、历史消息与工具声明；
/// - `options`：采样参数与输出上限等可选覆盖项。
///
/// # 返回
///
/// 可直接序列化为 HTTP 请求体的 JSON 值。
pub fn build(
  model: &Model,
  context: &Context,
  options: &StreamOptions,
) -> serde_json::Value {
  // ===== input 项序列 =====
  let mut input: Vec<serde_json::Value> =
    Vec::with_capacity(context.message.len());
  for message in &context.message {
    match message {
      Message::User(user) => {
        if let Some(item) = user_to_item(user) {
          input.push(item);
        }
      }
      Message::Assistant(assistant) => push_assistant(&mut input, assistant),
      Message::ToolResult(result) => {
        input.push(tool_result_to_item(result));
      }
    }
  }

  // ===== 必填字段 =====
  // store: false —— 每次请求都从本地历史完整回放，不依赖服务端状态。
  let mut body = serde_json::json!({
    "model": model.id.as_str(),
    "input": input,
    "stream": true,
    "store": false,
  });

  // ===== system → instructions =====
  let instructions = join_text(&context.system_prompt);
  if !instructions.is_empty() {
    body["instructions"] = serde_json::Value::String(instructions);
  }

  // ===== 输出上限（API 要求 >= 16）=====
  // 显式覆盖优先；u64 → u32 转换前先夹取上限，防止极端值截断。
  let max = match options.max_tokens {
    Some(v) => v.min(u64::from(u32::MAX)) as u32,
    None => model.max_tokens,
  };
  if max > 0 {
    body["max_output_tokens"] =
      serde_json::Value::from(max.max(MIN_OUTPUT_TOKENS));
  }

  // ===== 采样参数（Responses 不支持 stop / seed）=====
  if let Some(t) = options.sampling.temperature {
    body["temperature"] = serde_json::json!(t);
  }
  if let Some(p) = options.sampling.top_p {
    body["top_p"] = serde_json::json!(p);
  }

  // ===== 工具声明 =====
  if !context.tools.is_empty() {
    body["tools"] = serde_json::Value::Array(
      context.tools.iter().map(tool_to_wire).collect(),
    );
    body["tool_choice"] = tool_choice_to_wire(&context.tool_choice);
  }

  body
}

/// 用户消息 → `input` 消息项；文本为空时跳过（避免发送空 content）。
fn user_to_item(user: &UserMessage) -> Option<serde_json::Value> {
  let text = match &user.content {
    UserContent::Text(text) => text.clone(),
    UserContent::Blocks(blocks) => join_text(blocks),
  };
  if text.is_empty() {
    return None;
  }
  Some(serde_json::json!({
    "type": "message",
    "role": "user",
    "content": [{ "type": "input_text", "text": text }],
  }))
}

/// 助手消息 → 若干 `input` 项。
///
/// 文本折叠为一条 `message` 项；每个工具调用单独成为一条
/// `function_call` 项（Responses 的顶层项形态，不再包进 `tool_calls`）。
fn push_assistant(
  input: &mut Vec<serde_json::Value>,
  assistant: &AssistantMessage,
) {
  let text = join_text(&assistant.content);
  if !text.is_empty() {
    input.push(serde_json::json!({
      "type": "message",
      "role": "assistant",
      "content": [{ "type": "output_text", "text": text }],
    }));
  }
  for block in &assistant.content {
    match block {
      Content::ToolCall(call) => {
        input.push(serde_json::json!({
          "type": "function_call",
          "call_id": call.id,
          "name": call.name,
          // Responses 与 Chat Completions 一致：arguments 为 JSON 字符串。
          "arguments": call.arguments.to_string(),
        }));
      }
      Content::Thinking(_) => {
        tracing::warn!(
          "responses build_params: dropping thinking block (stage 1 text-only)"
        );
      }
      _ => {}
    }
  }
}

/// 工具结果 → `function_call_output` 项（通过 `call_id` 关联调用）。
fn tool_result_to_item(result: &ToolResultMessage) -> serde_json::Value {
  serde_json::json!({
    "type": "function_call_output",
    "call_id": result.tool_call_id,
    "output": join_text(&result.content),
  })
}

/// 工具声明 → Responses 扁平 function 定义。
fn tool_to_wire(tool: &ToolSchema) -> serde_json::Value {
  serde_json::json!({
    "type": "function",
    "name": tool.name,
    "description": tool.description,
    "parameters": tool.input_schema.0,
  })
}

/// 统一工具选择策略 → Responses 的 `tool_choice` 取值。
fn tool_choice_to_wire(choice: &ToolChoice) -> serde_json::Value {
  match choice {
    ToolChoice::Auto => serde_json::json!("auto"),
    ToolChoice::None => serde_json::json!("none"),
    ToolChoice::Required => serde_json::json!("required"),
    ToolChoice::Named(name) => {
      serde_json::json!({ "type": "function", "name": name })
    }
  }
}

#[cfg(test)]
mod tests {
  use ra_core::message::{
    AssistantMessage, Schema, TextContent, ThinkingContent, ToolCall,
    ToolResultMessage, ToolSchema, UserContent, UserMessage,
  };

  use super::*;
  use crate::{
    ids::{ModelId, ProviderId},
    options::SamplingOptions,
  };

  /// 构造最小可用模型；`max_tokens` 用于验证输出上限字段。
  fn test_model() -> Model {
    Model::new(
      ProviderId::parse("openai").expect("测试提供方 ID 合法"),
      ModelId::parse("gpt-4o").expect("测试模型 ID 合法"),
      "GPT-4o",
      "https://api.openai.com/v1",
    )
    .with_max_tokens(4096)
  }

  fn text(content: &str) -> Content {
    Content::Text(TextContent {
      content: content.to_string(),
      signature: None,
    })
  }

  /// 场景：系统提示、用户消息、助手文本 + 工具调用、工具结果与工具声明。
  ///
  /// 预期：system 折叠为 instructions；input 由 message / function_call /
  /// function_call_output 三类项按序组成；tools 为扁平结构。
  #[test]
  fn build_serializes_input_items_and_tools() {
    let model = test_model();

    let mut assistant = AssistantMessage::empty("openai", "gpt-4o", 0);
    assistant.content = vec![
      text("answer"),
      Content::ToolCall(ToolCall {
        id: "call_1".to_string(),
        name: "lookup".to_string(),
        arguments: serde_json::json!({ "q": "rust" }),
        signature: None,
        namespace: None,
      }),
    ];
    let tool_result = ToolResultMessage {
      tool_call_id: "call_1".to_string(),
      tool_name: "lookup".to_string(),
      is_error: false,
      timestamp: 0,
      content: vec![text("result")],
      details: None,
      usage: None,
      added_tool_names: Vec::new(),
    };
    let context = Context {
      system_prompt: vec![text("sys")],
      message: vec![
        Message::User(UserMessage {
          content: UserContent::Text("hi".to_string()),
          timestamp: 0,
        }),
        Message::Assistant(assistant),
        Message::ToolResult(tool_result),
      ],
      tools: vec![ToolSchema {
        name: "lookup".to_string(),
        description: "lookup something".to_string(),
        input_schema: Schema(serde_json::json!({ "type": "object" })),
        constrained_sampling: None,
      }],
      tool_choice: ToolChoice::Named("lookup".to_string()),
    };
    let options = StreamOptions {
      sampling: SamplingOptions {
        temperature: Some(0.5),
        top_p: Some(0.9),
        ..SamplingOptions::default()
      },
      max_tokens: Some(128),
      ..StreamOptions::default()
    };

    let body = build(&model, &context, &options);

    assert_eq!(body["model"], "gpt-4o");
    assert_eq!(body["stream"], true);
    assert_eq!(body["store"], false);
    assert_eq!(body["instructions"], "sys");
    assert_eq!(body["max_output_tokens"], 128);
    assert_eq!(body["temperature"], 0.5);

    let input = body["input"].as_array().expect("input 应为数组");
    assert_eq!(input.len(), 4);
    assert_eq!(input[0]["type"], "message");
    assert_eq!(input[0]["role"], "user");
    assert_eq!(input[0]["content"][0]["type"], "input_text");
    assert_eq!(input[0]["content"][0]["text"], "hi");
    assert_eq!(input[1]["type"], "message");
    assert_eq!(input[1]["role"], "assistant");
    assert_eq!(input[1]["content"][0]["type"], "output_text");
    assert_eq!(input[1]["content"][0]["text"], "answer");
    assert_eq!(input[2]["type"], "function_call");
    assert_eq!(input[2]["call_id"], "call_1");
    assert_eq!(input[2]["name"], "lookup");
    assert_eq!(
      input[2]["arguments"],
      serde_json::json!({ "q": "rust" }).to_string()
    );
    assert_eq!(input[3]["type"], "function_call_output");
    assert_eq!(input[3]["call_id"], "call_1");
    assert_eq!(input[3]["output"], "result");

    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["name"], "lookup");
    assert_eq!(
      body["tools"][0]["parameters"],
      serde_json::json!({ "type": "object" })
    );
    assert_eq!(
      body["tool_choice"],
      serde_json::json!({ "type": "function", "name": "lookup" })
    );
  }

  /// 场景：空系统提示、仅含非文本块的系统提示、块状用户消息、
  /// 仅有工具调用的助手消息、无工具。
  ///
  /// 预期：不写 instructions / tools / tool_choice；块状用户内容拼接文本；
  /// 只有工具调用时只产出 function_call 项。
  #[test]
  fn build_omits_optional_fields_and_normalizes_blocks() {
    let model = test_model();
    let mut assistant = AssistantMessage::empty("openai", "gpt-4o", 0);
    assistant.content = vec![Content::ToolCall(ToolCall {
      id: "call_1".to_string(),
      name: "lookup".to_string(),
      arguments: serde_json::json!({}),
      signature: None,
      namespace: None,
    })];
    let context = Context {
      system_prompt: vec![Content::Thinking(ThinkingContent {
        content: "ignore".to_string(),
        signature: None,
        redacted: false,
      })],
      message: vec![
        Message::User(UserMessage {
          content: UserContent::Blocks(vec![text("a"), text("b")]),
          timestamp: 0,
        }),
        Message::Assistant(assistant),
      ],
      tools: Vec::new(),
      tool_choice: ToolChoice::Auto,
    };

    let body = build(&model, &context, &StreamOptions::default());

    assert!(body.get("instructions").is_none());
    assert!(body.get("tools").is_none());
    assert!(body.get("tool_choice").is_none());
    assert!(body.get("temperature").is_none());
    assert!(body.get("top_p").is_none());
    // model.max_tokens = 4096 > 0，未显式覆盖时沿用模型默认
    assert_eq!(body["max_output_tokens"], 4096);

    let input = body["input"].as_array().expect("input 应为数组");
    assert_eq!(input.len(), 2);
    assert_eq!(input[0]["content"][0]["text"], "a\nb");
    assert_eq!(input[1]["type"], "function_call");
    assert!(input[1].get("role").is_none());
  }

  /// 场景：显式输出上限小于 API 最小值。
  ///
  /// 预期：抬升到 16，避免请求被服务端以 400 拒绝。
  #[test]
  fn build_clamps_small_max_output_tokens() {
    let model = test_model();
    let options = StreamOptions {
      max_tokens: Some(4),
      ..StreamOptions::default()
    };
    let body = build(&model, &Context::default(), &options);
    assert_eq!(body["max_output_tokens"], 16);
    assert_eq!(body["input"], serde_json::json!([]));
  }

  /// 场景：三种简写工具选择策略的线格式映射。
  #[test]
  fn build_maps_simple_tool_choices() {
    let model = test_model();
    let context = Context {
      tools: vec![ToolSchema {
        name: "lookup".to_string(),
        description: "d".to_string(),
        input_schema: Schema(serde_json::json!({})),
        constrained_sampling: None,
      }],
      ..Context::default()
    };

    for (choice, expected) in [
      (ToolChoice::Auto, "auto"),
      (ToolChoice::None, "none"),
      (ToolChoice::Required, "required"),
    ] {
      let body = build(
        &model,
        &Context {
          tool_choice: choice,
          ..context.clone()
        },
        &StreamOptions::default(),
      );
      assert_eq!(body["tool_choice"], expected);
    }
  }
}
