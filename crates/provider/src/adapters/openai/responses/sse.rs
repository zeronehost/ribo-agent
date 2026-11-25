//! OpenAI Responses API（`POST /responses`）流式解析：事件分派 + 事件聚合。
//!
//! 与 Chat Completions 的关键差异：
//!
//! 1. 每个 SSE 帧都带 `event:` 名（如 `response.output_text.delta`），
//!    `data:` 为对应 JSON 载荷；分派以事件名为准（缺省时回退 JSON `type`）；
//! 2. 没有 `[DONE]` 哨兵，正常终结事件是 `response.completed` /
//!    `response.incomplete`；`response.failed` 与 `error` 为错误终结；
//! 3. 工具调用是顶层 `function_call` item，参数增量事件为
//!    `response.function_call_arguments.delta`；
//! 4. 推理摘要增量事件为 `response.reasoning_summary_text.delta` /
//!    `response.reasoning_text.delta`。
//!
//! 传输层（[`SseFrame`] / [`SseParser`]）与错误码映射复用小节
//! [`crate::adapters::openai::sse`]，避免同一线格式二次实现。

use std::collections::{HashMap, HashSet};

use ra_core::message::{StopReason, Usage};

use super::super::sse::map_provider_error;
use crate::{
  adapters::parse_task,
  ids::{ModelId, ProviderId},
  options::CancelToken,
  stream::{LineFlow, ProviderEventStreamProducer, SseFrame, StreamAggregator},
};

// 线格式载荷（只声明读取所需字段）

/// `response.output_item.added` / `response.output_item.done` 载荷。
#[derive(Debug, serde::Deserialize)]
struct OutputItemEvent {
  #[serde(default)]
  output_index: usize,
  #[serde(default)]
  item: WireItem,
}

/// 顶层 output item（`message` / `function_call` / `reasoning` 等）。
#[derive(Debug, Default, serde::Deserialize)]
struct WireItem {
  #[serde(rename = "type", default)]
  item_type: String,
  #[serde(default)]
  id: Option<String>,
  #[serde(default)]
  call_id: Option<String>,
  #[serde(default)]
  name: Option<String>,
  #[serde(default)]
  arguments: Option<String>,
}

/// 文本 / 推理增量载荷（`delta` 为字符串）。
#[derive(Debug, Default, serde::Deserialize)]
struct DeltaEvent {
  #[serde(default)]
  delta: String,
}

/// `response.function_call_arguments.delta` 载荷。
#[derive(Debug, Default, serde::Deserialize)]
struct ArgsDeltaEvent {
  #[serde(default)]
  output_index: usize,
  #[serde(default)]
  item_id: Option<String>,
  #[serde(default)]
  delta: String,
}

/// `response.completed` / `response.incomplete` / `response.failed` 载荷。
#[derive(Debug, Default, serde::Deserialize)]
struct ResponseEvent {
  #[serde(default)]
  response: WireResponse,
}

/// 终结事件中的 response 对象。
#[derive(Debug, Default, serde::Deserialize)]
struct WireResponse {
  #[serde(default)]
  id: Option<String>,
  #[serde(default)]
  model: Option<String>,
  #[serde(default)]
  usage: Option<serde_json::Value>,
  #[serde(default)]
  incomplete_details: Option<IncompleteDetails>,
  #[serde(default)]
  error: Option<serde_json::Value>,
}

/// 未完成原因。
#[derive(Debug, serde::Deserialize)]
struct IncompleteDetails {
  #[serde(default)]
  reason: Option<String>,
}

// 单个流的聚合状态

/// 一次工具调用的元信息（id / 函数名可能只出现在 item 帧，不在 delta 帧）。
#[derive(Debug, Clone)]
struct ItemMeta {
  call_id: String,
  name: String,
}

/// Responses 流的额外状态。
#[derive(Debug, Default)]
struct ResponsesState {
  /// `output_index` → 元信息（聚合器的线格式下标即 output_index）。
  items: HashMap<usize, ItemMeta>,
  /// `item_id` → 元信息（delta 帧只带 item_id 时用于回查）。
  items_by_id: HashMap<String, ItemMeta>,
  /// 已收到参数增量的 output_index（用于 `output_item.done` 去重）。
  args_started: HashSet<usize>,
  /// 是否出现过工具调用（决定 `response.completed` 的 finish_reason）。
  saw_tool: bool,
}

// 帧分派

