//! Anthropic Messages API SSE 帧分派。
//!
//! 线格式参考：<https://docs.anthropic.com/en/api/messages-streaming>
//!
//! 传输层（字节 → 行 → 帧）与解析任务骨架复用
//! [`crate::stream::SseParser`] / [`crate::adapters::parse_task`]；
//! 本模块只保留 Anthropic 方言的「帧 → 事件」映射与错误码表。

use ra_core::message::StopReason;

use crate::{
  adapters::parse_task,
  error::{
    AUTH_FAILED, INVALID_REQUEST, QUOTA_EXCEEDED, RATE_LIMITED, SERVER_ERROR,
    TIMEOUT,
  },
  ids::{ModelId, ProviderId},
  options::CancelToken,
  stream::{LineFlow, ProviderEventStreamProducer, SseFrame, StreamAggregator},
};

// ============================================================================
// 方言私有错误码表
// ============================================================================

/// Anthropic 流内 `error` 事件的 `type` → 内部错误码。
///
/// 未识别取值返回 `None`（事件模型允许未分类错误）。
/// `request_too_large` / `not_found_error` 归入请求类——重试无法恢复。
fn map_error_type(raw: &str) -> Option<&'static str> {
  let code = match raw {
    "invalid_request_error" | "request_too_large" | "not_found_error" => {
      INVALID_REQUEST
    }
    "authentication_error" | "permission_error" => AUTH_FAILED,
    "billing_error" => QUOTA_EXCEEDED,
    "rate_limit_error" => RATE_LIMITED,
    "timeout_error" => TIMEOUT,
    "api_error" | "overloaded_error" => SERVER_ERROR,
    _ => return None,
  };
  Some(code)
}

// ============================================================================
// Anthropic SSE payload 类型（嵌套层是关键）
// ============================================================================

