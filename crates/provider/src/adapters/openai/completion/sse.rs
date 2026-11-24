//! OpenAI 兼容适配器的流式解析：SSE 传输层 + 帧分派 + 事件聚合。
//!
//! 职责边界：把 `reqwest::Response`
//! 的字节流折叠为 [`ProviderEvent`] 序列并推入生产端。
//!
//! 模块分层：
//!
//! 1. **传输层**：[`SseParser`] 字节 → 行 → 帧（`feed` / `finish`）；
//! 2. **分派层**：[`dispatch_frame`] 帧 → 事件（含 `[DONE]` 哨兵、usage、error）；
//! 3. **聚合层**：[`StreamAggregator`] 维护半成品快照并生成 `partial`；
//! 4. **错误映射**：[`map_provider_error`] 提供方错误体 → 统一错误码。
//!
//! 终结纪律：生产端以 `Option` 持有，`done` / `error` 消费 `self`，
//! 每条路径先 `take()`，保证恰好终结一次。

use ra_core::message::{StopReason, Usage};

use super::super::sse::map_provider_error;
use crate::{
  adapters::parse_task,
  ids::{ModelId, ProviderId},
  options::CancelToken,
  stream::{LineFlow, ProviderEventStreamProducer, SseFrame, StreamAggregator},
};

// ============================================================================
// 线格式（只声明读取所需字段，其余由 serde 忽略）
// ============================================================================

/// 一帧增量载荷。
#[derive(Debug, Default, serde::Deserialize)]
struct WireChunk {
  /// 响应 id（首个非空值写入 `response_id`）。
  #[serde(default)]
  id: Option<String>,
  /// 模型名（仅用于日志比对，归属以请求的 `ModelId` 为准）。
  #[serde(default)]
  model: Option<String>,
  /// 增量选项；流式帧通常只有一个元素，空数组视为纯元数据帧。
  #[serde(default)]
  choices: Vec<WireChoice>,
  /// 末帧用量（`stream_options.include_usage` 开启时出现在顶层）。
  #[serde(default)]
  usage: Option<serde_json::Value>,
  /// 部分网关以 HTTP 200 + `error` 字段透出流内错误（如 OpenRouter）。
  #[serde(default)]
  error: Option<serde_json::Value>,
}

/// 单个增量选项。
#[derive(Debug, Default, serde::Deserialize)]
struct WireChoice {
  /// 增量载荷。
  #[serde(default)]
  delta: WireDelta,
  /// 终止原因；中间帧为 `null`，末帧为 `"stop"` 等（映射见
  /// [`map_stop_reason`]）。
  #[serde(default)]
  finish_reason: Option<String>,
  /// 非标准位置：部分提供方把 usage 放在 choice 内（兜底读取）。
  #[serde(default)]
  usage: Option<serde_json::Value>,
}

/// 增量载荷：三类内容可单独出现，也可同帧混杂（宽松解析不假设互斥）。
#[derive(Debug, Default, serde::Deserialize)]
struct WireDelta {
  /// 文本增量；缺失与空串等价（部分提供方用空串占位）。
  #[serde(default)]
  content: Option<String>,
  /// 思考增量；不同提供方命名不一，靠别名归一（DeepSeek 用
  /// `reasoning_content`，部分网关用 `reasoning` / `reasoning_text`）。
  #[serde(default, alias = "reasoning", alias = "reasoning_text")]
  reasoning_content: Option<String>,
  /// 工具调用增量分片，按 `index` 聚合。
  #[serde(default)]
  tool_calls: Vec<WireToolCall>,
}

/// 工具调用增量分片。
#[derive(Debug, Default, serde::Deserialize)]
struct WireToolCall {
  /// 聚合下标：同一次响应内同 `index` 的分片属于同一次调用。
  #[serde(default)]
  index: usize,
  /// 调用 id（首分片携带，后续分片缺省）。
  #[serde(default)]
  id: Option<String>,
  /// 函数名与参数增量。
  #[serde(default)]
  function: Option<WireFunction>,
}

/// 函数调用增量。
#[derive(Debug, Default, serde::Deserialize)]
struct WireFunction {
  /// 函数名（首分片携带）。
  #[serde(default)]
  name: Option<String>,
  /// 参数 JSON 片段（跨分片拼接后方为完整 JSON）。
  #[serde(default)]
  arguments: Option<String>,
}