/// 事件名解析：优先 SSE 的 `event:` 行，缺省回退 JSON 载荷的 `type`。
fn event_kind<'a>(
  frame: &'a SseFrame,
  value: &'a serde_json::Value,
) -> Option<&'a str> {
  frame
    .event
    .as_deref()
    .filter(|name| !name.is_empty())
    .or_else(|| value.get("type").and_then(serde_json::Value::as_str))
}

/// 处理单帧，返回解析去向。
fn dispatch_frame(
  frame: &SseFrame,
  aggregator: &mut StreamAggregator,
  producer: &mut Option<ProviderEventStreamProducer>,
  state: &mut ResponsesState,
) -> LineFlow {
  // 哨兵兼容：少数网关仍在流末尾发 [DONE]
  if frame.data.trim() == "[DONE]" {
    aggregator.mark_done_sentinel();
    aggregator.finalize(producer);
    return LineFlow::Terminated;
  }

  // 载荷解析：非法 JSON → INVALID_RESPONSE 终结
  let value = match serde_json::from_str::<serde_json::Value>(&frame.data) {
    Ok(value) => value,
    Err(err) => {
      tracing::warn!(
        data_len = frame.data.len(),
        error = %err,
        "malformed responses sse frame"
      );
      return parse_task::invalid_response(
        producer,
        aggregator,
        format!("malformed sse data frame: {err}"),
      );
    }
  };

  // 事件名转为自有 String：后续分派需要按值移走 `value`
  let Some(kind) = event_kind(frame, &value).map(str::to_string) else {
    return LineFlow::Continue; // 无名帧：无可用语义，忽略
  };

  match kind.as_str() {
    // 元信息：response.created 只并入快照（Start 已由任务开头推送）
    "response.created" => {
      if let Ok(ev) = serde_json::from_value::<ResponseEvent>(value) {
        aggregator.set_meta(ev.response.id, ev.response.model);
      }
    }

    "response.output_item.added" => {
      return output_item_added(value, aggregator, producer, state);
    }

    "response.output_item.done" => {
      return output_item_done(value, aggregator, producer, state);
    }

    // 文本增量
    "response.output_text.delta" => {
      let Ok(ev) = serde_json::from_value::<DeltaEvent>(value) else {
        return LineFlow::Continue;
      };
      if ev.delta.is_empty() {
        return LineFlow::Continue;
      }
      let Some(p) = producer.as_ref() else {
        return LineFlow::Terminated;
      };
      if !matches!(aggregator.text_delta(p, &ev.delta), LineFlow::Continue) {
        return LineFlow::ConsumerGone;
      }
    }

    // 推理摘要 / 推理正文增量
    "response.reasoning_summary_text.delta"
    | "response.reasoning_text.delta" => {
      let Ok(ev) = serde_json::from_value::<DeltaEvent>(value) else {
        return LineFlow::Continue;
      };
      if ev.delta.is_empty() {
        return LineFlow::Continue;
      }
      let Some(p) = producer.as_ref() else {
        return LineFlow::Terminated;
      };
      if !matches!(aggregator.reasoning_delta(p, &ev.delta), LineFlow::Continue)
      {
        return LineFlow::ConsumerGone;
      }
    }

    // 工具调用参数增量
    "response.function_call_arguments.delta" => {
      return args_delta(value, aggregator, producer, state);
    }

    // 工具调用参数收尾（部分网关只在此帧给全量参数）
    "response.function_call_arguments.done" => {
      return args_done(value, aggregator, producer, state);
    }

    // 正常终结：completed / incomplete
    "response.completed" | "response.incomplete" => {
      let ev = match serde_json::from_value::<ResponseEvent>(value) {
        Ok(ev) => ev,
        Err(err) => {
          tracing::warn!(error = %err, "cannot parse responses terminal event");
          return parse_task::invalid_response(
            producer,
            aggregator,
            format!("cannot parse responses terminal event: {err}"),
          );
        }
      };
      aggregator.set_meta(ev.response.id, ev.response.model);
      if let Some(usage) = &ev.response.usage {
        aggregator.set_usage(parse_usage(usage));
      }
      // 正常完成无显式 stop reason：有工具调用即 ToolUse，否则 Stop；
      // incomplete 的 max_output_tokens 映射为 Length。
      aggregator.set_finish_reason(match kind.as_str() {
        "response.incomplete" => {
          match ev.response.incomplete_details.and_then(|d| d.reason) {
            Some(reason) if reason == "max_output_tokens" => "length",
            Some(reason) => {
              return fail_incomplete(producer, aggregator, &reason);
            }
            None => "stop",
          }
        }
        _ if state.saw_tool => "tool_calls",
        _ => "stop",
      });
      aggregator.mark_done_sentinel();
      aggregator.finalize(producer);
      return LineFlow::Terminated;
    }

    // 错误终结：response.failed
    "response.failed" => {
      let err = serde_json::from_value::<ResponseEvent>(value)
        .ok()
        .and_then(|ev| ev.response.error)
        .unwrap_or_else(|| serde_json::json!({ "message": "response failed" }));
      let code = map_provider_error(&err);
      let text = err
        .get("message")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("response failed");
      let msg = aggregator.terminal_error(StopReason::Error, text);
      if let Some(p) = producer.take() {
        p.error(StopReason::Error, msg, code);
      }
      return LineFlow::Terminated;
    }

    // 错误终结：顶层 error 事件
    "error" => {
      let code = map_provider_error(&value);
      let text = value
        .get("message")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("provider reported an error");
      let msg = aggregator.terminal_error(StopReason::Error, text);
      if let Some(p) = producer.take() {
        p.error(StopReason::Error, msg, code);
      }
      return LineFlow::Terminated;
    }

    other => {
      // 其余事件（content_part.*、output_text.done 等）不改变聚合状态
      tracing::debug!(event = %other, "responses: ignoring event");
    }
  }

  LineFlow::Continue
}

