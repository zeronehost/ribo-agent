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

use std::{collections::BTreeMap, sync::Arc};

use ra_core::message::{
  AssistantMessage, Content, StopReason, TextContent, ThinkingContent,
  ToolCall, Usage,
};
use serde::Deserialize;

use crate::{
  error::{
    AUTH_FAILED, CANCELLED, CONTENT_FILTERED, INVALID_REQUEST,
    INVALID_RESPONSE, QUOTA_EXCEEDED, RATE_LIMITED, SERVER_ERROR, TIMEOUT,
    TRANSPORT,
  },
  ids::{ModelId, ProviderId},
  options::CancelToken,
  stream::{ProviderEvent, ProviderEventStreamProducer},
};

// ============================================================================
// 分派出口
// ============================================================================

/// 单帧处理后的解析去向。
enum LineFlow {
  /// 继续解析下一帧。
  Continue,
  /// 流已终结（`done` / `error` 已在本帧处理内调用，producer 已被消费）。
  Terminated,
  /// `push` 返回 `false`，消费端已放弃；停止解析且不再终结。
  ConsumerGone,
}

// ============================================================================
// SSE 传输层：字节 → 行 → 帧
// ============================================================================

/// 一帧完整的 SSE 数据：事件名（可选）+ data 载荷。
#[derive(Debug)]
struct SseFrame {
  /// `event:` 行内容；多数服务商不发，缺省 `None`。
  event: Option<String>,
  /// `data:` 载荷（多行 data 以 `\n` 连接为一帧，SSE 规范）。
  data: String,
}

/// SSE 传输层解析：字节流 → 行 → 帧。
///
/// 只负责 text/event-stream 的传输层惯例：行切分（`\r\n` / `\n`）、
/// 注释跳过（`:` 前缀）、字段提取、帧边界（空行）。`[DONE]` 哨兵与
/// JSON 解析属事件语义，归 [`dispatch_frame`] 处理。
#[derive(Debug, Default)]
struct SseParser {
  /// 未成行的字节缓冲。
  buf: Vec<u8>,
  /// 当前帧已收集的 data 载荷。
  data: String,
  /// 当前帧的事件名。
  event: Option<String>,
}