#[derive(Debug, serde::Deserialize)]
struct ContentBlock {
  #[serde(rename = "type")]
  block_type: String,
  /// tool_use 块的调用 ID（仅 `content_block_start` 携带，必须注册）。
  #[serde(default)]
  id: Option<String>,
  /// tool_use 块的工具名（仅 `content_block_start` 携带，必须注册）。
  #[serde(default)]
  name: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct Delta {
  #[serde(rename = "type")]
  delta_type: String,
  #[serde(default)]
  text: Option<String>,
  /// tool_use 参数片段：线格式 `delta.type = "input_json_delta"`，
  /// 但**携带字段名是 `partial_json`**（type 值 ≠ 字段名，勿混淆）。
  #[serde(default)]
  partial_json: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct MessageDelta {
  #[serde(default)]
  stop_reason: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct MessageUsage {
  #[serde(default)]
  input_tokens: Option<i64>,
  #[serde(default)]
  output_tokens: Option<i64>,
}

// ============================================================================
// 帧分派（dispatch_frame）
// ============================================================================

/// 处理单帧，返回解析去向。
pub(crate) fn dispatch_frame(
  frame: &SseFrame,
  aggregator: &mut StreamAggregator,
  producer: &mut Option<ProviderEventStreamProducer>,
) -> LineFlow {
  let Some(p) = producer.as_ref() else {
    return LineFlow::Terminated;
  };

  match frame.event.as_deref().unwrap_or("") {
    "message_start" => {
      // Start 已由共享骨架在任务开头推送；此处仅记录锚点。
      // （历史实现曾在此推 Start，与骨架重复推送会破坏不变量 1）
      tracing::debug!("anthropic: message start");
    }

    "content_block_start" => {
      #[derive(Debug, serde::Deserialize)]
      struct Payload {
        index: usize,
        content_block: ContentBlock,
      }
      let Ok(payload) = serde_json::from_str::<Payload>(&frame.data) else {
        tracing::warn!("anthropic: failed to parse content_block_start");
        return LineFlow::Continue;
      };
      tracing::debug!(
        index = payload.index,
        block_type = %payload.content_block.block_type,
        "content_block_start"
      );
      // ===== tool_use 块：id/name 仅在本帧携带，必须立即注册 =====
      // 首次出现即经 tool_delta 开 ToolCall 块（推 ToolCallStart）并登记
      // id/name；后续 input_json_delta 帧以 None 传入（tool_delta 保留
      // 已注册值，只累积参数片段）。漏掉本步会导致 ToolCall 的
      // id/name 恒为空串，多轮回灌的 tool_use_id 被 Anthropic 拒绝。
      if payload.content_block.block_type == "tool_use"
        && !matches!(
          aggregator.tool_delta(
            p,
            payload.index,
            payload.content_block.id.as_deref(),
            payload.content_block.name.as_deref(),
            None,
          ),
          LineFlow::Continue
        )
      {
        return LineFlow::ConsumerGone;
      }
      // text_delta / tool_delta 自管理块纪律；thinking 块阶段 1 丢弃
    }

    "content_block_delta" => {
      #[derive(Debug, serde::Deserialize)]
      struct Payload {
        index: usize,
        delta: Delta,
      }
      let Ok(payload) = serde_json::from_str::<Payload>(&frame.data) else {
        tracing::warn!("anthropic: failed to parse content_block_delta");
        return LineFlow::Continue;
      };

      match payload.delta.delta_type.as_str() {
        "text_delta" => {
          let Some(text) = payload.delta.text.as_ref() else {
            return LineFlow::Continue;
          };
          if !matches!(
            aggregator.text_delta(p, text.as_str()),
            LineFlow::Continue
          ) {
            return LineFlow::ConsumerGone;
          }
        }
        "input_json_delta" => {
          let args = payload.delta.partial_json.as_deref().unwrap_or("");
          if !matches!(
            aggregator.tool_delta(p, payload.index, None, None, Some(args)),
            LineFlow::Continue
          ) {
            return LineFlow::ConsumerGone;
          }
        }
        "thinking_delta" | "redacted_thinking_delta" => {
          tracing::debug!(
            "anthropic: dropping thinking delta (stage 1 text-only)"
          );
        }
        other => {
          tracing::debug!(delta_type = %other, "anthropic: unknown delta type");
        }
      }
    }

    "content_block_stop" => {}

    "message_delta" => {
      #[derive(Debug, serde::Deserialize)]
      struct Payload {
        delta: MessageDelta,
        usage: Option<MessageUsage>,
      }
      let Ok(payload) = serde_json::from_str::<Payload>(&frame.data) else {
        tracing::warn!("anthropic: failed to parse message_delta");
        return LineFlow::Continue;
      };
      if let Some(reason) = payload.delta.stop_reason {
        aggregator.set_finish_reason(&reason);
      }
      if let Some(u) = payload.usage {
        aggregator.set_usage(ra_core::message::Usage {
          input: u.input_tokens.unwrap_or(0),
          output: u.output_tokens.unwrap_or(0),
          ..Default::default()
        });
      }
    }

    "message_stop" => {
      // Anthropic message_stop = [DONE] 等价
      aggregator.mark_done_sentinel();
      aggregator.finalize(producer);
      return LineFlow::Terminated;
    }

    // ===== 流内错误：映射错误码后终结 =====
    // 历史实现缺少本分支，error 事件会被当作未知事件忽略，
    // 导致流只能等 EOF 走截断路径（错误码丢失）。
    "error" => {
      #[derive(Debug, serde::Deserialize)]
      struct Payload {
        #[serde(default)]
        error: Option<ErrorBody>,
      }
      #[derive(Debug, serde::Deserialize)]
      struct ErrorBody {
        #[serde(rename = "type", default)]
        error_type: String,
        #[serde(default)]
        message: Option<String>,
      }
      // error 事件本身即终结信号：载荷解析失败也按未分类错误终结。
      let body = serde_json::from_str::<Payload>(&frame.data)
        .ok()
        .and_then(|payload| payload.error);
      let (code, text) = match body {
        Some(err) => (
          map_error_type(&err.error_type),
          err
            .message
            .unwrap_or_else(|| "provider reported an error".to_string()),
        ),
        None => (None, "provider reported an error".to_string()),
      };
      return parse_task::terminate(
        producer,
        aggregator,
        StopReason::Error,
        text,
        code,
      );
    }

    _ => {
      tracing::debug!(
        event = frame.event.as_deref().unwrap_or(""),
        "anthropic: unknown event type"
      );
    }
  }

  LineFlow::Continue
}

// ============================================================================
// 解析任务
// ============================================================================

/// SSE 解码任务入口。
///
/// 生产者义务（`Start` 先行、取消以 `Aborted` 终结、EOF 冲刷与收尾兜底）
/// 全部落在共享骨架 [`parse_task::spawn_sse_task`]；本函数只注入
/// Anthropic 方言的帧分派。
pub(crate) fn spawn_parse_task(
  response: reqwest::Response,
  producer: ProviderEventStreamProducer,
  cancel: CancelToken,
  provider: ProviderId,
  model: ModelId,
) {
  parse_task::spawn_sse_task(
    response,
    producer,
    cancel,
    provider,
    model,
    dispatch_frame,
  );
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{
    stream::{ProviderEvent, SseParser},
    test_support::init,
  };
  use ra_core::message::Content;

  /// 场景：共享传输层按空行切帧，Anthropic 的 event / data 字段正确提取。
  #[test]
  fn parser_parses_anthropic_sse_frame() {
    init();
    let mut parser = SseParser::default();
    let frames = parser
      .feed(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].event.as_deref(), Some("message_stop"));
    assert!(frames[0].data.contains("message_stop"));
    // 空行已收尾，EOF 无残余帧
    assert!(parser.finish().is_empty());
  }

  /// 场景：多帧连续到达 + 末帧不带空行（部分网关行为）。
  ///
  /// 预期：feed 产出首帧，finish 冲刷出无空行末帧。
  #[test]
  fn parser_handles_multiple_frames_and_trailing_without_blank_line() {
    init();
    let mut parser = SseParser::default();
    let body = concat!(
      "event: content_block_delta\n",
      "data: {\"type\":\"content_block_delta\",\"index\":0,",
      "\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n",
      "event: message_stop\n",
      "data: {\"type\":\"message_stop\"}"
    );
    let frames = parser.feed(body.as_bytes());
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].event.as_deref(), Some("content_block_delta"));
    assert!(frames[0].data.contains("hello"));

    let tail = parser.finish();
    assert_eq!(tail.len(), 1);
    assert_eq!(tail[0].event.as_deref(), Some("message_stop"));
  }