/// `response.output_item.added`：工具调用 item 登记 id / 函数名并开块。
fn output_item_added(
  value: serde_json::Value,
  aggregator: &mut StreamAggregator,
  producer: &mut Option<ProviderEventStreamProducer>,
  state: &mut ResponsesState,
) -> LineFlow {
  let ev = match serde_json::from_value::<OutputItemEvent>(value) {
    Ok(ev) => ev,
    Err(err) => {
      return parse_task::invalid_response(
        producer,
        aggregator,
        format!("cannot parse output_item event: {err}"),
      );
    }
  };
  if ev.item.item_type != "function_call" {
    tracing::debug!(
      item_type = %ev.item.item_type,
      "responses: non tool-call output item"
    );
    return LineFlow::Continue;
  }

  let Some(p) = producer.as_ref() else {
    return LineFlow::Terminated;
  };
  // 回灌工具结果时需要的是 call_id（item.id 是 Responses 内部 item 标识）
  let call_id = ev
    .item
    .call_id
    .clone()
    .or_else(|| ev.item.id.clone())
    .unwrap_or_default();
  let name = ev.item.name.clone().unwrap_or_default();
  let meta = ItemMeta {
    call_id: call_id.clone(),
    name: name.clone(),
  };
  state.items.insert(ev.output_index, meta.clone());
  if let Some(item_id) = &ev.item.id {
    state.items_by_id.insert(item_id.clone(), meta);
  }
  state.saw_tool = true;

  // 参数增量先于 added 帧到达的网关：块已存在，不得重复拼接 id / name
  let (id_fragment, name_fragment) =
    if aggregator.has_tool_block(ev.output_index) {
      (None, None)
    } else {
      (Some(call_id.as_str()), Some(name.as_str()))
    };
  if !matches!(
    aggregator.tool_delta(p, ev.output_index, id_fragment, name_fragment, None),
    LineFlow::Continue
  ) {
    return LineFlow::ConsumerGone;
  }

  // 少数网关在 added 帧就带完整 arguments
  if let Some(args) = ev.item.arguments.filter(|args| !args.is_empty()) {
    if !matches!(
      aggregator.tool_delta(p, ev.output_index, None, None, Some(&args)),
      LineFlow::Continue
    ) {
      return LineFlow::ConsumerGone;
    }
    state.args_started.insert(ev.output_index);
  }
  LineFlow::Continue
}

