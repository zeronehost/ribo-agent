//! 首包超时守卫。
//!
//! 在把流交付给调用方之前，先在限定时限内窥视首个内容事件：
//! 按时等到内容则把窥视事件无损转发到新流后交付；
//! 超时或流异常关闭则返回对应结果，由重试层决定是否重试。

use std::time::Duration;

use ra_core::{
  message::{AssistantMessage, StopReason},
  utils::time::now_ms,
};

use crate::{
  error::STREAM_INTERRUPTED,
  options::CancelToken,
  stream::{ProviderEvent, ProviderEventStream, ProviderEventStreamProducer},
};

/// 首包超时守卫策略。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FirstTokenPolicy {
  /// 从建流成功到首个内容事件的最大等待时长。
  pub timeout: Duration,
  /// 首包超时后是否允许重试层重新发起请求。
  pub retry_on_timeout: bool,
}

/// 首包窥视结果。
pub(crate) enum Peeked {
  /// 已等到内容（或终结）事件：窥视事件已转发至新流，可直接交付。
  Ready(ProviderEventStream),
  /// 在策略时限内未等到首个内容事件。
  Timeout,
  /// 等待期间取消令牌被触发。
  Aborted,
  /// 原流在等到内容前已关闭（无终结事件的异常关闭）。
  Closed,
}

impl core::fmt::Debug for Peeked {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    match self {
      Self::Ready(_) => f.write_str("Peeked::Ready(..)"),
      Self::Timeout => f.write_str("Peeked::Timeout"),
      Self::Aborted => f.write_str("Peeked::Aborted"),
      Self::Closed => f.write_str("Peeked::Closed"),
    }
  }
}

/// 窥视流的首个内容事件，并按 [`FirstTokenPolicy`] 做超时控制。
///
/// 处理流程：
///
/// 1. 每轮先检查取消令牌，已触发则返回 [`Peeked::Aborted`]；
/// 2. 在 `policy.timeout` 时限内等待下一个事件；
/// 3. 等到内容或终结事件时，把已窥视事件无损转发到新流并返回
///    [`Peeked::Ready`]（原流未终结时由后台任务继续转发并兜底收尾）；
/// 4. 流提前关闭返回 [`Peeked::Closed`]，超时返回 [`Peeked::Timeout`]。
///
/// # 参数
///
/// - `stream`：待窥视的事件流（所有权被接管）；
/// - `policy`：首包超时守卫策略；
/// - `cancel`：可选的取消令牌。
pub(crate) async fn peek_first_content(
  stream: ProviderEventStream,
  policy: &FirstTokenPolicy,
  cancel: Option<&CancelToken>,
) -> Peeked {
  let mut peeked: Vec<ProviderEvent> = Vec::new();
  let mut stream = Some(stream);

  loop {
    if let Some(c) = cancel
      && c.is_cancelled()
    {
      tracing::info!("first-token guard aborted by cancellation");
      return Peeked::Aborted;
    }

    let current = stream
      .as_mut()
      .expect("stream 仅在返回 Ready/Closed 前被 take");
    let next_event = tokio::time::timeout(policy.timeout, current.next()).await;

    match next_event {
      Ok(Some(event)) => {
        let is_terminal = event.is_terminal();
        let is_content = event.is_content();
        peeked.push(event);

        if is_terminal || is_content {
          let old_stream = stream.take().expect("上方已排除 None");
          let (producer, new_stream) = ProviderEventStreamProducer::build();
          tokio::spawn(forward_loop(peeked, old_stream, producer));
          return Peeked::Ready(new_stream);
        }
      }
      Ok(None) => {
        return Peeked::Closed;
      }
      Err(_) => {
        tracing::warn!("first-token guard timed out, no content received");
        return Peeked::Timeout;
      }
    }
  }
}