/// 从 usage 载荷解析统一用量；字段缺失一律取 0，不判错）。
///
/// 解析步骤：
/// 1. 读取基准计数 `prompt_tokens` / `completion_tokens`（各提供方一致上报）；
/// 2. 缓存命中按「官方 details 对象 → 平铺别名 → 顶层同名」三级兜底；
/// 3. `input` 扣除缓存部分——启用缓存的提供方把缓存计入 `prompt_tokens`，
///    而 [`Usage::input`] 语义是「未命中缓存的纯输入」——并 `max(0)` 防御
///    个别提供方的口径漂移；
/// 4. `total_tokens` 按统一口径求和。
fn parse_usage(value: &serde_json::Value) -> Usage {
  // ===== 第一步：基准计数 =====
  let prompt_tokens = value
    .get("prompt_tokens")
    .and_then(serde_json::Value::as_i64)
    .unwrap_or(0);
  let output = value
    .get("completion_tokens")
    .and_then(serde_json::Value::as_i64)
    .unwrap_or(0);

  // ===== 第二步：缓存命中三级兜底 =====
  // 1) prompt_tokens_details.cached_tokens（OpenAI 官方 details 对象）；
  // 2) prompt_cache_hit_tokens（DeepSeek 风格平铺字段）；
  // 3) 顶层 cached_tokens（部分聚合网关）。
  let cache_read = value
    .get("prompt_tokens_details")
    .and_then(|v| v.get("cached_tokens"))
    .and_then(serde_json::Value::as_i64)
    .or_else(|| {
      value
        .get("prompt_cache_hit_tokens")
        .and_then(serde_json::Value::as_i64)
    })
    .or_else(|| {
      value
        .get("cached_tokens")
        .and_then(serde_json::Value::as_i64)
    })
    .unwrap_or(0);
  // 缓存写入只有 details 对象一种上报形态，无别名需求。
  let cache_write = value
    .get("prompt_tokens_details")
    .and_then(|v| v.get("cache_write_tokens"))
    .and_then(serde_json::Value::as_i64)
    .unwrap_or(0);

  // ===== 第三步：推导未缓存输入与推理计数 =====
  let input = (prompt_tokens - cache_read - cache_write).max(0);
  let reasoning = value
    .get("completion_tokens_details")
    .and_then(|v| v.get("reasoning_tokens"))
    .and_then(serde_json::Value::as_i64);

  Usage {
    input,
    output,
    cache_read,
    cache_write,
    cache_write_1h: None,
    reasoning,
    total_tokens: input + output + cache_read + cache_write,
    cost: Default::default(),
  }
}

// ============================================================================
// 帧分派
// ============================================================================

/// 把一帧线格式数据分派为事件。
///
/// # 返回
///
/// - [`LineFlow::Continue`]：继续解析下一帧；
/// - [`LineFlow::Terminated`]：本帧触发终结（`done` / `error` 已消费 producer）；
/// - [`LineFlow::ConsumerGone`]：`push` 返回 `false`，消费端已放弃。
fn dispatch_frame(
  frame: &SseFrame,
  aggregator: &mut StreamAggregator,
  producer: &mut Option<ProviderEventStreamProducer>,
) -> LineFlow {
  // ===== 非标准 event 名：仅记录不参与分派（宽松解析）=====
  // OpenAI 兼容流为 data-only；debug 日志用于诊断发送异常 event
  // 语义的服务商（每帧至多一条，且仅 event 名非空时）。
  if let Some(name) = &frame.event {
    tracing::debug!(sse_event = %name, "ignoring sse event name");
  }

  // ===== [DONE] 哨兵：聚合完成并成功终结 =====
  if frame.data.trim() == "[DONE]" {
    aggregator.mark_done_sentinel(); // R2 截断检测置位：正常终结路径
    aggregator.finalize(producer); // 幂等：内部以 finished 标志防重入
    return LineFlow::Terminated;
  }

  // ===== 载荷解析：JSON 非法 / 形状不符 → INVALID_RESPONSE 终结=====
  let chunk: WireChunk = match serde_json::from_str(&frame.data) {
    Ok(chunk) => chunk,
    Err(err) => {
      tracing::warn!(
        data_len = frame.data.len(),
        error = %err,
        "malformed sse data frame"
      );
      return parse_task::invalid_response(
        producer,
        aggregator,
        format!("malformed sse data frame: {err}"),
      );
    }
  };

  // ===== 元信息与用量：并入 partial 快照，不单独发事件 =====
  aggregator.set_meta(chunk.id, chunk.model);
  // usage 通常在 chunk 顶层（末帧）；兼容部分服务商放在 choice 内
  let usage = chunk
    .usage
    .as_ref()
    .or_else(|| chunk.choices.first().and_then(|c| c.usage.as_ref()));
  if let Some(usage) = usage {
    aggregator.set_usage(parse_usage(usage));
  }

  // ===== 流内业务错误：映射错误码后终结 =====
  if let Some(err) = &chunk.error {
    let code = map_provider_error(err);
    let text = err
      .get("message")
      .and_then(serde_json::Value::as_str)
      .unwrap_or("provider reported an error");
    let msg = aggregator.terminal_error(StopReason::Error, text);
    if let Some(p) = producer.take() {
      p.error(StopReason::Error, msg, code);
    }
    return LineFlow::Terminated;
  }

  // ===== 增量分派：此处置 producer 必仍为 Some——所有终结路径均立即返回 =====
  let Some(p) = producer.as_ref() else {
    return LineFlow::Terminated;
  };
  for choice in &chunk.choices {
    // finish_reason：记录原文，映射推迟到 finalize
    if let Some(raw) = &choice.finish_reason {
      aggregator.set_finish_reason(raw);
    }
    let delta = &choice.delta;
    // 推理增量：块未打开时聚合器自动补 ReasoningStart（块纪律自愈）
    if let Some(text) =
      delta.reasoning_content.as_deref().filter(|s| !s.is_empty())
      && let flow @ (LineFlow::Terminated | LineFlow::ConsumerGone) =
        aggregator.reasoning_delta(p, text)
    {
      return flow;
    }
    // 文本增量（块首 → TextStart + TextDelta；块内 → TextDelta）
    if let Some(text) = delta.content.as_deref().filter(|s| !s.is_empty())
      && let flow @ (LineFlow::Terminated | LineFlow::ConsumerGone) =
        aggregator.text_delta(p, text)
    {
      return flow;
    }
    // 工具调用增量：按线格式下标聚块（同一下标 = 同一块）
    for tc in &delta.tool_calls {
      // 必需结构判定：块未登记且本帧无 id → 无法归属
      if !aggregator.has_tool_block(tc.index) && tc.id.is_none() {
        return parse_task::invalid_response(
          producer,
          aggregator,
          "tool call fragment without id",
        );
      }
      if let flow @ (LineFlow::Terminated | LineFlow::ConsumerGone) = aggregator
        .tool_delta(
          p,
          tc.index,
          tc.id.as_deref(),
          tc.function.as_ref().and_then(|f| f.name.as_deref()),
          tc.function.as_ref().and_then(|f| f.arguments.as_deref()),
        )
      {
        return flow;
      }
    }
  }
  LineFlow::Continue
}