/// `response.output_item.done`：仅在从未收到参数增量时补喂完整参数。
fn output_item_done(
  value: serde_json::Value,
  aggregator: &mut StreamAggregator,
  producer: &mut Option<ProviderEventStreamProducer>,
  state: &mut ResponsesState,
) -> LineFlow {
  let Ok(ev) = serde_json::from_value::<OutputItemEvent>(value) else {
    return LineFlow::Continue;
  };
  if ev.item.item_type != "function_call"
    || state.args_started.contains(&ev.output_index)
  {
    return LineFlow::Continue;
  }
  let Some(args) = ev.item.arguments.filter(|args| !args.is_empty()) else {
    return LineFlow::Continue;
  };
  let Some(p) = producer.as_ref() else {
    return LineFlow::Terminated;
  };
  // 块已由 earlier 帧登记时不得重复传入 id / name（聚合器按片段拼接）
  let (call_id, name) = if aggregator.has_tool_block(ev.output_index) {
    (None, None)
  } else {
    state
      .items
      .get(&ev.output_index)
      .map_or((None, None), |meta| {
        (Some(meta.call_id.clone()), Some(meta.name.clone()))
      })
  };
  state.args_started.insert(ev.output_index);
  state.saw_tool = true;
  if !matches!(
    aggregator.tool_delta(
      p,
      ev.output_index,
      call_id.as_deref(),
      name.as_deref(),
      Some(&args)
    ),
    LineFlow::Continue
  ) {
    return LineFlow::ConsumerGone;
  }
  LineFlow::Continue
}

/// `response.function_call_arguments.delta`：参数片段累积。
fn args_delta(
  value: serde_json::Value,
  aggregator: &mut StreamAggregator,
  producer: &mut Option<ProviderEventStreamProducer>,
  state: &mut ResponsesState,
) -> LineFlow {
  let Ok(ev) = serde_json::from_value::<ArgsDeltaEvent>(value) else {
    return LineFlow::Continue;
  };
  if ev.delta.is_empty() {
    return LineFlow::Continue;
  }
  let Some(p) = producer.as_ref() else {
    return LineFlow::Terminated;
  };
  // 元信息优先按 item_id 回查（delta 帧通常不带 name / call_id）
  let meta = ev
    .item_id
    .as_ref()
    .and_then(|id| state.items_by_id.get(id))
    .or_else(|| state.items.get(&ev.output_index))
    .cloned();
  // 块已登记（output_item.added 已开块）时只续累积参数，
  // 否则补传入元信息以完成登记。
  let (call_id, name) = if aggregator.has_tool_block(ev.output_index) {
    (None, None)
  } else {
    meta.map_or((None, None), |meta| (Some(meta.call_id), Some(meta.name)))
  };
  state.args_started.insert(ev.output_index);
  state.saw_tool = true;
  if !matches!(
    aggregator.tool_delta(
      p,
      ev.output_index,
      call_id.as_deref(),
      name.as_deref(),
      Some(&ev.delta)
    ),
    LineFlow::Continue
  ) {
    return LineFlow::ConsumerGone;
  }
  LineFlow::Continue
}

/// `response.function_call_arguments.done`：与 item.done 同语义的收尾兜底。
fn args_done(
  value: serde_json::Value,
  aggregator: &mut StreamAggregator,
  producer: &mut Option<ProviderEventStreamProducer>,
  state: &mut ResponsesState,
) -> LineFlow {
  let Ok(ev) = serde_json::from_value::<ArgsDeltaEvent>(value) else {
    return LineFlow::Continue;
  };
  output_item_done(
    serde_json::json!({
      "output_index": ev.output_index,
      "item": {
        "type": "function_call",
        "arguments": ev.delta,
      },
    }),
    aggregator,
    producer,
    state,
  )
}

/// `response.incomplete` 的非长度原因（如 content_filter）终结为错误。
fn fail_incomplete(
  producer: &mut Option<ProviderEventStreamProducer>,
  aggregator: &StreamAggregator,
  reason: &str,
) -> LineFlow {
  tracing::warn!(reason = %reason, "responses stream incomplete");
  let message = aggregator.terminal_error(
    StopReason::Error,
    format!("response incomplete: {reason}"),
  );
  // content_filter → 内容安全码；其余按未分类错误
  let code = if reason == "content_filter" {
    Some(crate::error::CONTENT_FILTERED)
  } else {
    None
  };
  if let Some(p) = producer.take() {
    p.error(StopReason::Error, message, code);
  }
  LineFlow::Terminated
}

// 用量解析

