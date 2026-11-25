//! OpenAI Chat Completions 请求体构造。
//!
//! 把与线格式无关的 [`Context`] / [`Model`] / [`StreamOptions`] 折叠为
//! OpenAI 兼容的 JSON 请求体：
//!
//! 1. 先写入系统提示（存在文本时），再按序追加历史消息；
//! 2. 固定开启流式输出，并要求服务端在末尾附带 usage 统计；
//! 3. 采样参数、输出上限与工具声明仅在调用方显式提供时写入，
//!    避免用默认值覆盖提供方自身的默认行为。

use ra_core::message::{Content, Message, ToolChoice, ToolSchema, UserContent};

use crate::{
  adapters::common::join_text, context::Context, model::Model,
  options::StreamOptions,
};

/// 构造一次 OpenAI 流式请求的 JSON 请求体。
///
/// # 参数
///
/// - `model`：本次调用的模型元信息（取其中 `id` 作为模型名）；
/// - `context`：系统提示、历史消息与工具声明；
/// - `options`：采样参数与输出上限等可选覆盖项。
///
/// # 返回
///
/// 可直接序列化为 HTTP 请求体的 JSON 值。
pub(super) fn build(
  model: &Model,
  context: &Context,
  options: &StreamOptions,
) -> serde_json::Value {
  // 消息序列
  // 预留 1 个位置给系统提示，减少扩容。
  let mut messages = Vec::with_capacity(context.message.len() + 1);
  push_system_message(&mut messages, &context.system_prompt);
  for message in &context.message {
    push_message(&mut messages, message);
  }

  // 必填字段
  // 始终开启流式输出，并要求服务端在最后一个 chunk 中附带 usage。
  let mut body = serde_json::json!({
    "model": model.id.as_str(),
    "messages": messages,
    "stream": true,
    "stream_options": { "include_usage": true }
  });

  // 采样参数（仅在显式提供时写入）
  if let Some(t) = options.sampling.temperature {
    body["temperature"] = serde_json::json!(t);
  }
  if let Some(p) = options.sampling.top_p {
    body["top_p"] = serde_json::json!(p);
  }
  if !options.sampling.stop.is_empty() {
    body["stop"] = serde_json::json!(options.sampling.stop);
  }
  if let Some(seed) = options.sampling.seed {
    body["seed"] = serde_json::json!(seed);
  }
  if let Some(max_tokens) = options.max_tokens {
    body["max_tokens"] = serde_json::json!(max_tokens);
  }

  // 工具声明
  // 只有调用方提供了工具时才写入 `tools` 与 `tool_choice`，
  // 避免向不支持工具调用的模型发送多余字段。
  if !context.tools.is_empty() {
    body["tools"] = serde_json::json!(
      context.tools.iter().map(tool_to_wire).collect::<Vec<_>>()
    );
    body["tool_choice"] = tool_choice_to_wire(&context.tool_choice);
  }

  body
}

/// 把系统提示中的文本块拼接为一条 `system` 消息。
///
/// 系统提示为空（或仅含非文本块）时不写入任何消息——部分提供方对空
/// 系统消息敏感。
fn push_system_message(
  messages: &mut Vec<serde_json::Value>,
  blocks: &[Content],
) {
  let text = join_text(blocks);
  if !text.is_empty() {
    messages.push(serde_json::json!({"role": "system", "content": text}));
  }
}

/// 把一条统一消息转换为 OpenAI 线格式消息。
///
/// 分别处理用户、助手与工具结果三类角色；助手消息中的工具调用会映射为
/// `tool_calls` 数组。
fn push_message(messages: &mut Vec<serde_json::Value>, message: &Message) {
  match message {
    // 用户消息：纯文本直接使用，多模态块拼接其中的文本部分。
    Message::User(user) => {
      let content = match &user.content {
        UserContent::Text(text) => text.clone(),
        UserContent::Blocks(blocks) => join_text(blocks),
      };
      messages.push(serde_json::json!({"role": "user", "content": content}));
    }
    // 助手消息：文本 + 工具调用分别落到 content 与 tool_calls。
    Message::Assistant(assistant) => {
      let text = join_text(&assistant.content);
      let tool_calls: Vec<serde_json::Value> = assistant
        .content
        .iter()
        .filter_map(|block| match block {
          Content::ToolCall(call) => Some(serde_json::json!({
            "id": call.id,
            "type": "function",
            "function": {
              "name": call.name,
              // OpenAI 要求 arguments 为 JSON 字符串而非对象。
              "arguments": call.arguments.to_string()
            }
          })),
          _ => None,
        })
        .collect();

      // 只有工具调用、没有文本时，content 必须为 null（而非空串）。
      let content = if text.is_empty() && !tool_calls.is_empty() {
        serde_json::Value::Null
      } else {
        serde_json::json!(text)
      };

      let mut wire =
        serde_json::json!({"role": "assistant", "content": content});
      if !tool_calls.is_empty() {
        wire["tool_calls"] = serde_json::Value::Array(tool_calls)
      }
      messages.push(wire);
    }
    // 工具结果：通过 tool_call_id 关联此前的工具调用。
    Message::ToolResult(result) => {
      messages.push(serde_json::json!({
        "role": "tool",
        "tool_call_id": result.tool_call_id,
        "content": join_text(&result.content),
      }));
    }
  }
}

/// 把工具声明转换为 OpenAI 的 function 定义。
fn tool_to_wire(tool: &ToolSchema) -> serde_json::Value {
  serde_json::json!({
    "type": "function",
    "function": {
      "name": tool.name,
      "description": tool.description,
      "parameters": tool.input_schema.0
    }
  })
}

