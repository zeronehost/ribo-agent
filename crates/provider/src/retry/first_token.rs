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

    let next_event = match stream.as_mut() {
      Some(s) => tokio::time::timeout(policy.timeout, s.next()).await,
      None => return Peeked::Closed,
    };

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
fn terminal_producer(
  event: ProviderEvent,
  producer: ProviderEventStreamProducer,
) {
  match event {
    ProviderEvent::Done { reason, message } => producer.done(reason, message),
    ProviderEvent::Error {
      reason,
      message,
      code,
    } => producer.error(reason, message, code),
    _ => unreachable!("只在 is_terminal() 命中后才调用"),
  }
}