impl SseParser {
  /// 喂入新字节，返回随之完成的帧（空行触发，一次喂入可能完成多帧）。
  ///
  /// # 参数
  ///
  /// - `bytes`：本轮 `response.chunk()` 读到的原始字节。
  ///
  /// # 返回
  ///
  /// 本轮喂入期间完成的帧，按到达顺序排列。
  fn feed(&mut self, bytes: &[u8]) -> Vec<SseFrame> {
    // ===== 第一步：并入缓冲，逐行消费 =====
    self.buf.extend_from_slice(bytes);
    let mut frames = Vec::new();
    // 以 '\n' 为行界反复取行；'\r\n' 的 '\r' 在行处理时剥离
    while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
      // drain(..=pos) 连 '\n' 一并取出，剩余字节自动前移
      let line_bytes: Vec<u8> = self.buf.drain(..=pos).collect();
      // 行内必为完整 UTF-8（续字节不含 '\n'）；lossy 仅防提供方发送非法序列
      let line = String::from_utf8_lossy(&line_bytes[..pos]);
      self.push_line(line.trim_end_matches('\r'), &mut frames);
    }
    frames
  }

  /// 流结束时冲刷残余字节成帧；部分服务商末帧不带空行（宽松解析）。
  ///
  /// # 返回
  ///
  /// 冲刷产生的帧（可能为空：流以空行正常收尾时无残余）。
  fn finish(&mut self) -> Vec<SseFrame> {
    let mut frames = Vec::new();

    // ===== 第一步：残余未换行字节补作最后一行 =====
    if !self.buf.is_empty() {
      // mem::take 取出缓冲：Cow 借用 bytes 局部变量，避免与 push_line
      // 的可变借用（&mut self）冲突
      let bytes = std::mem::take(&mut self.buf);
      let line = String::from_utf8_lossy(&bytes);
      self.push_line(line.trim_end_matches('\r'), &mut frames);
    }

    // ===== 第二步：data 非空则无条件成帧 =====
    // 末帧无空行触发的宽松兜底
    if !self.data.is_empty() {
      frames.push(SseFrame {
        event: self.event.take(),
        data: std::mem::take(&mut self.data),
      });
    }
    frames
  }

  /// 处理单行：帧边界 / 注释 / 字段提取（`feed` 与 `finish` 共用）。
  fn push_line(&mut self, line: &str, frames: &mut Vec<SseFrame>) {
    if line.is_empty() {
      // ===== 帧边界：仅当收集到 data 才成帧 =====
      // 纯 event 无 data 的帧对补全协议无意义（dispatch 只认 JSON 载荷），
      // 直接丢弃——否则心跳帧（连续空行、": ping" 后的空行）会被
      // 误判为 INVALID_RESPONSE 终结整条流。
      if !self.data.is_empty() {
        frames.push(SseFrame {
          event: self.event.take(),
          data: std::mem::take(&mut self.data),
        });
      }
      // 无 data 的 event 一并复位，避免泄漏到下一帧
      self.event = None;
    } else if line.starts_with(':') {
      // 注释行：SSE 规范忽略（服务商心跳 keep-alive 常用 ": ping"）
    } else {
      // 字段提取：冒号切分 + value 剥离单个前导空格（SSE 规范）
      let (field, value) = match line.split_once(':') {
        Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
        // 只有字段名没有冒号：规范规定视作空值
        None => (line, ""),
      };
      match field {
        "data" => {
          // 多行 data 以 '\n' 连接为单帧载荷（SSE 规范）
          if !self.data.is_empty() {
            self.data.push('\n');
          }
          self.data.push_str(value);
        }
        "event" => {
          // 规范语义为覆盖；重复 event 行属提供方缺陷，取最新值（宽松）
          self.event = Some(value.to_string());
        }
        // id: / retry: 及未知字段与帧语义无关，一律忽略
        _ => {}
      }
    }
  }
}

// ============================================================================
// 线格式（只声明读取所需字段，其余由 serde 忽略）
// ============================================================================

/// 一帧增量载荷。
#[derive(Debug, Default, Deserialize)]
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
#[derive(Debug, Default, Deserialize)]
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
#[derive(Debug, Default, Deserialize)]
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
#[derive(Debug, Default, Deserialize)]
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
#[derive(Debug, Default, Deserialize)]
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
// 错误映射
// ============================================================================

/// 提供方私有错误体 → `&'static str` 错误码（始终以
/// `producer.error(StopReason::Error, .., Some(code))` 终结流）。
///
/// 映射优先级：**显式 `code` / `type` 字段 > `message` 关键词**；
/// 返回 `None` 表示无法分类，`Error` 事件的 `code` 落为 `None`
/// （事件模型允许），消费端按未分类流内错误处置。
///
/// 内容安全拦截必须映射为 [`CONTENT_FILTERED`]，不得归入
/// [`TRANSPORT`] 类可重试故障——两者处置方式相反（改写输入 vs 重试）。
fn map_provider_error(body: &serde_json::Value) -> Option<&'static str> {
  // ===== 第一级：显式字段精确匹配 =====
  // `code` 与 `type` 各提供方取值不一，这里维护并集表；
  // 未识别的显式码不猜测类别，直接落入关键词兜底。
  let explicit = body
    .get("code")
    .and_then(serde_json::Value::as_str)
    .or_else(|| body.get("type").and_then(serde_json::Value::as_str));
  if let Some(raw) = explicit {
    let code = match raw {
      // 请求形态错误（含上下文超长——重试无法恢复）
      "invalid_request_error" | "context_length_exceeded" => INVALID_REQUEST,
      // 鉴权类
      "authentication_error" | "invalid_api_key" | "permission_error" => {
        AUTH_FAILED
      }
      // 配额类
      "insufficient_quota" => QUOTA_EXCEEDED,
      // 限流类
      "rate_limit_error" | "rate_limit_exceeded" => RATE_LIMITED,
      // 服务端类
      "api_error" | "server_error" | "overloaded_error" => SERVER_ERROR,
      // 超时类
      "timeout" | "timeout_error" => TIMEOUT,
      // 内容安全类
      "content_filter" | "content_management_policy" => CONTENT_FILTERED,
      // 未识别：不映射，交给关键词兜底
      _ => return map_provider_error_by_message(body),
    };
    return Some(code);
  }

  // ===== 第二级：message 关键词兜底 =====
  map_provider_error_by_message(body)
}