/// Responses `usage` → 统一 [`Usage`]。
///
/// `input_tokens` 含缓存命中部分，而 [`Usage::input`] 语义为「未命中缓存的
/// 纯输入」，故做减去并 `max(0)` 防御口径漂移。
fn parse_usage(value: &serde_json::Value) -> Usage {
  let input_total = value
    .get("input_tokens")
    .and_then(serde_json::Value::as_i64)
    .unwrap_or(0);
  let output = value
    .get("output_tokens")
    .and_then(serde_json::Value::as_i64)
    .unwrap_or(0);
  let cache_read = value
    .get("input_tokens_details")
    .and_then(|details| details.get("cached_tokens"))
    .and_then(serde_json::Value::as_i64)
    .unwrap_or(0);
  let reasoning = value
    .get("output_tokens_details")
    .and_then(|details| details.get("reasoning_tokens"))
    .and_then(serde_json::Value::as_i64);
  let input = (input_total - cache_read).max(0);

  Usage {
    input,
    output,
    cache_read,
    cache_write: 0,
    cache_write_1h: None,
    reasoning,
    total_tokens: input + output + cache_read,
    cost: Default::default(),
  }
}

// 解析任务

/// Responses 流的 SSE 解析任务（适配器建流成功后调用）。
///
/// 生产者义务（`Start` 先行、每条路径恰好终结一次、`push` 失败即停止）
/// 全部落在共享骨架 [`parse_task::spawn_sse_task`]；方言私有状态
/// [`ResponsesState`] 由闭包捕获，随任务结束释放。
pub(crate) fn spawn_parse_task(
  response: reqwest::Response,
  producer: ProviderEventStreamProducer,
  cancel: CancelToken,
  provider: ProviderId,
  model: ModelId,
) {
  let mut state = ResponsesState::default();
  parse_task::spawn_sse_task(
    response,
    producer,
    cancel,
    provider,
    model,
    move |frame, aggregator, producer| {
      dispatch_frame(frame, aggregator, producer, &mut state)
    },
  );
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::stream::ProviderEvent;
  use crate::test_support::collect_events;

  // 测试辅助

  // 构造带 event 名的 SSE 帧（Responses 线格式总是带事件名）。
  fn frame(kind: &str, data: serde_json::Value) -> SseFrame {
    SseFrame {
      event: Some(kind.to_string()),
      data: data.to_string(),
    }
  }

  /// 场景：文本增量 → completed 终结。
  ///
  /// 预期：TextStart / TextDelta / TextEnd / Done 依次到达，Done 消息含聚合文本。
  #[tokio::test]
  async fn text_delta_then_completed_emits_full_sequence() {
    let (producer, mut stream) = ProviderEventStreamProducer::build();
    let mut producer = Some(producer);
    let mut agg = crate::test_support::aggregator("openai", "gpt-4o");
    let mut state = ResponsesState::default();
    if let Some(p) = producer.as_ref() {
      let start = ProviderEvent::Start {
        partial: agg.snapshot(),
      };
      assert!(p.push(start));
    }

    let flow = dispatch_frame(
      &frame(
        "response.output_text.delta",
        serde_json::json!({ "delta": "hello" }),
      ),
      &mut agg,
      &mut producer,
      &mut state,
    );
    assert!(matches!(flow, LineFlow::Continue));

    let flow = dispatch_frame(
      &frame(
        "response.completed",
        serde_json::json!({ "response": { "id": "resp_1", "model": "gpt-4o" } }),
      ),
      &mut agg,
      &mut producer,
      &mut state,
    );
    assert!(matches!(flow, LineFlow::Terminated));

    let events = collect_events(&mut stream).await;
    assert!(
      events
        .iter()
        .any(|e| matches!(e, ProviderEvent::TextStart { .. }))
    );
    assert!(events.iter().any(|e| matches!(
      e,
      ProviderEvent::TextDelta { delta, .. } if delta == "hello"
    )));
    match events.last().expect("terminal event") {
      ProviderEvent::Done { reason, message } => {
        assert_eq!(*reason, StopReason::Stop);
        assert!(matches!(
          message.content.first(),
          Some(ra_core::message::Content::Text(t)) if t.content == "hello"
        ));
      }
      other => panic!("expected Done, got {other:?}"),
    }
  }

  /// 场景：function_call item + 参数增量 → completed。
  ///
  /// 预期：ToolCallStart / ToolCallDelta / ToolCallEnd；done 消息携带
  /// call_id 作为 id、函数名与解析后的参数；stop_reason 为 ToolUse。
  #[tokio::test]
  async fn function_call_item_aggregates_tool_call() {
    let (producer, mut stream) = ProviderEventStreamProducer::build();
    let mut producer = Some(producer);
    let mut agg = crate::test_support::aggregator("openai", "gpt-4o");
    let mut state = ResponsesState::default();
    if let Some(p) = producer.as_ref() {
      let start = ProviderEvent::Start {
        partial: agg.snapshot(),
      };
      assert!(p.push(start));
    }

    dispatch_frame(
      &frame(
        "response.output_item.added",
        serde_json::json!({
          "output_index": 0,
          "item": {
            "type": "function_call",
            "id": "fc_1",
            "call_id": "call_1",
            "name": "lookup"
          }
        }),
      ),
      &mut agg,
      &mut producer,
      &mut state,
    );
    dispatch_frame(
      &frame(
        "response.function_call_arguments.delta",
        serde_json::json!({
          "output_index": 0,
          "item_id": "fc_1",
          "delta": "{\"q\":\"rust\"}"
        }),
      ),
      &mut agg,
      &mut producer,
      &mut state,
    );
    let flow = dispatch_frame(
      &frame(
        "response.completed",
        serde_json::json!({ "response": { "usage": { "input_tokens": 10, "output_tokens": 5 } } }),
      ),
      &mut agg,
      &mut producer,
      &mut state,
    );
    assert!(matches!(flow, LineFlow::Terminated));

    let events = collect_events(&mut stream).await;
    assert!(
      events
        .iter()
        .any(|e| matches!(e, ProviderEvent::ToolCallStart { .. }))
    );
    assert!(events.iter().any(|e| matches!(
      e,
      ProviderEvent::ToolCallDelta { delta, .. } if delta == "{\"q\":\"rust\"}"
    )));
    match events.last().expect("terminal event") {
      ProviderEvent::Done { reason, message } => {
        assert_eq!(*reason, StopReason::ToolUse);
        assert_eq!(message.usage.input, 10);
        assert_eq!(message.usage.output, 5);
        assert!(matches!(
          message.content.first(),
          Some(ra_core::message::Content::ToolCall(call))
            if call.id == "call_1"
              && call.name == "lookup"
              && call.arguments == serde_json::json!({ "q": "rust" })
        ));
      }
      other => panic!("expected Done, got {other:?}"),
    }
  }

  /// 场景：`response.failed` 错误终结。
  ///
  /// 预期：Error 事件且 code 映射为 RATE_LIMITED。
  #[tokio::test]
  async fn failed_event_terminates_with_mapped_error() {
    let (producer, mut stream) = ProviderEventStreamProducer::build();
    let mut producer = Some(producer);
    let mut agg = crate::test_support::aggregator("openai", "gpt-4o");
    let mut state = ResponsesState::default();

    let flow = dispatch_frame(
      &frame(
        "response.failed",
        serde_json::json!({
          "response": {
            "error": {
              "code": "rate_limit_exceeded",
              "message": "too many requests"
            }
          }
        }),
      ),
      &mut agg,
      &mut producer,
      &mut state,
    );
    assert!(matches!(flow, LineFlow::Terminated));

    let events = collect_events(&mut stream).await;
    match events.last().expect("terminal event") {
      ProviderEvent::Error { code, message, .. } => {
        assert_eq!(*code, Some(crate::error::RATE_LIMITED));
        assert_eq!(message.error_message.as_deref(), Some("too many requests"));
      }
      other => panic!("expected Error, got {other:?}"),
    }
  }

  /// 场景：EOF 提前断开、未收到 completed（无哨兵）。
  ///
  /// 预期：finalize 走截断分支，以 Error + STREAM_INTERRUPTED 终结。
  #[tokio::test]
  async fn eof_without_completed_is_truncated() {
    let (producer, mut stream) = ProviderEventStreamProducer::build();
    let mut producer = Some(producer);
    let mut agg = crate::test_support::aggregator("openai", "gpt-4o");
    let mut state = ResponsesState::default();
    if let Some(p) = producer.as_ref() {
      let start = ProviderEvent::Start {
        partial: agg.snapshot(),
      };
      assert!(p.push(start));
    }

    dispatch_frame(
      &frame(
        "response.output_text.delta",
        serde_json::json!({ "delta": "partial" }),
      ),
      &mut agg,
      &mut producer,
      &mut state,
    );
    // 模拟读流结束：没有 completed → finalize 走截断路径
    agg.finalize(&mut producer);

    let events = collect_events(&mut stream).await;
    match events.last().expect("terminal event") {
      ProviderEvent::Error { code, .. } => {
        assert_eq!(*code, Some(crate::error::STREAM_INTERRUPTED));
      }
      other => panic!("expected truncation Error, got {other:?}"),
    }
  }
}
