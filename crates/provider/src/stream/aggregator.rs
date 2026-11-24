//! 跨适配器复用的流式聚合器：维护半成品快照并生成 partial。
//!
//! Anthropic / OpenAI 适配器各建实例，内部结构完全同构；
//! 变更 StreamAggregator 实现时**两个适配器同时受益**，不再复制粘贴。
//!
//! 本模块同时承载 `map_stop_reason`（finish_reason → StopReason 权威映射），
//! 因为它是聚合器 finalize 阶段的内部依赖。

use std::{collections::BTreeMap, sync::Arc};

use ra_core::message::{
  AssistantMessage, Content, StopReason, TextContent, ThinkingContent,
  ToolCall, Usage,
};

use crate::{
  error::STREAM_INTERRUPTED,
  ids::{ModelId, ProviderId},
  stream::{ProviderEvent, ProviderEventStreamProducer},
};

/// 单个工具调用的聚合状态（以线格式 `tool_calls[i].index` 区分并行调用）。
#[derive(Debug, Clone)]
pub(crate) struct ToolState {
  /// 对应 `AssistantMessage.content` 中的块下标。
  pub(crate) content_index: usize,
  /// 已聚合的调用 id（可能分段到达）。
  pub(crate) id: String,
  /// 已聚合的函数名（首帧携带）。
  pub(crate) name: String,
  /// 已聚合的参数 JSON 片段（可能不完整）。
  pub(crate) arguments: String,
}

/// 解析任务内的聚合器。
///
/// 消费端从不拼接增量——每个事件的 `partial` 都是本结构在 push 前
/// 生成的快照（`Arc` 共享，克隆廉价）；聚合状态只存在于本结构内。
///
/// # R2 截断修复
///
/// 新增 `had_done_sentinel: bool` 标志。[DONE] 帧到达时置 true。
/// finalize 检查：EOF 终结但无 [DONE] 哨兵 → 截断 → Error + STREAM_INTERRUPTED。
pub(crate) struct StreamAggregator {
  /// 提供方标识（日志字段）。
  pub(crate) provider: ProviderId,
  /// 模型标识（日志字段）。
  pub(crate) model: ModelId,
  /// 聚合中的消息：增量追加到 `content`，usage / 停止原因就地更新。
  pub(crate) output: AssistantMessage,
  /// 当前打开的文本块在 `content` 中的下标；`None` 表示未打开。
  pub(crate) content_index: Option<usize>,
  /// 当前打开的推理块下标；`None` 表示未打开。
  pub(crate) reasoning_index: Option<usize>,
  /// 线格式 `tool_calls` 下标 → 工具调用聚合状态。
  pub(crate) tools: BTreeMap<usize, ToolState>,
  /// 服务商原始 finish_reason（原样保留，含无法识别的值）。
  pub(crate) raw_finish_reason: Option<String>,
  /// 是否已完成终结（防 `finalize` 重入）。
  pub(crate) finished: bool,
  /// [DONE] 哨兵是否到达过——区分「正常 [DONE] 终结」与「EOF 提前断开截断」。
  pub(crate) had_done_sentinel: bool,
}