/// `message` 关键词兜底映射（显式字段缺失或未识别时调用）。
///
/// 命中顺序即优先级：内容安全 > 限流 > 配额 > 鉴权 > 超时——
/// 内容安全最优先，防止被误判为可重试故障后重放被拦截的输入。
/// 匹配在 `message` 小写化后做子串查找；除本函数外任何逻辑
/// 不得匹配错误文本内容（P2：结构化优先于字符串）。
fn map_provider_error_by_message(
  body: &serde_json::Value,
) -> Option<&'static str> {
  let message = body.get("message").and_then(serde_json::Value::as_str)?;
  let lowered = message.to_lowercase();

  if lowered.contains("content filter")
    || lowered.contains("content_filter")
    || lowered.contains("content management policy")
    || lowered.contains("safety system")
  {
    return Some(CONTENT_FILTERED);
  }
  if lowered.contains("rate limit") {
    return Some(RATE_LIMITED);
  }
  if lowered.contains("quota") {
    return Some(QUOTA_EXCEEDED);
  }
  if lowered.contains("api key") || lowered.contains("unauthorized") {
    return Some(AUTH_FAILED);
  }
  if lowered.contains("timed out") || lowered.contains("timeout") {
    return Some(TIMEOUT);
  }
  None
}

/// `finish_reason` → `StopReason` 的权威映射。
///
/// 返回 `(StopReason, 保留原文 Option)`：`Some(raw)` 表示原文无法归入
/// 标准变体，调用方必须写入 `AssistantMessage.raw_stop_reason`。
fn map_stop_reason(raw: Option<&str>) -> (StopReason, Option<String>) {
  match raw {
    None | Some("stop" | "end" | "end_turn") => (StopReason::Stop, None),
    Some("length" | "max_tokens") => (StopReason::Length, None),
    Some("tool_calls" | "tool_use" | "function_call") => {
      (StopReason::ToolUse, None)
    }
    // 无法识别：保留原文，取最近合理值 Stop，不判错
    Some(other) => (StopReason::Stop, Some(other.to_string())),
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
  frame: SseFrame,
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
      return fail_invalid_response(
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
        return fail_invalid_response(
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

/// 以 `INVALID_RESPONSE` 终结流：JSON 非法 / 必需结构缺失的唯一出口。
///
/// producer 已被消费，调用方收到 [`LineFlow::Terminated`] 后停止解析。
fn fail_invalid_response(
  producer: &mut Option<ProviderEventStreamProducer>,
  aggregator: &StreamAggregator,
  reason: impl std::fmt::Display,
) -> LineFlow {
  tracing::warn!(
    provider = aggregator.provider().as_str(),
    reason = %reason,
    "invalid provider response, terminating stream"
  );
  let message = aggregator.terminal_error(StopReason::Error, reason);
  if let Some(p) = producer.take() {
    p.error(StopReason::Error, message, Some(INVALID_RESPONSE));
  }
  LineFlow::Terminated
}

// ============================================================================
// 事件聚合
// ============================================================================

/// 单个工具调用的聚合状态（以线格式 `tool_calls[i].index` 区分并行调用）。
struct ToolState {
  /// 对应 `AssistantMessage.content` 中的块下标。
  content_index: usize,
  /// 已聚合的调用 id（可能分段到达）。
  id: String,
  /// 已聚合的函数名（首帧携带）。
  name: String,
  /// 已聚合的参数 JSON 片段（可能不完整）。
  arguments: String,
}

/// 解析任务内的聚合器。
///
/// 消费端从不拼接增量——每个事件的 `partial` 都是本结构在 push 前
/// 生成的快照（`Arc` 共享，克隆廉价）；聚合状态只存在于本结构内。
struct StreamAggregator {
  /// 提供方标识（日志字段）。
  provider: ProviderId,
  /// 模型标识（日志字段）。
  model: ModelId,
  /// 聚合中的消息：增量追加到 `content`，usage / 停止原因就地更新。
  output: AssistantMessage,
  /// 当前打开的文本块在 `content` 中的下标；`None` 表示未打开。
  content_index: Option<usize>,
  /// 当前打开的推理块下标；`None` 表示未打开。
  reasoning_index: Option<usize>,
  /// 线格式 `tool_calls` 下标 → 工具调用聚合状态。
  tools: BTreeMap<usize, ToolState>,
  /// 服务商原始 finish_reason（原样保留，含无法识别的值）。
  raw_finish_reason: Option<String>,
  /// 是否已完成终结（防 `finalize` 重入）。
  finished: bool,
}

impl StreamAggregator {
  /// 起始状态：空消息骨架（stop_reason 为占位，finalize 时覆盖）。
  fn new(provider: ProviderId, model: ModelId) -> Self {
    Self {
      output: AssistantMessage::empty(
        provider.as_str(),
        model.as_str(),
        ra_core::utils::time::now_ms(),
      ),
      provider,
      model,
      content_index: None,
      reasoning_index: None,
      tools: BTreeMap::new(),
      raw_finish_reason: None,
      finished: false,
    }
  }

  /// 当前聚合状态的共享快照。
  fn snapshot(&self) -> Arc<AssistantMessage> {
    Arc::new(self.output.clone())
  }

  /// 只读访问：日志字段用。
  fn provider(&self) -> &ProviderId {
    &self.provider
  }

  /// 只读访问：日志字段用。
  fn model(&self) -> &ModelId {
    &self.model
  }

  /// 元信息并入快照（宽松：缺失即跳过）。
  fn set_meta(
    &mut self,
    response_id: Option<String>,
    response_model: Option<String>,
  ) {
    if let Some(id) = response_id {
      self.output.response_id = Some(id);
    }
    if let Some(name) = response_model {
      self.output.response_model = Some(name);
    }
  }

  /// 并入用量（写入聚合消息的 usage 字段，不单独发事件）。
  fn set_usage(&mut self, usage: Usage) {
    self.output.usage = usage;
  }

  /// 记录 finish_reason 原文（映射推迟到 finalize）。
  fn set_finish_reason(&mut self, raw: &str) {
    self.raw_finish_reason = Some(raw.to_string());
  }

  /// 指定线格式下标是否已登记工具调用块。
  fn has_tool_block(&self, wire_index: usize) -> bool {
    self.tools.contains_key(&wire_index)
  }

  /// push 包装：消费端关闭时折叠为 [`LineFlow::ConsumerGone`]。
  fn push_or_gone(
    &mut self,
    producer: &ProviderEventStreamProducer,
    event: ProviderEvent,
  ) -> LineFlow {
    if producer.push(event) {
      LineFlow::Continue
    } else {
      LineFlow::ConsumerGone
    }
  }

  /// 文本增量：块未打开时先补 `TextStart`（块纪律自愈，不判错），
  /// 追加落盘后推 `TextDelta`——**先落盘再生成快照**，
  /// 保证 partial 包含本增量。
  fn text_delta(
    &mut self,
    producer: &ProviderEventStreamProducer,
    delta: &str,
  ) -> LineFlow {
    let index = match self.content_index {
      Some(index) => index,
      None => {
        // ===== 块纪律自愈：首个文本增量自动开块 =====
        let index = self.output.content.len();
        self.output.content.push(Content::Text(TextContent {
          content: String::new(),
          signature: None,
        }));
        self.content_index = Some(index);
        if !producer.push(ProviderEvent::TextStart {
          index,
          partial: self.snapshot(),
        }) {
          return LineFlow::ConsumerGone;
        }
        index
      }
    };
    if let Some(Content::Text(text)) = self.output.content.get_mut(index) {
      text.content.push_str(delta);
    }
    self.push_or_gone(
      producer,
      ProviderEvent::TextDelta {
        index,
        delta: delta.to_string(),
        partial: self.snapshot(),
      },
    )
  }

  /// 推理增量：与 [`StreamAggregator::text_delta`] 同构
  /// （对应 `ReasoningStart` / `ReasoningDelta`，落盘到 `ThinkingContent`）。
  fn reasoning_delta(
    &mut self,
    producer: &ProviderEventStreamProducer,
    delta: &str,
  ) -> LineFlow {
    let index = match self.reasoning_index {
      Some(index) => index,
      None => {
        // ===== 块纪律自愈：首个推理增量自动开块 =====
        let index = self.output.content.len();
        self.output.content.push(Content::Thinking(ThinkingContent {
          content: String::new(),
          signature: None,
          redacted: false,
        }));
        self.reasoning_index = Some(index);
        if !producer.push(ProviderEvent::ReasoningStart {
          index,
          partial: self.snapshot(),
        }) {
          return LineFlow::ConsumerGone;
        }
        index
      }
    };
    if let Some(Content::Thinking(thinking)) =
      self.output.content.get_mut(index)
    {
      thinking.content.push_str(delta);
    }
    self.push_or_gone(
      producer,
      ProviderEvent::ReasoningDelta {
        index,
        delta: delta.to_string(),
        partial: self.snapshot(),
      },
    )
  }

  /// 工具调用增量：线格式下标首次出现时开 ToolCall 块（推 `ToolCallStart`）
  /// 并登记聚合状态；id / 函数名 / 参数片段分别累积，占位 ToolCall 的
  /// id / name 同步更新（partial 快照可见，便于消费端关联）。
  fn tool_delta(
    &mut self,
    producer: &ProviderEventStreamProducer,
    wire_index: usize,
    id: Option<&str>,
    name: Option<&str>,
    args_fragment: Option<&str>,
  ) -> LineFlow {
    // ===== 块登记：线格式下标首次出现 → 开 ToolCall 块并推 Start =====
    if !self.tools.contains_key(&wire_index) {
      let content_index = self.output.content.len();
      self.output.content.push(Content::ToolCall(ToolCall {
        id: String::new(),
        name: String::new(),
        arguments: serde_json::json!({}), // 参数占位，finalize 时回填
        signature: None,
        namespace: None,
      }));
      self.tools.insert(
        wire_index,
        ToolState {
          content_index,
          id: String::new(),
          name: String::new(),
          arguments: String::new(),
        },
      );
      if !producer.push(ProviderEvent::ToolCallStart {
        index: content_index,
        partial: self.snapshot(),
      }) {
        return LineFlow::ConsumerGone;
      }
    }

    // ===== 片段累积（id / 函数名 / 参数可能分段到达）=====
    let content_index = self.tools[&wire_index].content_index;
    {
      let state = self.tools.get_mut(&wire_index).expect("registered above");
      if let Some(id) = id {
        state.id.push_str(id);
      }
      if let Some(name) = name {
        state.name.push_str(name);
      }
      if let Some(args) = args_fragment {
        state.arguments.push_str(args);
      }
    }
    // ===== 同步占位 ToolCall 的 id / 函数名（partial 快照可见）=====
    if let Some(Content::ToolCall(call)) =
      self.output.content.get_mut(content_index)
    {
      let state = &self.tools[&wire_index];
      call.id = state.id.clone();
      call.name = state.name.clone();
    }
    // ===== 参数片段事件（空片段跳过）=====
    if let Some(args) = args_fragment.filter(|s| !s.is_empty()) {
      return self.push_or_gone(
        producer,
        ProviderEvent::ToolCallDelta {
          index: content_index,
          delta: args.to_string(),
          partial: self.snapshot(),
        },
      );
    }
    LineFlow::Continue
  }

  /// 异常终结消息：保留已聚合内容与用量，填入 error_message 与指定停止原因。
  ///
  /// 供取消（`Aborted`）与各类流内错误（`Error`）路径构造终结载荷；
  /// 上层仍可凭已聚合部分做展示与记账。
  fn terminal_error(
    &self,
    stop_reason: StopReason,
    reason: impl std::fmt::Display,
  ) -> AssistantMessage {
    let mut message = self.output.clone();
    message.stop_reason = stop_reason;
    message.error_message = Some(reason.to_string());
    message
  }

  /// 流正常结束：关闭未闭合块（推各 `End` 事件）→ 映射停止原因 → `done` 终结。
  ///
  /// # 幂等性
  ///
  /// `[DONE]` 帧与流 EOF 两处收敛到本方法；
  /// `finished` 标志保证只有第一次调用生效，`producer` 为 `None`
  /// （消费端已放弃）时安全退出，不再产生任何事件。
  fn finalize(&mut self, producer: &mut Option<ProviderEventStreamProducer>) {
    if self.finished {
      return; // 已终结：另一收敛点的 no-op
    }
    self.finished = true;

    // ===== 阶段 1：关闭未闭合的内容块（块纪律收尾）=====
    let Some(p) = producer.as_ref() else {
      return; // 消费端已放弃：不再产生任何事件
    };
    if let Some(index) = self.content_index.take() {
      let content = match &self.output.content[index] {
        Content::Text(text) => text.content.clone(),
        _ => String::new(),
      };
      if !p.push(ProviderEvent::TextEnd {
        index,
        content,
        partial: self.snapshot(),
      }) {
        return; // push false：消费端已放弃（合法退出路径，不再终结）
      }
    }
    if let Some(index) = self.reasoning_index.take() {
      let content = match &self.output.content[index] {
        Content::Thinking(thinking) => thinking.content.clone(),
        _ => String::new(),
      };
      if !p.push(ProviderEvent::ReasoningEnd {
        index,
        content,
        partial: self.snapshot(),
      }) {
        return;
      }
    }
    for (_, state) in std::mem::take(&mut self.tools) {
      // 解构取所有权：content_index 需在 id / name 被移动后使用
      let ToolState {
        content_index,
        id,
        name,
        arguments,
      } = state;
      // 参数聚合结果非法 JSON 时按宽松解析兜底为空对象，不判错
      let arguments = serde_json::from_str(&arguments).unwrap_or_else(|err| {
        tracing::warn!(
          tool = %name,
          error = %err,
          "tool call arguments is not valid json, fallback to empty object"
        );
        serde_json::json!({})
      });
      let tool_call = ToolCall {
        id,
        name,
        arguments,
        signature: None,
        namespace: None,
      };
      if !p.push(ProviderEvent::ToolCallEnd {
        index: content_index,
        tool_call,
        partial: self.snapshot(),
      }) {
        return;
      }
    }

    // ===== 阶段 2：映射停止原因并产出最终消息 =====
    let (reason, raw) = map_stop_reason(self.raw_finish_reason.as_deref());
    if let Some(raw) = raw {
      self.output.raw_stop_reason = Some(raw);
    }
    // StopReason 不实现 Copy：落盘用克隆，原值留给 done 终结
    self.output.stop_reason = reason.clone();
    let message = self.output.clone();
    if let Some(p) = producer.take() {
      p.done(reason, message);
    }
  }
}

// ============================================================================
// 解析任务
// ============================================================================

/// SSE 解析任务（由适配器 `stream` 建流成功后调用）。
///
/// # 生产者义务落点
///
/// - `Start` 在建通道后立即推送（第 1 条）；
/// - 每个内容事件携带最新 `partial` 快照（第 2 条，由聚合器保证）；
/// - 每条结束路径恰好终结一次（第 4 条，由 `Option` take 纪律保证）；
/// - `push` 返回 `false` 立即停止解析并释放连接（第 5 条）。
pub(crate) fn spawn_parse_task(
  mut response: reqwest::Response,
  producer: ProviderEventStreamProducer,
  cancel: CancelToken,
  provider: ProviderId,
  model: ModelId,
) {
  tokio::spawn(async move {
    // ===== 初始化阶段 =====
    // 终结能力用 Option 持有：done / error 消费 self，
    // 每条结束路径必须 take 后调用，保证"恰好终结一次"。
    let mut producer = Some(producer);
    let mut aggregator = StreamAggregator::new(provider, model);
    // 消费端放弃标志：区分合法退出与逻辑缺陷（收尾兜底用）。
    let mut consumer_gone = false;

    // 生产者义务第 1 条：Start 必是第一个事件，建通道后立即推送。
    let started = ProviderEvent::Start {
      partial: aggregator.snapshot(),
    };
    if !producer.as_ref().is_some_and(|p| p.push(started)) {
      // 消费端在建流返回前已 drop 流：oneshot 接收端已关闭，
      // producer drop 不会让任何消费者看到 STREAM_INTERRUPTED——合法路径。
      return;
    }

    // ===== 主循环：SSE 字节流 vs 取消 =====
    // 流式阶段不设总时长超时：首字迟滞归 TTFT 守卫、事件停滞归网关看门狗；
    // 适配器只处理网络错误与取消。
    let mut parser = SseParser::default();
    loop {
      // biased：取消分支优先轮询，尽快终结已取消的流。
      tokio::select! {
        biased;
        _ = cancel.cancelled() => {
          tracing::info!(
            provider = aggregator.provider().as_str(),
            model = aggregator.model().as_str(),
            "stream aborted by cancellation"
          );
          // 流式取消以 Aborted 终结，不得伪装成传输错误。
          let msg = aggregator
            .terminal_error(StopReason::Aborted, "cancelled while streaming");
          if let Some(p) = producer.take() {
            p.error(StopReason::Aborted, msg, Some(CANCELLED));
          }
          return; // response 就地 drop → 底层连接释放
        }

        chunk = response.chunk() => match chunk {
          // ===== 流正常结束：冲刷残余帧（部分服务商末帧不带空行）后聚合 =====
          Ok(None) => {
            let mut terminated_early = false;
            for frame in parser.finish() {
              match dispatch_frame(frame, &mut aggregator, &mut producer) {
                LineFlow::Continue => {}
                LineFlow::Terminated => { terminated_early = true; break; }
                LineFlow::ConsumerGone => { consumer_gone = true; break; }
              }
            }
            // finalize 幂等：[DONE] 帧已终结时本调用收敛为 no-op；
            // 消费端已放弃时同样跳过（不再产生任何事件）。
            if !terminated_early && !consumer_gone {
              aggregator.finalize(&mut producer);
            }
            break;
          }

          // ===== 正常数据：切帧并逐帧驱动状态机 =====
          Ok(Some(bytes)) => {
            'pump: for frame in parser.feed(&bytes) {
              match dispatch_frame(frame, &mut aggregator, &mut producer) {
                LineFlow::Continue => {}
                LineFlow::Terminated => break 'pump,
                LineFlow::ConsumerGone => { consumer_gone = true; break 'pump; }
              }
            }
            if consumer_gone {
              // 停止解析；response 随任务结束 drop，底层连接释放。
              break;
            }
          }

          // ===== 网络中断 / 读失败：以 TRANSPORT 终结 =====
          Err(err) => {
            tracing::warn!(
              provider = aggregator.provider().as_str(),
              model = aggregator.model().as_str(),
              error = %err,
              "stream read failed"
            );
            let msg = aggregator
              .terminal_error(StopReason::Error, format!("stream read failed: {err}"));
            if let Some(p) = producer.take() {
              p.error(StopReason::Error, msg, Some(TRANSPORT));
            }
            break;
          }
        }
      }
    }

    // ===== 收尾兜底（本节核心不变量）=====
    match producer {
      // 正常路径：done / error 已消费终结能力，此处为空是常态。
      None => {}
      // push false 退出：消费端已不存在，STREAM_INTERRUPTED 无人接收，合法。
      Some(_) if consumer_gone => {}
      // 逻辑缺陷：既未终结、消费端也未放弃——出现即视为缺陷，
      // 记 error 供测试与运维捕获；drop 后消费端 result() 收到
      // Err(STREAM_INTERRUPTED) 兜底，保证永不永久挂起。
      Some(p) => {
        tracing::error!(
          provider = aggregator.provider().as_str(),
          "parse task exited without terminal event"
        );
        drop(p);
      }
    }
  });
}

#[cfg(test)]
mod tests {
  use super::*;

  // ===== 测试辅助构造 =====

  // 构造聚合器（provider / model 取自固定测试标识）。
  fn test_aggregator() -> StreamAggregator {
    crate::test_support::init();
    StreamAggregator::new(
      ProviderId::parse("p1").expect("valid provider id"),
      ModelId::parse("m1").expect("valid model id"),
    )
  }

  // 拉取全部事件直到流结束（终结事件被读入 events 后 next 返回 None）。
  async fn collect_events(
    stream: &mut crate::stream::ProviderEventStream,
  ) -> Vec<ProviderEvent> {
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
      events.push(event);
    }
    events
  }

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
    let mut aggregator = test_aggregator();

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
    let mut aggregator = test_aggregator();
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
    let flow = dispatch_frame(frame, &mut aggregator, &mut producer);
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
    let mut aggregator = test_aggregator();
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
        SseFrame {
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
    let mut aggregator = test_aggregator();
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
        SseFrame {
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
    let mut aggregator = test_aggregator();
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
        SseFrame {
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
    let mut aggregator = test_aggregator();
    producer
      .as_ref()
      .expect("alive")
      .push(ProviderEvent::Start {
        partial: aggregator.snapshot(),
      });

    let flow = dispatch_frame(
      SseFrame {
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
    let mut aggregator = test_aggregator();
    producer
      .as_ref()
      .expect("alive")
      .push(ProviderEvent::Start {
        partial: aggregator.snapshot(),
      });

    let flow = dispatch_frame(
      SseFrame {
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
    let mut aggregator = test_aggregator();
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
        SseFrame {
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
    let mut aggregator = test_aggregator();
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
        SseFrame {
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
    let mut aggregator = test_aggregator();
    assert!(matches!(
      aggregator.text_delta(&producer, "x"),
      LineFlow::Continue
    ));
    drop(consumer);

    let mut producer = Some(producer);
    aggregator.finalize(&mut producer);
    assert!(producer.is_some(), "finalize 不应在推送失败后终结");
  }

  /// 场景：消费端已放弃时 finalize 关闭思考块。
  #[tokio::test]
  async fn finalize_stops_when_consumer_gone_for_reasoning() {
    let (producer, consumer) = ProviderEventStreamProducer::build();
    let mut aggregator = test_aggregator();
    assert!(matches!(
      aggregator.reasoning_delta(&producer, "t"),
      LineFlow::Continue
    ));
    drop(consumer);

    let mut producer = Some(producer);
    aggregator.finalize(&mut producer);
    assert!(producer.is_some());
  }

  /// 场景：消费端已放弃时 finalize 关闭工具块。
  #[tokio::test]
  async fn finalize_stops_when_consumer_gone_for_tool() {
    let (producer, consumer) = ProviderEventStreamProducer::build();
    let mut aggregator = test_aggregator();
    assert!(matches!(
      aggregator.tool_delta(&producer, 0, Some("c1"), Some("f"), Some("{}")),
      LineFlow::Continue
    ));
    drop(consumer);

    let mut producer = Some(producer);
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
    let mut aggregator = test_aggregator();

    let flow = dispatch_frame(
      SseFrame {
        event: Some("message".to_string()),
        data: r#"{"choices":[{"delta":{"content":"x"}}]}"#.to_string(),
      },
      &mut aggregator,
      &mut producer,
    );
    assert!(matches!(flow, LineFlow::Continue));

    assert!(matches!(
      dispatch_frame(
        SseFrame {
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
        SseFrame {
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
      let mut aggregator = test_aggregator();

      let flow = dispatch_frame(
        SseFrame {
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
    let mut aggregator = test_aggregator();

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
    let aggregator = test_aggregator();
    assert_eq!(aggregator.provider().as_str(), "p1");
    assert_eq!(aggregator.model().as_str(), "m1");

    let mut aggregator = test_aggregator();
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
    let mut aggregator = test_aggregator();

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
    let mut aggregator = test_aggregator();

    for data in [
      r#"{"choices":[{"delta":{},"finish_reason":"mystery"}]}"#,
      "[DONE]",
    ] {
      dispatch_frame(
        SseFrame {
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