// ============================================================================
// 解析任务
// ============================================================================

/// SSE 解析任务（由适配器 `stream` 建流成功后调用）。
///
/// 生产者义务（`Start` 先行、每条路径恰好终结一次、`push` 失败即停止）
/// 全部落在共享骨架 [`parse_task::spawn_sse_task`]；本函数只注入
/// Chat Completions 方言的帧分派。
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
    error::{
      AUTH_FAILED, CONTENT_FILTERED, INVALID_RESPONSE, QUOTA_EXCEEDED,
      RATE_LIMITED, TIMEOUT,
    },
    stream::{ProviderEvent, SseParser, map_stop_reason},
    test_support::collect_events,
  };
  use ra_core::message::Content;

  // ===== 测试辅助构造 =====
  // 聚合器与事件流拉取由 crate::test_support 提供。

  // 场景：CRLF 行结束符 + 注释行 + 空行分帧。
  // 预期：注释与空行不产生帧，data 载荷剥离 \r 后成单帧。
  #[test]
  fn feed_handles_crlf_comments_and_blank_lines() {
    let mut parser = SseParser::default();
    let frames = parser.feed(b": keep-alive\r\n\r\ndata: {\"a\":1}\r\n\r\n");
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].data, "{\"a\":1}");
    assert!(frames[0].event.is_none());
  }

  // 场景：一行数据跨多次 chunk 到达。
  // 预期：不完整的行留在缓冲中，直到空行到达才成帧。
  #[test]
  fn feed_buffers_partial_lines_across_chunks() {
    let mut parser = SseParser::default();
    assert!(parser.feed(b"data: {\"a\":").is_empty());
    assert_eq!(parser.feed(b"1}\n\n").len(), 1);
    assert!(parser.feed(b"\n").is_empty());
  }

  // 场景：同一帧内多行 data 与 event 字段。
  // 预期：多行 data 以 '\n' 连接，event 名随帧返回。
  #[test]
  fn feed_joins_multi_line_data_and_keeps_event_name() {
    let mut parser = SseParser::default();
    let frames = parser.feed(b"event: delta\ndata: line1\ndata: line2\n\n");
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].event.as_deref(), Some("delta"));
    assert_eq!(frames[0].data, "line1\nline2");
  }

  // 场景：纯 event 无 data 的心跳帧。
  // 预期：不产生帧（避免被误判为 INVALID_RESPONSE）。
  #[test]
  fn push_line_drops_event_only_frames() {
    let mut parser = SseParser::default();
    assert!(parser.feed(b"event: ping\n\n").is_empty());
  }

  // 场景：末帧不带空行（部分服务商行为）。
  // 预期：finish 冲刷残余行并按 data 非空成帧。
  #[test]
  fn finish_flushes_trailing_data_without_blank_line() {
    let mut parser = SseParser::default();
    parser.feed(b"data: tail");
    let frames = parser.finish();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].data, "tail");
  }

  // 场景：finish_reason 的权威映射。
  // 预期：已知值精确映射；未知值回落 Stop 并保留原文；None 得 Stop。
  #[test]
  fn map_stop_reason_covers_known_and_unknown_values() {
    assert_eq!(map_stop_reason(Some("stop")), (StopReason::Stop, None));
    assert_eq!(map_stop_reason(Some("length")), (StopReason::Length, None));
    assert_eq!(
      map_stop_reason(Some("tool_calls")),
      (StopReason::ToolUse, None)
    );
    assert_eq!(
      map_stop_reason(Some("weird_reason")),
      (StopReason::Stop, Some("weird_reason".to_string()))
    );
    assert_eq!(map_stop_reason(None), (StopReason::Stop, None));
  }

  // 场景：usage 解析的三级缓存别名兜底。
  // 预期：官方 details 对象优先；DeepSeek 平铺别名次之；input 扣除缓存部分。
  #[test]
  fn parse_usage_prefers_official_details_then_aliases() {
    // 1) 官方 details 对象
    let usage = parse_usage(&serde_json::json!({
      "prompt_tokens": 100,
      "completion_tokens": 20,
      "prompt_tokens_details": { "cached_tokens": 30, "cache_write_tokens": 10 },
      "completion_tokens_details": { "reasoning_tokens": 5 }
    }));
    assert_eq!(usage.input, 60);
    assert_eq!(usage.output, 20);
    assert_eq!(usage.cache_read, 30);
    assert_eq!(usage.cache_write, 10);
    assert_eq!(usage.total_tokens, 120);
    assert_eq!(usage.reasoning, Some(5));

    // 2) DeepSeek 平铺别名
    let usage = parse_usage(&serde_json::json!({
      "prompt_tokens": 100,
      "completion_tokens": 20,
      "prompt_cache_hit_tokens": 40
    }));
    assert_eq!(usage.cache_read, 40);
    assert_eq!(usage.input, 60);

    // 3) 字段缺失一律取 0
    let usage = parse_usage(&serde_json::json!({}));
    assert_eq!(usage.input + usage.output + usage.cache_read, 0);
  }

  // 场景：错误映射的两级优先级。
  // 预期：显式 code 优先；未识别 code 落入 message 关键词；
  // 内容安全关键词优先于其他关键词；无法分类返回 None。
  #[test]
  fn map_provider_error_prefers_explicit_then_keywords() {
    // 显式 code 精确匹配
    assert_eq!(
      map_provider_error(&serde_json::json!({ "code": "insufficient_quota" })),
      Some(QUOTA_EXCEEDED)
    );
    // 显式 type 匹配（code 缺失时读 type）
    assert_eq!(
      map_provider_error(&serde_json::json!({ "type": "rate_limit_error" })),
      Some(RATE_LIMITED)
    );
    // 未识别 code → message 关键词兜底
    assert_eq!(
      map_provider_error(&serde_json::json!({
        "code": "unknown_code", "message": "Your api key is invalid"
      })),
      Some(AUTH_FAILED)
    );
    // 内容安全优先于限流关键词
    assert_eq!(
      map_provider_error(&serde_json::json!({
        "message": "rate limited due to content management policy"
      })),
      Some(CONTENT_FILTERED)
    );
    // 无法分类
    assert_eq!(
      map_provider_error(&serde_json::json!({ "message": "something else" })),
      None
    );
  }

  // 场景：文本增量的块纪律自愈 + finalize 的 End 补发与 Done 终结。
  // 预期：事件序列 Start（spawn 外不验）→ TextStart → TextDelta* →
  // TextEnd → Done；最终消息内容与 usage / 停止原因正确。
  #[tokio::test]
  async fn text_delta_then_finalize_emits_full_sequence() {
    let (producer, mut stream) = ProviderEventStreamProducer::build();
    let mut producer = Some(producer);
    let mut aggregator = crate::test_support::aggregator("p1", "m1");

    // Start 先行（模拟 spawn_parse_task 的首事件义务）
    if let Some(p) = producer.as_ref() {
      p.push(ProviderEvent::Start {
        partial: aggregator.snapshot(),
      });
    }
    // 两次增量：首次自动开块
    assert!(matches!(
      aggregator.text_delta(producer.as_ref().expect("alive"), "he"),
      LineFlow::Continue
    ));
    assert!(matches!(
      aggregator.text_delta(producer.as_ref().expect("alive"), "llo"),
      LineFlow::Continue
    ));
    aggregator.set_usage(parse_usage(&serde_json::json!({
      "prompt_tokens": 3, "completion_tokens": 2
    })));
    aggregator.set_finish_reason("stop");
    aggregator.mark_done_sentinel(); // R2: 模拟 [DONE] 哨兵已到达，走正常收尾路径
    aggregator.finalize(&mut producer);

    let events = collect_events(&mut stream).await;
    assert!(matches!(&events[0], ProviderEvent::Start { .. }));
    assert!(matches!(
      &events[1],
      ProviderEvent::TextStart { index: 0, .. }
    ));
    assert!(matches!(
      &events[2],
      ProviderEvent::TextDelta { index: 0, delta, .. } if delta == "he"
    ));
    assert!(matches!(
      &events[events.len() - 2],
      ProviderEvent::TextEnd { index: 0, content, .. } if content == "hello"
    ));
    let terminal = events.last().expect("terminal event");
    assert!(matches!(terminal, ProviderEvent::Done { .. }));
    if let ProviderEvent::Done { reason, message } = terminal {
      assert_eq!(*reason, StopReason::Stop);
      assert_eq!(message.content.len(), 1);
      assert_eq!(message.usage.input, 3);
      assert_eq!(message.usage.output, 2);
    }
    // finalize 幂等：重复调用为 no-op
    let mut none_producer: Option<ProviderEventStreamProducer> = None;
    aggregator.finalize(&mut none_producer);
  }

  // 场景：工具调用分片缺 id 且块未登记（必需结构缺失）。
  // 预期：dispatch 以 INVALID_RESPONSE 终结流。
  #[tokio::test]
  async fn tool_fragment_without_id_terminates_with_invalid_response() {
    let (producer, mut stream) = ProviderEventStreamProducer::build();
    let mut producer = Some(producer);
    let mut aggregator = crate::test_support::aggregator("p1", "m1");
    if let Some(p) = producer.as_ref() {
      p.push(ProviderEvent::Start {
        partial: aggregator.snapshot(),
      });
    }

    let frame = SseFrame {
      event: None,
      data: r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{}"}}]}}]}"#
        .to_string(),
    };
    let flow = dispatch_frame(&frame, &mut aggregator, &mut producer);
    assert!(matches!(flow, LineFlow::Terminated));

    let events = collect_events(&mut stream).await;
    match events.last().expect("terminal event") {
      ProviderEvent::Error { code, .. } => {
        assert_eq!(*code, Some(INVALID_RESPONSE))
      }
      other => panic!("expected Error event, got {other:?}"),
    }
  }

  // 场景：完整分帧 → 分派 → 聚合 → finalize 的端到端路径。
  // 预期：多帧文本增量聚为单块，[DONE] 帧触发终结；事件流按序完整。
  #[tokio::test]
  async fn dispatch_frames_end_to_end_produce_done() {
    let (producer, mut stream) = ProviderEventStreamProducer::build();
    let mut producer = Some(producer);
    let mut aggregator = crate::test_support::aggregator("p1", "m1");
    if let Some(p) = producer.as_ref() {
      p.push(ProviderEvent::Start {
        partial: aggregator.snapshot(),
      });
    }

    // 逐帧喂入：两个文本增量 + finish_reason + [DONE]
    let frames = [
      r#"{"choices":[{"delta":{"content":"wor"}}]}"#,
      r#"{"choices":[{"delta":{"content":"ld"},"finish_reason":"stop"}]}"#,
      r#"{"choices":[{"delta":{},"usage":{"prompt_tokens":1,"completion_tokens":1}}]}"#,
      "[DONE]",
    ];
    for data in frames {
      let flow = dispatch_frame(
        &SseFrame {
          event: None,
          data: data.to_string(),
        },
        &mut aggregator,
        &mut producer,
      );
      assert!(matches!(flow, LineFlow::Continue | LineFlow::Terminated));
      if data == "[DONE]" {
        assert!(matches!(flow, LineFlow::Terminated));
      }
    }

    let events = collect_events(&mut stream).await;
    // Start + TextStart + 2*TextDelta + TextEnd + Done
    assert_eq!(events.len(), 6);
    match events.last().expect("terminal event") {
      ProviderEvent::Done { message, .. } => {
        assert_eq!(message.usage.input, 1);
        assert_eq!(message.usage.output, 1);
      }
      other => panic!("expected Done event, got {other:?}"),
    }
  }

  /// 场景：推理增量（`reasoning_content`）的分派与收尾。
  ///
  /// 预期：自动补 `ReasoningStart`，增量累积到思考块，
  /// finalize 产出 `ReasoningEnd` 与携带思考内容的终态消息。
  #[tokio::test]
  async fn dispatch_reasoning_delta_then_finalize() {
    let (producer, mut stream) = ProviderEventStreamProducer::build();
    let mut producer = Some(producer);
    let mut aggregator = crate::test_support::aggregator("p1", "m1");
    producer
      .as_ref()
      .expect("alive")
      .push(ProviderEvent::Start {
        partial: aggregator.snapshot(),
      });

    for data in [
      r#"{"choices":[{"delta":{"reasoning_content":"thin"}}]}"#,
      r#"{"choices":[{"delta":{"reasoning_content":"king"},"finish_reason":"stop"}]}"#,
      "[DONE]",
    ] {
      dispatch_frame(
        &SseFrame {
          event: None,
          data: data.to_string(),
        },
        &mut aggregator,
        &mut producer,
      );
    }

    let events = collect_events(&mut stream).await;
    assert!(
      events
        .iter()
        .any(|e| matches!(e, ProviderEvent::ReasoningStart { index: 0, .. }))
    );
    assert!(events.iter().any(|e| matches!(
      e,
      ProviderEvent::ReasoningDelta { delta, .. } if delta == "thin"
    )));
    match events.last().expect("terminal event") {
      ProviderEvent::Done { message, .. } => {
        assert_eq!(message.stop_reason, StopReason::Stop);
        assert!(matches!(
          &message.content[0],
          Content::Thinking(thinking) if thinking.content == "thinking"
        ));
      }
      other => panic!("expected Done event, got {other:?}"),
    }
  }

  /// 场景：工具调用分片（id / 函数名 / 参数分段到达）。
  ///
  /// 预期：聚合为单个工具调用块，finalize 产出 `ToolCallEnd` 与
  /// `ToolUse` 停止原因，参数被解析为 JSON 对象。
  #[tokio::test]
  async fn dispatch_tool_delta_accumulates_and_finalizes() {
    let (producer, mut stream) = ProviderEventStreamProducer::build();
    let mut producer = Some(producer);
    let mut aggregator = crate::test_support::aggregator("p1", "m1");
    producer
      .as_ref()
      .expect("alive")
      .push(ProviderEvent::Start {
        partial: aggregator.snapshot(),
      });

    for data in [
      r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"lookup","arguments":"{\"q\":"}}]}}]}"#,
      r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"rust\"}"}}]},"finish_reason":"tool_calls"}]}"#,
      "[DONE]",
    ] {
      if data == "[DONE]" {
        assert!(aggregator.tools.contains_key(&0), "工具块应已建立");
      }
      dispatch_frame(
        &SseFrame {
          event: None,
          data: data.to_string(),
        },
        &mut aggregator,
        &mut producer,
      );
    }

    let events = collect_events(&mut stream).await;
    // 终结事件中的工具调用应携带完整聚合后的参数
    let tool_call = events
      .iter()
      .find_map(|event| match event {
        ProviderEvent::ToolCallEnd { tool_call, .. } => Some(tool_call),
        _ => None,
      })
      .expect("应收到 ToolCallEnd");
    assert_eq!(tool_call.id, "call_1");
    assert_eq!(tool_call.name, "lookup");
    assert_eq!(tool_call.arguments, serde_json::json!({ "q": "rust" }));

    match events.last().expect("terminal event") {
      ProviderEvent::Done { message, .. } => {
        assert_eq!(message.stop_reason, StopReason::ToolUse);
      }
      other => panic!("expected Done event, got {other:?}"),
    }
    // 工具参数已在聚合结果中落盘
    assert_eq!(aggregator.tools.len(), 0, "finalize 应清空工具状态");
  }

  /// 场景：数据帧不是合法 JSON。
  ///
  /// 预期：以 INVALID_RESPONSE 终结，不继续解析。
  #[tokio::test]
  async fn dispatch_invalid_json_terminates_invalid_response() {
    let (producer, mut stream) = ProviderEventStreamProducer::build();
    let mut producer = Some(producer);
    let mut aggregator = crate::test_support::aggregator("p1", "m1");
    producer
      .as_ref()
      .expect("alive")
      .push(ProviderEvent::Start {
        partial: aggregator.snapshot(),
      });

    let flow = dispatch_frame(
      &SseFrame {
        event: None,
        data: "{not json".to_string(),
      },
      &mut aggregator,
      &mut producer,
    );
    assert!(matches!(flow, LineFlow::Terminated));

    let events = collect_events(&mut stream).await;
    match events.last().expect("terminal event") {
      ProviderEvent::Error { code, .. } => {
        assert_eq!(*code, Some(INVALID_RESPONSE))
      }
      other => panic!("expected Error event, got {other:?}"),
    }
  }

  /// 场景：服务商在流内返回业务错误。
  ///
  /// 预期：映射为 QUOTA_EXCEEDED 并以 Error 终结。
  #[tokio::test]
  async fn dispatch_provider_error_maps_code() {
    let (producer, mut stream) = ProviderEventStreamProducer::build();
    let mut producer = Some(producer);
    let mut aggregator = crate::test_support::aggregator("p1", "m1");
    producer
      .as_ref()
      .expect("alive")
      .push(ProviderEvent::Start {
        partial: aggregator.snapshot(),
      });

    let flow = dispatch_frame(
      &SseFrame {
        event: None,
        data: r#"{"error":{"code":"insufficient_quota","message":"quota"}}"#
          .to_string(),
      },
      &mut aggregator,
      &mut producer,
    );
    assert!(matches!(flow, LineFlow::Terminated));

    let events = collect_events(&mut stream).await;
    match events.last().expect("terminal event") {
      ProviderEvent::Error { code, .. } => {
        assert_eq!(*code, Some(QUOTA_EXCEEDED))
      }
      other => panic!("expected Error event, got {other:?}"),
    }
  }

  /// 场景：响应元信息与 choice 内用量。
  ///
  /// 预期：`id` / `model` 写入快照，用量从 choice 内解析。
  #[tokio::test]
  async fn dispatch_meta_and_choice_usage() {
    let (producer, mut stream) = ProviderEventStreamProducer::build();
    let mut producer = Some(producer);
    let mut aggregator = crate::test_support::aggregator("p1", "m1");
    producer
      .as_ref()
      .expect("alive")
      .push(ProviderEvent::Start {
        partial: aggregator.snapshot(),
      });

    for data in [
      r#"{"id":"resp-1","model":"gpt-x","choices":[{"delta":{},"usage":{"prompt_tokens":2,"completion_tokens":3}}]}"#,
      "[DONE]",
    ] {
      dispatch_frame(
        &SseFrame {
          event: None,
          data: data.to_string(),
        },
        &mut aggregator,
        &mut producer,
      );
    }

    let events = collect_events(&mut stream).await;
    match events.last().expect("terminal event") {
      ProviderEvent::Done { message, .. } => {
        assert_eq!(message.response_id.as_deref(), Some("resp-1"));
        assert_eq!(message.response_model.as_deref(), Some("gpt-x"));
        assert_eq!(message.usage.input, 2);
        assert_eq!(message.usage.output, 3);
      }
      other => panic!("expected Done event, got {other:?}"),
    }
  }

  /// 场景：工具调用参数聚合结果不是合法 JSON。
  ///
  /// 预期：宽松兜底为空对象 `{}`，不判错。
  #[tokio::test]
  async fn finalize_falls_back_to_empty_args_for_invalid_json() {
    let (producer, mut stream) = ProviderEventStreamProducer::build();
    let mut producer = Some(producer);
    let mut aggregator = crate::test_support::aggregator("p1", "m1");
    producer
      .as_ref()
      .expect("alive")
      .push(ProviderEvent::Start {
        partial: aggregator.snapshot(),
      });

    for data in [
      r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"f","arguments":"not-json"}}]}}]}"#,
      "[DONE]",
    ] {
      dispatch_frame(
        &SseFrame {
          event: None,
          data: data.to_string(),
        },
        &mut aggregator,
        &mut producer,
      );
    }

    let events = collect_events(&mut stream).await;
    match events.last().expect("terminal event") {
      ProviderEvent::Done { message, .. } => {
        assert!(matches!(
          &message.content[0],
          Content::ToolCall(call) if call.arguments == serde_json::json!({})
        ));
      }
      other => panic!("expected Done event, got {other:?}"),
    }
  }

  /// 场景：消费端已放弃时 finalize 关闭文本块。
  ///
  /// 预期：`TextEnd` 推送失败后立即返回，不再构建终态。
  #[tokio::test]
  async fn finalize_stops_when_consumer_gone_for_text() {
    let (producer, consumer) = ProviderEventStreamProducer::build();
    let mut aggregator = crate::test_support::aggregator("p1", "m1");
    assert!(matches!(
      aggregator.text_delta(&producer, "x"),
      LineFlow::Continue
    ));
    drop(consumer);

    let mut producer = Some(producer);
    aggregator.mark_done_sentinel(); // R2: 模拟正常终结，非截断路径
    aggregator.finalize(&mut producer);
    assert!(producer.is_some(), "finalize 不应在推送失败后终结");
  }

  /// 场景：消费端已放弃时 finalize 关闭思考块。
  #[tokio::test]
  async fn finalize_stops_when_consumer_gone_for_reasoning() {
    let (producer, consumer) = ProviderEventStreamProducer::build();
    let mut aggregator = crate::test_support::aggregator("p1", "m1");
    assert!(matches!(
      aggregator.reasoning_delta(&producer, "t"),
      LineFlow::Continue
    ));
    drop(consumer);

    let mut producer = Some(producer);
    aggregator.mark_done_sentinel(); // R2: 模拟正常终结，非截断路径
    aggregator.finalize(&mut producer);
    assert!(producer.is_some());
  }

  /// 场景：消费端已放弃时 finalize 关闭工具块。
  #[tokio::test]
  async fn finalize_stops_when_consumer_gone_for_tool() {
    let (producer, consumer) = ProviderEventStreamProducer::build();
    let mut aggregator = crate::test_support::aggregator("p1", "m1");
    assert!(matches!(
      aggregator.tool_delta(&producer, 0, Some("c1"), Some("f"), Some("{}")),
      LineFlow::Continue
    ));
    drop(consumer);

    let mut producer = Some(producer);
    aggregator.mark_done_sentinel(); // R2: 模拟正常终结，非截断路径
    aggregator.finalize(&mut producer);
    assert!(producer.is_some());
  }

  /// 场景：解析器遇到无冒号行与 id/retry 等无关字段。
  ///
  /// 预期：无关行被忽略，只产出真正的 data 帧。
  #[test]
  fn parser_ignores_unknown_and_no_colon_lines() {
    let mut parser = SseParser::default();

    let frames = parser.feed(b"id: 1\nretry: 100\nping\n\ndata: x\n\n");

    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].data, "x");
  }

  /// 场景：错误映射的鉴权与关键词兜底分支。
  #[test]
  fn map_provider_error_covers_auth_and_keywords() {
    assert_eq!(
      map_provider_error(&serde_json::json!({
        "code": "authentication_error"
      })),
      Some(AUTH_FAILED)
    );
    assert_eq!(
      map_provider_error(&serde_json::json!({
        "message": "rate limit exceeded"
      })),
      Some(RATE_LIMITED)
    );
    assert_eq!(
      map_provider_error(&serde_json::json!({
        "message": "quota exceeded"
      })),
      Some(QUOTA_EXCEEDED)
    );
    assert_eq!(
      map_provider_error(&serde_json::json!({
        "message": "request timeout"
      })),
      Some(TIMEOUT)
    );
  }

  /// 场景：帧携带非标准 event 名，且终结后 producer 已为空。
  ///
  /// 预期：event 名仅记录不参与分派；producer 为空时后续帧直接
  /// 返回 Terminated，不再产生事件。
  #[tokio::test]
  async fn dispatch_ignores_event_name_and_missing_producer() {
    let (producer, mut stream) = ProviderEventStreamProducer::build();
    let mut producer = Some(producer);
    let mut aggregator = crate::test_support::aggregator("p1", "m1");

    let flow = dispatch_frame(
      &SseFrame {
        event: Some("message".to_string()),
        data: r#"{"choices":[{"delta":{"content":"x"}}]}"#.to_string(),
      },
      &mut aggregator,
      &mut producer,
    );
    assert!(matches!(flow, LineFlow::Continue));

    assert!(matches!(
      dispatch_frame(
        &SseFrame {
          event: None,
          data: "[DONE]".to_string(),
        },
        &mut aggregator,
        &mut producer,
      ),
      LineFlow::Terminated
    ));
    // producer 已被终结消费：再分派数据帧应在网关前直接返回
    assert!(matches!(
      dispatch_frame(
        &SseFrame {
          event: None,
          data: r#"{"choices":[{"delta":{"content":"y"}}]}"#.to_string(),
        },
        &mut aggregator,
        &mut producer,
      ),
      LineFlow::Terminated
    ));
    let _ = collect_events(&mut stream).await;
  }

  /// 场景：消费端已放弃时，三类增量分派均返回 ConsumerGone。
  #[tokio::test]
  async fn dispatch_returns_consumer_gone_for_all_delta_kinds() {
    let frames = [
      r#"{"choices":[{"delta":{"reasoning_content":"r"}}]}"#,
      r#"{"choices":[{"delta":{"content":"t"}}]}"#,
      r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"f","arguments":"{}"}}]}}]}"#,
    ];
    for data in frames {
      let (producer, consumer) = ProviderEventStreamProducer::build();
      drop(consumer);
      let mut producer = Some(producer);
      let mut aggregator = crate::test_support::aggregator("p1", "m1");

      let flow = dispatch_frame(
        &SseFrame {
          event: None,
          data: data.to_string(),
        },
        &mut aggregator,
        &mut producer,
      );
      assert!(matches!(flow, LineFlow::ConsumerGone));
    }
  }

  /// 场景：文本块已打开后消费端放弃，再次追加增量。
  ///
  /// 预期：`push_or_gone` 返回 ConsumerGone。
  #[test]
  fn text_delta_reports_consumer_gone_on_subsequent_push() {
    let (producer, consumer) = ProviderEventStreamProducer::build();
    let mut aggregator = crate::test_support::aggregator("p1", "m1");

    assert!(matches!(
      aggregator.text_delta(&producer, "a"),
      LineFlow::Continue
    ));
    drop(consumer);
    assert!(matches!(
      aggregator.text_delta(&producer, "b"),
      LineFlow::ConsumerGone
    ));
  }

  /// 场景：读取聚合器身份信息；producer 为空时 finalize 直接返回。
  #[test]
  fn aggregator_exposes_identity_and_finalizes_without_producer() {
    let aggregator = crate::test_support::aggregator("p1", "m1");
    assert_eq!(aggregator.provider().as_str(), "p1");
    assert_eq!(aggregator.model().as_str(), "m1");

    let mut aggregator = crate::test_support::aggregator("p1", "m1");
    let mut producer = None;
    aggregator.finalize(&mut producer);
    assert!(producer.is_none());
  }

  /// 场景：工具增量只带 id 与函数名、不带参数片段。
  ///
  /// 预期：登记块后返回 Continue，不产生 ToolCallDelta。
  #[test]
  fn tool_delta_without_args_continues() {
    let (producer, _stream) = ProviderEventStreamProducer::build();
    let mut aggregator = crate::test_support::aggregator("p1", "m1");

    let flow = aggregator.tool_delta(&producer, 0, Some("c1"), Some("f"), None);

    assert!(matches!(flow, LineFlow::Continue));
  }

  /// 场景：未知 finish_reason。
  ///
  /// 预期：原文写入 `raw_stop_reason`，映射结果保留为错误停止原因。
  #[tokio::test]
  async fn finalize_records_unknown_finish_reason() {
    let (producer, mut stream) = ProviderEventStreamProducer::build();
    let mut producer = Some(producer);
    let mut aggregator = crate::test_support::aggregator("p1", "m1");

    for data in [
      r#"{"choices":[{"delta":{},"finish_reason":"mystery"}]}"#,
      "[DONE]",
    ] {
      dispatch_frame(
        &SseFrame {
          event: None,
          data: data.to_string(),
        },
        &mut aggregator,
        &mut producer,
      );
    }

    let events = collect_events(&mut stream).await;
    match events.last().expect("terminal event") {
      ProviderEvent::Done { message, .. } => {
        assert_eq!(message.raw_stop_reason.as_deref(), Some("mystery"));
      }
      other => panic!("expected Done event, got {other:?}"),
    }
  }
}