  /// 场景：流内 error 事件（限流）。
  ///
  /// 预期：映射为 RATE_LIMITED 并以 Error 终结——历史实现会忽略该事件，
  /// 只能等 EOF 走截断路径且丢失错误码。
  #[tokio::test]
  async fn error_event_terminates_with_mapped_code() {
    init();
    let (producer, mut stream) = ProviderEventStreamProducer::build();
    let mut producer = Some(producer);
    let mut aggregator = StreamAggregator::new(
      ProviderId::parse("anthropic").expect("valid provider id"),
      ModelId::parse("claude").expect("valid model id"),
    );

    let frame = SseFrame {
      event: Some("error".to_string()),
      data: r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#
        .to_string(),
    };
    let flow = dispatch_frame(&frame, &mut aggregator, &mut producer);
    assert!(matches!(flow, LineFlow::Terminated));

    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
      events.push(event);
    }
    match events.last().expect("terminal event") {
      ProviderEvent::Error { code, message, .. } => {
        assert_eq!(*code, Some(crate::error::RATE_LIMITED));
        assert_eq!(message.error_message.as_deref(), Some("slow down"));
      }
      other => panic!("expected Error, got {other:?}"),
    }
  }

  /// 场景：完整工具调用流（Mapper-02）——content_block_start 注册
  /// tool_use 的 id/name，input_json_delta 累积参数，message_stop 终结。
  ///
  /// 预期：`ToolCallEnd` 事件与 done 消息携带**完整** ToolCall
  /// （id/name 来自 content_block_start 注册，arguments 由参数片段聚合、
  /// finalize 回写 output.content）——两处与 build_params 的
  /// tool_use/tool_result 往返映射闭环，多轮回灌 tool_use_id 不为空。
  #[tokio::test]
  async fn tool_use_stream_aggregates_id_name_and_arguments() {
    init();
    let (producer, mut stream) = ProviderEventStreamProducer::build();
    let mut producer = Some(producer);
    let mut aggregator = StreamAggregator::new(
      ProviderId::parse("anthropic").expect("valid provider id"),
      ModelId::parse("claude").expect("valid model id"),
    );

    let frames = [
      (
        "content_block_start",
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"lookup"}}"#,
      ),
      (
        "content_block_delta",
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"q\":"}}"#,
      ),
      (
        "content_block_delta",
        // 注意：值内含字面 `}`，JSON 文本需要 `\"rust\"}"` + 闭合 `}}`
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"rust\"}"}}"#,
      ),
      (
        "message_delta",
        r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":10}}"#,
      ),
      ("message_stop", r#"{"type":"message_stop"}"#),
    ];
    for (event, data) in frames {
      let frame = SseFrame {
        event: Some(event.to_string()),
        data: data.to_string(),
      };
      if matches!(
        dispatch_frame(&frame, &mut aggregator, &mut producer),
        LineFlow::Terminated
      ) {
        break;
      }
    }

    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
      events.push(event);
    }

    // ToolCallEnd 事件携带完整 id / name / arguments
    let end = events
      .iter()
      .find_map(|event| match event {
        ProviderEvent::ToolCallEnd { tool_call, .. } => Some(tool_call),
        _ => None,
      })
      .expect("ToolCallEnd event");
    assert_eq!(end.id, "toolu_1", "id 应来自 content_block_start 注册");
    assert_eq!(end.name, "lookup", "name 应来自 content_block_start 注册");
    assert_eq!(end.arguments, serde_json::json!({ "q": "rust" }));

    // done 消息的 content 同样完整（finalize 回写 output.content）
    match events.last().expect("terminal event") {
      ProviderEvent::Done { message, .. } => {
        assert_eq!(message.stop_reason, StopReason::ToolUse);
        match &message.content[0] {
          Content::ToolCall(call) => {
            assert_eq!(call.id, "toolu_1", "done 消息 id 不应是空串占位");
            assert_eq!(call.name, "lookup");
            assert_eq!(call.arguments, serde_json::json!({ "q": "rust" }));
          }
          other => panic!("expected ToolCall block, got {other:?}"),
        }
      }
      other => panic!("expected Done, got {other:?}"),
    }
  }
}