/// 后台转发任务：把窥视阶段缓存的事件与原流剩余事件推送到新流。
///
/// 原流在终结事件前意外关闭时，合成 `STREAM_INTERRUPTED` 错误终结新流，
/// 避免调用方悬挂在无限等待上。
async fn forward_loop(
  peeked: Vec<ProviderEvent>,
  mut old_stream: ProviderEventStream,
  producer: ProviderEventStreamProducer,
) {
  for ev in peeked {
    if !producer.push(ev) {
      return;
    }
  }

  while let Some(event) = old_stream.next().await {
    if event.is_terminal() {
      terminal_producer(event, producer);
      return;
    }
    if !producer.push(event) {
      return;
    }
  }

  tracing::warn!(
    "forward loop: original stream closed before terminal, synthesizing STREAM_INTERRUPTED"
  );
  let msg = AssistantMessage::terminal(
    "",
    "",
    StopReason::Error,
    "stream closed before terminal event",
    now_ms(),
  );
  producer.error(StopReason::Error, msg, Some(STREAM_INTERRUPTED));
}

/// 把终结事件按其类型推送给 `producer`，完成新流的收尾。
///
/// 契约要求调用方先经 `is_terminal()` 过滤，因此无需再处理其他变体；
/// 非终结事件不会走到这里（否则 producer 被 drop，消费端按中断收场）。
fn terminal_producer(
  event: ProviderEvent,
  producer: ProviderEventStreamProducer,
) {
  if let ProviderEvent::Done { reason, message } = event {
    producer.done(reason, message);
    return;
  }
  if let ProviderEvent::Error {
    reason,
    message,
    code,
  } = event
  {
    producer.error(reason, message, code);
  }
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use super::*;

  /// 默认守卫策略（虚拟时钟下使用，不会真实等待）。
  fn policy(timeout: Duration) -> FirstTokenPolicy {
    crate::test_support::init();
    FirstTokenPolicy {
      timeout,
      retry_on_timeout: true,
    }
  }

  /// 取出窥视成功的流；非 Ready 结果直接 panic（仅测试使用）。
  fn into_ready(peeked: Peeked) -> ProviderEventStream {
    match peeked {
      Peeked::Ready(stream) => stream,
      other => panic!("预期 Peeked::Ready，实际为 {other:?}"),
    }
  }

  /// 构造一个事件快照。
  fn partial() -> Arc<AssistantMessage> {
    Arc::new(AssistantMessage::terminal(
      "",
      "",
      StopReason::Pending,
      "",
      0,
    ))
  }

  /// 场景：内容事件在时限内到达 → 返回 Ready，且窥视到的事件
  /// 连同后续事件与终结事件被无损转发到新流（数量、顺序、内容一致）。
  ///
  /// 预期：新流依次收到 Start、TextStart（窥视缓存）、TextDelta（后续
  /// 转发）、Done（终结分派），最后流关闭。
  #[tokio::test(start_paused = true)]
  async fn ready_stream_forwards_events_losslessly() {
    let (producer, stream) = ProviderEventStreamProducer::build();
    let start_ev = ProviderEvent::Start { partial: partial() };
    let text_start = ProviderEvent::TextStart {
      index: 0,
      partial: partial(),
    };
    let text_delta = ProviderEvent::TextDelta {
      index: 0,
      delta: "hi".into(),
      partial: partial(),
    };
    producer.push(start_ev.clone());
    producer.push(text_start.clone());
    producer.push(text_delta.clone());
    producer.done(
      StopReason::Stop,
      AssistantMessage::terminal("stub", "stub-model", StopReason::Stop, "", 0),
    );

    let mut new_stream = into_ready(
      peek_first_content(stream, &policy(Duration::from_millis(50)), None)
        .await,
    );

    assert_eq!(new_stream.next().await, Some(start_ev));
    assert_eq!(new_stream.next().await, Some(text_start));
    assert_eq!(new_stream.next().await, Some(text_delta));
    let event = new_stream.next().await.expect("应收到 Done");
    assert!(format!("{event:?}").contains("Done"));
    assert_eq!(new_stream.next().await, None);
  }

  /// 场景：`Start` 事件不算内容（TTFT 语义）——只有 Start 时守卫
  /// 继续等待，直至策略超时返回 Timeout 而非 Ready。
  ///
  /// 预期：Start 之后无内容事件，守卫在 50ms 虚拟时间后返回 Timeout。
  #[tokio::test(start_paused = true)]
  async fn start_event_is_not_treated_as_content() {
    let (producer, stream) = ProviderEventStreamProducer::build();
    producer.push(ProviderEvent::Start { partial: partial() });

    let res =
      peek_first_content(stream, &policy(Duration::from_millis(50)), None)
        .await;

    assert_eq!(
      format!("{res:?}"),
      "Peeked::Timeout",
      "Start 不应被判为内容，应等待至超时"
    );
  }

  /// 场景：时限内无任何事件 → 返回 Timeout。
  #[tokio::test(start_paused = true)]
  async fn times_out_when_no_event_arrives() {
    let (_producer, stream) = ProviderEventStreamProducer::build();

    let res =
      peek_first_content(stream, &policy(Duration::from_millis(10)), None)
        .await;

    assert_eq!(format!("{res:?}"), "Peeked::Timeout");
  }

  /// 场景：等待期间取消令牌已触发 → 返回 Aborted。
  #[tokio::test(start_paused = true)]
  async fn aborted_when_cancel_token_fired() {
    let (_producer, stream) = ProviderEventStreamProducer::build();
    let token = CancelToken::new();
    token.cancel();

    let res = peek_first_content(
      stream,
      &policy(Duration::from_secs(60)),
      Some(&token),
    )
    .await;

    assert_eq!(format!("{res:?}"), "Peeked::Aborted");
  }

  /// 场景：原流未终结即关闭（生产端 drop）→ 返回 Closed。
  #[tokio::test(start_paused = true)]
  async fn closed_when_producer_dropped_without_terminal() {
    let (producer, stream) = ProviderEventStreamProducer::build();
    drop(producer);

    let res =
      peek_first_content(stream, &policy(Duration::from_secs(60)), None).await;

    assert_eq!(format!("{res:?}"), "Peeked::Closed");
  }

  /// 场景：原流在终结事件前意外关闭 → forward_loop 合成
  /// STREAM_INTERRUPTED 终结事件，消费端不永久挂起。
  ///
  /// 预期：先收到窥视转发的 Start、TextStart，随后收到 code 为
  /// STREAM_INTERRUPTED 的 Error 终结事件；流随即关闭，`result()`
  /// 路径同样能拿到终态消息（stop_reason 为 Error）。
  #[tokio::test(start_paused = true)]
  async fn synthesized_interrupted_when_source_closes_before_terminal() {
    let (producer, stream) = ProviderEventStreamProducer::build();
    producer.push(ProviderEvent::Start { partial: partial() });
    producer.push(ProviderEvent::TextStart {
      index: 0,
      partial: partial(),
    });

    let mut new_stream = into_ready(
      peek_first_content(stream, &policy(Duration::from_secs(60)), None).await,
    );

    // 原流在生产端 drop 后关闭，且从未出现终结事件
    drop(producer);

    let first = new_stream.next().await.expect("应收到 Start");
    assert!(format!("{first:?}").contains("Start"));
    let second = new_stream.next().await.expect("应收到 TextStart");
    assert!(format!("{second:?}").contains("TextStart"));
    let event = new_stream.next().await.expect("应收到终结事件");
    assert!(format!("{event:?}").contains("Error"));
    assert!(format!("{event:?}").contains(STREAM_INTERRUPTED));
    assert_eq!(new_stream.next().await, None);

    // result() 路径不永久挂起，能拿到合成终态消息
    let final_msg = new_stream.result().await.expect("终态应被兑现");
    assert_eq!(final_msg.stop_reason, StopReason::Error);
  }

  /// 场景：窥视到内容后，源流以 `Error` 终结事件结束。
  ///
  /// 预期：`Error` 事件被原样转发，消费端可读取错误码。
  #[tokio::test(start_paused = true)]
  async fn forwards_error_terminal_event() {
    let (producer, stream) = ProviderEventStreamProducer::build();
    producer.push(ProviderEvent::TextStart {
      index: 0,
      partial: partial(),
    });
    producer.error(
      StopReason::Error,
      AssistantMessage::terminal("p1", "m1", StopReason::Error, "boom", 0),
      Some(STREAM_INTERRUPTED),
    );

    let mut new_stream = into_ready(
      peek_first_content(stream, &policy(Duration::from_secs(60)), None).await,
    );

    let first = new_stream.next().await.expect("应收到 TextStart");
    assert!(format!("{first:?}").contains("TextStart"));
    let event = new_stream.next().await.expect("应收到 Error 终结事件");
    assert!(format!("{event:?}").contains("Error"));
    assert!(format!("{event:?}").contains(STREAM_INTERRUPTED));
    assert_eq!(new_stream.next().await, None);
  }

  /// 场景：`Peeked` 的 Debug 输出覆盖全部变体。
  #[test]
  fn peeked_debug_covers_all_variants() {
    let (_, stream) = ProviderEventStreamProducer::build();

    assert_eq!(format!("{:?}", Peeked::Ready(stream)), "Peeked::Ready(..)");
    assert_eq!(format!("{:?}", Peeked::Timeout), "Peeked::Timeout");
    assert_eq!(format!("{:?}", Peeked::Aborted), "Peeked::Aborted");
    assert_eq!(format!("{:?}", Peeked::Closed), "Peeked::Closed");
  }

  /// 场景：`into_ready` 收到非 Ready 结果。
  ///
  /// 预期：panic（覆盖测试辅助函数的失败分支）。
  #[test]
  #[should_panic(expected = "预期 Peeked::Ready")]
  fn into_ready_panics_for_non_ready() {
    let _ = into_ready(Peeked::Timeout);
  }

  /// 场景：转发窥视缓存时消费端已关闭。
  ///
  /// 预期：转发任务在首个事件处直接返回，不 panic。
  #[tokio::test]
  async fn forward_loop_stops_when_consumer_closed() {
    let (producer, consumer) = ProviderEventStreamProducer::build();
    drop(consumer);
    let (_source_producer, source) = ProviderEventStreamProducer::build();

    forward_loop(
      vec![ProviderEvent::Start { partial: partial() }],
      source,
      producer,
    )
    .await;
  }

  /// 场景：转发后续事件时消费端关闭。
  ///
  /// 预期：任务在 while 循环的推送处返回，不 panic。
  #[tokio::test]
  async fn forward_loop_stops_on_later_push_when_consumer_closed() {
    let (producer, mut consumer) = ProviderEventStreamProducer::build();
    let (source_producer, source) = ProviderEventStreamProducer::build();
    let task = tokio::spawn(forward_loop(
      vec![ProviderEvent::Start { partial: partial() }],
      source,
      producer,
    ));

    // 先消费已窥视的 Start，确保首个 push 成功。
    let first = consumer.next().await.expect("应收到 Start");
    assert!(format!("{first:?}").contains("Start"));
    drop(consumer);

    // 再推入一个事件，使 while 循环中的 push 失败。
    source_producer.push(ProviderEvent::TextDelta {
      index: 0,
      delta: "x".into(),
      partial: partial(),
    });

    task.await.expect("转发任务不应 panic");
    drop(source_producer);
  }

  /// 场景：终结分派函数收到非终结事件（违反契约）。
  ///
  /// 预期：不推送任何终结事件，生产端被丢弃，消费端读取到通道关闭。
  #[tokio::test]
  async fn terminal_producer_ignores_non_terminal_event() {
    let (producer, mut consumer) = ProviderEventStreamProducer::build();

    terminal_producer(ProviderEvent::Start { partial: partial() }, producer);

    assert!(consumer.next().await.is_none());
  }
}