/// 把统一的工具选择策略转换为 OpenAI 的 `tool_choice` 取值。
///
/// `Auto` / `None` / `Required` 映射为同名字符串；指定工具名时使用
/// `{"type":"function","function":{"name":..}}` 的对象形式。
fn tool_choice_to_wire(choice: &ToolChoice) -> serde_json::Value {
  match choice {
    ToolChoice::Auto => serde_json::json!("auto"),
    ToolChoice::None => serde_json::json!("none"),
    ToolChoice::Required => serde_json::json!("required"),
    ToolChoice::Named(name) => {
      serde_json::json!({ "type": "function", "function": { "name": name } })
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

  /// 构造一个最小可用模型（请求体只使用其 `id`）。
  fn test_model() -> Model {
    Model::new(
      ProviderId::parse("p1").expect("测试提供方 ID 合法"),
      ModelId::parse("m1").expect("测试模型 ID 合法"),
      "Model One",
      "http://stub.local",
    )
  }

  fn text(content: &str) -> Content {
    Content::Text(TextContent {
      content: content.to_string(),
      signature: None,
    })
  }

  /// 场景：完整的系统提示、历史消息、工具声明与全部可选参数。
  ///
  /// 预期：消息按 system → user → assistant → tool 排序，采样参数与
  /// 工具声明按线格式写入。
  #[test]
  fn build_serializes_messages_and_optional_params() {
    let model = test_model();

    let mut assistant = AssistantMessage::empty("p1", "m1", 0);
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
      system_prompt: vec![
        text("sys"),
        // 非文本块不进入 system 消息
        Content::Thinking(ThinkingContent {
          content: "ignored".to_string(),
          signature: None,
          redacted: false,
        }),
      ],
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
        stop: vec!["END".to_string()],
        seed: Some(42),
      },
      max_tokens: Some(128),
      ..StreamOptions::default()
    };

    let body = build(&model, &context, &options);

    // 基础字段
    assert_eq!(body["model"], "m1");
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"]["include_usage"], true);

    // 消息序列
    let messages = body["messages"].as_array().expect("messages 应为数组");
    assert_eq!(messages.len(), 4);
    assert_eq!(
      messages[0],
      serde_json::json!({ "role": "system", "content": "sys" })
    );
    assert_eq!(
      messages[1],
      serde_json::json!({ "role": "user", "content": "hi" })
    );
    assert_eq!(messages[2]["role"], "assistant");
    assert_eq!(messages[2]["content"], "answer");
    assert_eq!(messages[2]["tool_calls"][0]["id"], "call_1");
    assert_eq!(
      messages[2]["tool_calls"][0]["function"]["arguments"],
      serde_json::json!({ "q": "rust" }).to_string()
    );
    assert_eq!(
      messages[3],
      serde_json::json!({
        "role": "tool",
        "tool_call_id": "call_1",
        "content": "result"
      })
    );

    // 采样参数与输出上限
    assert_eq!(body["temperature"], 0.5);
    assert_eq!(body["top_p"].as_f64(), Some(0.9_f32 as f64));
    assert_eq!(body["stop"], serde_json::json!(["END"]));
    assert_eq!(body["seed"], 42);
    assert_eq!(body["max_tokens"], 128);

    // 工具声明与选择策略
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["function"]["name"], "lookup");
    assert_eq!(
      body["tools"][0]["function"]["parameters"],
      serde_json::json!({ "type": "object" })
    );
    assert_eq!(
      body["tool_choice"],
      serde_json::json!({
        "type": "function",
        "function": { "name": "lookup" }
      })
    );
  }

  /// 场景：未提供可选参数、空系统提示、块状用户消息、仅含工具调用的助手消息。
  ///
  /// 预期：可选键缺省；空 system 不写入；块状内容只拼接文本；
  /// 仅有工具调用时 `content` 为 `null`。
  #[test]
  fn build_omits_optional_fields_and_normalizes_blocks() {
    let model = test_model();
    let mut assistant = AssistantMessage::empty("p1", "m1", 0);
    assistant.content = vec![Content::ToolCall(ToolCall {
      id: "call_1".to_string(),
      name: "lookup".to_string(),
      arguments: serde_json::json!({}),
      signature: None,
      namespace: None,
    })];
    let context = Context {
      // 仅含非文本块 → 不生成 system 消息
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

    assert!(body.get("temperature").is_none());
    assert!(body.get("top_p").is_none());
    assert!(body.get("stop").is_none());
    assert!(body.get("seed").is_none());
    assert!(body.get("max_tokens").is_none());
    assert!(body.get("tools").is_none());
    assert!(body.get("tool_choice").is_none());

    let messages = body["messages"].as_array().expect("messages 应为数组");
    assert_eq!(messages.len(), 2);
    assert_eq!(
      messages[0],
      serde_json::json!({ "role": "user", "content": "a\nb" })
    );
    assert_eq!(messages[1]["content"], serde_json::Value::Null);
    assert_eq!(messages[1]["tool_calls"][0]["function"]["name"], "lookup");
  }

  /// 场景：助手消息只有文本、没有工具调用。
  ///
  /// 预期：不写入 `tool_calls` 字段（覆盖条件为假的分支）。
  #[test]
  fn build_skips_tool_calls_for_text_only_assistant() {
    let model = test_model();
    let mut assistant = AssistantMessage::empty("p1", "m1", 0);
    assistant.content = vec![text("plain")];
    let context = Context {
      message: vec![Message::Assistant(assistant)],
      ..Context::default()
    };

    let body = build(&model, &context, &StreamOptions::default());

    let messages = body["messages"].as_array().expect("messages 应为数组");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["content"], "plain");
    assert!(messages[0].get("tool_calls").is_none());
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