/// `finish_reason` → `StopReason` 的权威映射。
///
/// 返回 `(StopReason, 保留原文 Option)`：`Some(raw)` 表示原文无法归入
/// 标准变体，调用方必须写入 `AssistantMessage.raw_stop_reason`。
pub(crate) fn map_stop_reason(
  raw: Option<&str>,
) -> (StopReason, Option<String>) {
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

impl StreamAggregator {
  /// 起始状态：空消息骨架（stop_reason 为占位，finalize 时覆盖）。
  pub(crate) fn new(provider: ProviderId, model: ModelId) -> Self {
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
      had_done_sentinel: false,
    }
  }

  /// 标记 [DONE] 哨兵已到达——R2 截断检测的关键标志。
  pub(crate) fn mark_done_sentinel(&mut self) {
    self.had_done_sentinel = true;
  }

  /// 当前聚合状态的共享快照。
  pub(crate) fn snapshot(&self) -> Arc<AssistantMessage> {
    Arc::new(self.output.clone())
  }

  /// 只读访问：日志字段用。
  pub(crate) fn provider(&self) -> &ProviderId {
    &self.provider
  }

  /// 只读访问：日志字段用。
  pub(crate) fn model(&self) -> &ModelId {
    &self.model
  }

  /// 元信息并入快照（宽松：缺失即跳过）。
  pub(crate) fn set_meta(
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
  pub(crate) fn set_usage(&mut self, usage: Usage) {
    self.output.usage = usage;
  }

  /// 记录 finish_reason 原文（映射推迟到 finalize）。
  pub(crate) fn set_finish_reason(&mut self, raw: &str) {
    self.raw_finish_reason = Some(raw.to_string());
  }

  /// 指定线格式下标是否已登记工具调用块。
  pub(crate) fn has_tool_block(&self, wire_index: usize) -> bool {
    self.tools.contains_key(&wire_index)
  }

  /// push 包装：消费端关闭时折叠为 `LineFlow::ConsumerGone`。
  pub(crate) fn push_or_gone(
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
  pub(crate) fn text_delta(
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
  pub(crate) fn reasoning_delta(
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
  pub(crate) fn tool_delta(
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
  pub(crate) fn terminal_error(
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
  ///
  /// # R2 截断检测
  ///
  /// EOF 提前断开且无 [DONE] 哨兵（had_done_sentinel == false）时，
  /// 直接以 Error + STREAM_INTERRUPTED 终结，不做块纪律收尾。
  pub(crate) fn finalize(
    &mut self,
    producer: &mut Option<ProviderEventStreamProducer>,
  ) {
    if self.finished {
      return; // 已终结：另一收敛点的 no-op
    }
    self.finished = true;

    // ===== R2 截断检测：EOF 提前断开且无 [DONE] 哨兵 =====
    // [DONE] 路径已在 dispatch_frame 里调 mark_done_sentinel + finalize，
    // 此处 finalize 再进入就是 finished 返回；本路径进入且 had_done_sentinel=false
    // 说明 EOF 提前断开（spawn_parse_task 的 Ok(None) 路径），没有收到 [DONE]。
    if !self.had_done_sentinel {
      tracing::warn!(
        provider = self.provider.as_str(),
        model = self.model.as_str(),
        "stream truncated before [DONE] sentinel"
      );
      let msg = self.terminal_error(
        StopReason::Error,
        "stream truncated before [DONE] sentinel",
      );
      if let Some(p) = producer.take() {
        p.error(StopReason::Error, msg, Some(STREAM_INTERRUPTED));
      }
      return;
    }

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

      // ===== R1 修复：同步回写 output.content =====
      // 重建后的完整 ToolCall 必须同时写入 output.content，
      // 保证 done 消息的 content 与 ToolCallEnd 事件携带的 arguments 一致。
      if let Some(Content::ToolCall(existing)) =
        self.output.content.get_mut(content_index)
      {
        *existing = tool_call.clone();
      }

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

/// 单帧处理后的解析去向（从 sse.rs 迁入，聚合器需要引用）。
#[derive(Debug, Clone, Copy)]
pub(crate) enum LineFlow {
  /// 继续解析下一帧。
  Continue,
  /// 流已终结（`done` / `error` 已在本帧处理内调用，producer 已被消费）。
  Terminated,
  /// `push` 返回 `false`，消费端已放弃；停止解析且不再终结。
  ConsumerGone,
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

  // ===== R2 新增单测 =====

  /// 场景：StreamAggregator::new() 后 had_done_sentinel 默认 false。
  #[test]
  fn aggregator_default_no_done_sentinel() {
    let agg = test_aggregator();
    assert!(!agg.had_done_sentinel, "新建聚合器不应有 done 哨兵");
  }

  /// 场景：mark_done_sentinel() 置 true 后 finalize 走正常收尾路径。
  #[tokio::test]
  async fn mark_done_sentinel_stops_truncation() {
    let (producer, mut stream) = ProviderEventStreamProducer::build();
    let mut producer = Some(producer);
    let mut agg = test_aggregator();
    if let Some(p) = producer.as_ref() {
      p.push(ProviderEvent::Start {
        partial: agg.snapshot(),
      });
    }

    agg.text_delta(producer.as_ref().expect("alive"), "hello");
    agg.mark_done_sentinel();
    agg.set_finish_reason("stop");
    agg.finalize(&mut producer);

    let events = collect_events(&mut stream).await;
    // 应有正常的 TextEnd + Done（不是 Error + STREAM_INTERRUPTED）
    assert!(
      events
        .iter()
        .any(|e| matches!(e, ProviderEvent::TextEnd { .. }))
    );
    assert!(matches!(events.last(), Some(ProviderEvent::Done { .. })));
  }

  /// 场景：finalize 截断分支——had_done_sentinel=false 时以 Error + STREAM_INTERRUPTED 终结。
  #[tokio::test]
  async fn truncation_finalize_returns_error() {
    let (producer, mut stream) = ProviderEventStreamProducer::build();
    let mut producer = Some(producer);
    let mut agg = test_aggregator();
    if let Some(p) = producer.as_ref() {
      p.push(ProviderEvent::Start {
        partial: agg.snapshot(),
      });
    }

    // 聚合一些内容，但不调 mark_done_sentinel——模拟 EOF 提前断开
    agg.text_delta(producer.as_ref().expect("alive"), "partial content");
    // 不调 mark_done_sentinel！
    agg.finalize(&mut producer);

    let events = collect_events(&mut stream).await;
    match events.last().expect("terminal event") {
      ProviderEvent::Error { code, .. } => {
        assert_eq!(*code, Some(STREAM_INTERRUPTED));
      }
      other => panic!("expected Error with STREAM_INTERRUPTED, got {other:?}"),
    }
  }

  /// 场景：截断路径触发 terminal_error，output.content 中已聚合的部分仍保留。
  #[tokio::test]
  async fn truncation_preserves_aggregated_content() {
    let (producer, mut stream) = ProviderEventStreamProducer::build();
    let mut producer = Some(producer);
    let mut agg = test_aggregator();
    if let Some(p) = producer.as_ref() {
      p.push(ProviderEvent::Start {
        partial: agg.snapshot(),
      });
    }

    agg.text_delta(producer.as_ref().expect("alive"), "partial content");
    agg.finalize(&mut producer); // 截断分支

    let events = collect_events(&mut stream).await;
    match events.last().expect("terminal event") {
      ProviderEvent::Error { message, .. } => {
        // terminal_error 保留了已聚合的内容
        assert!(message.content.iter().any(|c| matches!(
          c,
          Content::Text(t) if t.content == "partial content"
        )));
        assert_eq!(message.stop_reason, StopReason::Error);
      }
      other => panic!("expected Error event, got {other:?}"),
    }
  }

  // ===== R1 新增单测 =====

  /// 场景：finalize 必须把重建后的 ToolCall 完整 arguments 写回 output.content。
  ///
  /// 修复前 output.content[content_index].arguments 是占位 json!({})，
  /// ToolCallEnd 事件是对的但 done 消息是错的；修复后两者一致。
  #[tokio::test]
  async fn finalize_backfills_tool_call_arguments_into_output_content() {
    let (producer, mut stream) = ProviderEventStreamProducer::build();
    let mut producer = Some(producer);
    let mut agg = test_aggregator();
    if let Some(p) = producer.as_ref() {
      p.push(ProviderEvent::Start {
        partial: agg.snapshot(),
      });
    }

    // 先推 ToolCallStart + ToolCallDelta（tool_delta 里 arguments 是占位 json!({})）
    agg.tool_delta(
      producer.as_ref().expect("alive"),
      0,
      Some("call-1"),
      Some("lookup"),
      Some(r#"{"q": "rust"}"#),
    );
    agg.mark_done_sentinel();
    agg.set_finish_reason("tool_calls");
    agg.finalize(&mut producer);

    let events = collect_events(&mut stream).await;
    match events.last().expect("terminal event") {
      ProviderEvent::Done { message, .. } => {
        // R1 修复：done 消息的 content 里 arguments 是完整的（不是占位 {}）
        assert!(matches!(
          message.content.first(),
          Some(Content::ToolCall(call))
          if call.arguments == serde_json::json!({"q": "rust"})
        ));
      }
      other => panic!("expected Done event, got {other:?}"),
    }
  }
}
