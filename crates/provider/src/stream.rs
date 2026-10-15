//! 模型流式响应的事件通道。
//!
//! 提供方以流式方式产出回复时，事件种类多（思考增量、文本增量、工具调用……），
//! 且最终还要收敛出一条完整的 [`AssistantMessage`]。本模块用一对类型解决该问题：
//!
//! - [`ProviderEventStreamProducer`]：提供方实现侧持有，通过
//!   [`push`](ProviderEventStreamProducer::push) 逐条推送事件；
//! - [`ProviderEventStream`]：调用侧持有，通过 [`next`](ProviderEventStream::next)
//!   拉取事件，结束后通过 [`result`](ProviderEventStream::result) 取最终消息。
//!
//! 二者由 [`ProviderEventStreamProducer::build`] 一次性构造，底层是两条通道：
//!
//! - 一条无界 mpsc 通道承载所有 [`ProviderEvent`]；
//! - 一条 oneshot 通道承载终结事件（Done/Error）携带的最终 [`AssistantMessage`]，
//!   使调用方即使跳过部分事件也能拿到收敛结果。
//!
//! 终结约定：推送 [`ProviderEvent::Done`] 或 [`ProviderEvent::Error`] 后流即结束，
//! 生产端会标记完成并兑现 oneshot；此后再推送的事件会被静默丢弃。
//!
//! # Examples
//!
//! 典型的生产/消费配合方式（`stream` 模块尚未在 crate 根导出，
//! 模块公开后该示例即可编译运行）：
//!
//! ```ignore
//! # #[tokio::main]
//! # async fn main() {
//! use std::sync::Arc;
//!
//! use ra_core::message::{AssistantMessage, StopReason};
//! use ra_provider::stream::{
//!   ProviderEvent, ProviderEventStream, ProviderEventStreamProducer,
//! };
//!
//! async fn produce(mut producer: ProviderEventStreamProducer) {
//!   let partial = Arc::new(AssistantMessage::empty("demo", "gpt", 0));
//!   producer.push(ProviderEvent::Start { partial });
//!   let message = AssistantMessage::empty("demo", "gpt", 0);
//!   producer.push(ProviderEvent::Done {
//!     reason: StopReason::Stop,
//!     message,
//!   });
//! }
//!
//! let (producer, mut stream) = ProviderEventStreamProducer::build();
//! tokio::spawn(produce(producer));
//!
//! while let Some(event) = stream.next().await {
//!   println!("{event:?}");
//! }
//! let final_message = stream.result().await.unwrap();
//! # let _ = final_message;
//! # }
//! ```

use ra_core::{error::RiboResult, message::AssistantMessage};
use tokio::sync::{mpsc, oneshot};

mod event;
use crate::error::ProviderError;

pub use self::event::*;

/// 模型流式响应的消费端句柄，由 [`ProviderEventStreamProducer::build`] 创建。
///
/// 典型用法是循环调用 [`next`](Self::next) 拉取事件直到得到 `None`，
/// 再调用 [`result`](Self::result) 获取收敛后的完整消息；
/// 也可以通过 [`split`](Self::split) 绕过封装直接持有两个底层接收端。
pub struct ProviderEventStream {
  // 承载逐条事件的无界通道接收端。
  rx: mpsc::UnboundedReceiver<ProviderEvent>,
  // 承载终结事件所携带最终消息的一次性接收端。
  result_rx: oneshot::Receiver<AssistantMessage>,
  // 收到终结事件后置为 true，使后续 next 直接短路返回 None。
  done: bool,
}

impl ProviderEventStream {
  /// 拉取下一个事件。
  ///
  /// - 已收到终结事件或事件通道关闭时返回 `None`；
  /// - 收到 [`ProviderEvent::Done`] / [`ProviderEvent::Error`] 时会把该事件返回给调用方，
  ///   同时内部标记结束，此后再次调用恒为 `None`。
  ///
  /// 终结事件中的完整消息请随后通过 [`result`](Self::result) 获取。
  pub async fn next(&mut self) -> Option<ProviderEvent> {
    // 终结后短路，避免再次 await 已无需读取的通道。
    if self.done {
      return None;
    }
    let event = self.rx.recv().await?;
    if event.is_terminal() {
      self.done = true;
    }
    Some(event)
  }

  /// 等待并返回终结事件收敛出的最终助手消息。
  ///
  /// 生产端推送 Done/Error 时通过 oneshot 兑现该消息；
  /// 若生产端在未推送终结事件的情况下被丢弃，返回 `recv_error` 错误。
  pub async fn result(self) -> RiboResult<AssistantMessage> {
    self.result_rx.await.map_err(ProviderError::recv_error)
  }

  /// 拆分为底层的事件接收端与最终消息接收端，交由调用方自行驱动。
  ///
  /// 拆分后 [`next`](Self::next) 的终结短路语义不再生效，
  /// 调用方需自行识别终结事件并从 oneshot 接收端取最终消息。
  ///
  /// # 返回
  ///
  /// 返回二元组：事件流 mpsc 接收端，以及最终消息的 oneshot 接收端。
  pub fn split(
    self,
  ) -> (
    mpsc::UnboundedReceiver<ProviderEvent>,
    oneshot::Receiver<AssistantMessage>,
  ) {
    (self.rx, self.result_rx)
  }
}

/// 模型流式响应的生产端句柄，由提供方实现侧持有。
///
/// 通过 [`build`](Self::build) 与消费端 [`ProviderEventStream`] 成对创建。
/// 生产端被丢弃（或显式调用 [`close`](Self::close)）后，事件通道关闭，
/// 消费端的 [`next`](ProviderEventStream::next) 返回 `None`。
pub struct ProviderEventStreamProducer {
  // 事件通道发送端；所有接收端丢弃后 push 会返回 false。
  tx: mpsc::UnboundedSender<ProviderEvent>,
  // 最终消息的一次性发送端；用 Option 以便终结时 take 出来消费。
  result_tx: Option<oneshot::Sender<AssistantMessage>>,
  // 是否已推送过终结事件；终结后所有推送均被忽略。
  done: bool,
}

impl ProviderEventStreamProducer {
  /// 推送一个事件到事件流。
  ///
  /// - 推送 [`ProviderEvent::Done`] / [`ProviderEvent::Error`] 时流立即终结：
  ///   内部标记完成，并把事件中的消息通过 oneshot 兑现给消费端；
  /// - 终结之后再推送的事件会被静默丢弃，本方法仍返回 `true`；
  /// - 事件接收端已全部丢弃时发送失败，返回 `false`。
  ///
  /// # 参数
  ///
  /// - `event`：待推送的流式事件。
  ///
  /// # 返回
  ///
  /// 返回 `false` 表示事件通道已无接收端；其余情况返回 `true`
  ///（含终结后事件被丢弃的情况）。
  pub fn push(&mut self, event: ProviderEvent) -> bool {
    if self.done {
      return true;
    }

    // ===== 终结处理 =====
    // 终结事件只需兑现一次最终消息；先标记 done 以阻断后续一切推送。
    if event.is_terminal() {
      self.done = true;
      match &event {
        ProviderEvent::Done { message, .. }
        | ProviderEvent::Error { message, .. } => {
          self.fulfill_result(message.clone())
        }
        _ => {}
      }
    }
    self.tx.send(event).is_ok()
  }

  /// 把终结事件携带的最终消息送入 oneshot 通道。
  ///
  /// 发送端用 `take` 取出，保证只兑现一次；消费端已放弃结果时
  /// （接收端被丢弃）发送失败会被显式忽略。
  fn fulfill_result(&mut self, message: AssistantMessage) {
    if let Some(tx) = self.result_tx.take() {
      let _ = tx.send(message);
    }
  }

  /// 返回流是否已经终结（已推送过 Done/Error 事件）。
  pub fn is_done(&self) -> bool {
    self.done
  }

  /// 关闭生产端。
  ///
  /// 语义等同于直接丢弃当前实例：事件通道随之关闭，
  /// 消费端拉完缓冲事件后得到 `None`。
  pub fn close(self) {
    drop(self);
  }

  /// 创建一对相互连接的生产端与消费端。
  ///
  /// 内部同时建立无界事件通道与一次性最终消息通道，
  /// 生产端交由提供方实现使用，消费端返回给调用方。
  ///
  /// # Examples
  ///
  /// ```ignore
  /// // stream 模块尚未在 crate 根导出，模块公开后可编译运行
  /// let (producer, stream) = ProviderEventStreamProducer::build();
  /// # drop(producer);
  /// # drop(stream);
  /// ```
  pub fn build() -> (Self, ProviderEventStream) {
    let (tx, rx) = mpsc::unbounded_channel::<ProviderEvent>();
    let (result_tx, result_rx) = oneshot::channel::<AssistantMessage>();
    (
      Self {
        tx,
        result_tx: Some(result_tx),
        done: false,
      },
      ProviderEventStream {
        rx,
        result_rx,
        done: false,
      },
    )
  }
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use ra_core::message::StopReason;

  use super::*;

  // 构造一条测试用助手消息，作为各事件的载体。
  fn assistant_message() -> AssistantMessage {
    AssistantMessage::empty("test-provider", "test-model", 0)
  }

  // 构造非终结的 Start 事件。
  fn start_event() -> ProviderEvent {
    ProviderEvent::Start {
      partial: Arc::new(assistant_message()),
    }
  }

  // 构造终结的 Done 事件。
  fn done_event() -> ProviderEvent {
    ProviderEvent::Done {
      reason: StopReason::Stop,
      message: assistant_message(),
    }
  }

  // 场景：生产端推送一个非终结事件。
  // 预期：消费端收到该事件，且事件与生产端均未标记结束。
  #[tokio::test]
  async fn push_non_terminal_event_test() {
    let (mut producer, mut stream) = ProviderEventStreamProducer::build();

    assert!(producer.push(start_event()));
    assert!(!producer.is_done());

    let event = stream
      .next()
      .await
      .expect("non-terminal event should be delivered");
    assert!(matches!(event, ProviderEvent::Start { .. }));
    assert!(!event.is_terminal());
  }

  // 场景：依次推送非终结事件与 Done 终结事件。
  // 预期：两个事件按序到达；收到 Done 后流结束，此后 next 恒为 None。
  #[tokio::test]
  async fn done_event_terminates_stream_test() {
    let (mut producer, mut stream) = ProviderEventStreamProducer::build();

    assert!(producer.push(start_event()));
    assert!(producer.push(done_event()));
    assert!(producer.is_done());

    assert!(matches!(
      stream.next().await,
      Some(ProviderEvent::Start { .. })
    ));
    assert!(matches!(
      stream.next().await,
      Some(ProviderEvent::Done { .. })
    ));
    // 终结之后再次拉取应直接返回 None，且重复调用行为稳定
    assert!(stream.next().await.is_none());
    assert!(stream.next().await.is_none());
  }

  // 场景：推送 Done 后通过 result 取最终消息。
  // 预期：oneshot 已在终结时兑现，返回与事件中相同的助手消息。
  #[tokio::test]
  async fn result_returns_done_message_test() {
    let (mut producer, stream) = ProviderEventStreamProducer::build();
    let expected = assistant_message();
    producer.push(ProviderEvent::Done {
      reason: StopReason::Stop,
      message: expected.clone(),
    });

    let message = stream
      .result()
      .await
      .expect("done event should fulfill result");
    assert_eq!(message, expected);
  }

  // 场景：以 Error 事件终结流。
  // 预期：Error 与 Done 一样会兑现最终结果，消费端能拿到事件中的消息。
  #[tokio::test]
  async fn error_event_fulfills_result_test() {
    let (mut producer, stream) = ProviderEventStreamProducer::build();
    let expected = assistant_message();
    producer.push(ProviderEvent::Error {
      reason: StopReason::Error,
      message: expected.clone(),
    });

    let message = stream
      .result()
      .await
      .expect("error terminal event should fulfill result");
    assert_eq!(message, expected);
  }

  // 场景：流已终结后继续推送事件。
  // 预期：push 仍返回 true，但事件被直接丢弃，终结事件之后不会再出现数据。
  #[tokio::test]
  async fn push_after_terminal_is_ignored_test() {
    let (mut producer, mut stream) = ProviderEventStreamProducer::build();

    producer.push(done_event());
    assert!(producer.push(start_event()));

    assert!(matches!(
      stream.next().await,
      Some(ProviderEvent::Done { .. })
    ));
    assert!(stream.next().await.is_none());
  }

  // 场景：生产端推送缓冲事件后、未发终结事件即被丢弃。
  // 预期：已缓冲事件仍可取出，通道关闭后 next 返回 None。
  #[tokio::test]
  async fn producer_drop_closes_event_stream_test() {
    let (mut producer, mut stream) = ProviderEventStreamProducer::build();
    producer.push(start_event());
    drop(producer);

    assert!(stream.next().await.is_some());
    assert!(stream.next().await.is_none());
  }

  // 场景：生产端未推送任何终结事件即被丢弃，消费端转而 await result。
  // 预期：oneshot 发送端随生产端消失，result 收到错误并被转换为 recv_error。
  #[tokio::test]
  async fn result_errors_when_producer_dropped_test() {
    let (producer, stream) = ProviderEventStreamProducer::build();
    drop(producer);

    let err = stream
      .result()
      .await
      .expect_err("dropped producer should fail result");
    assert_eq!(err.code(), "recv_error");
  }

  // 场景：推送前消费端被整体丢弃。
  // 预期：事件通道已无接收端，push 发送失败并返回 false。
  #[tokio::test]
  async fn push_returns_false_when_receiver_dropped_test() {
    let (mut producer, stream) = ProviderEventStreamProducer::build();
    drop(stream);

    assert!(!producer.push(start_event()));
  }

  // 场景：调用 split 拆出事件接收端与结果接收端，绕过封装直接操作两个通道。
  // 预期：事件按序进入事件接收端，终结消息进入 oneshot 接收端。
  #[tokio::test]
  async fn split_delivers_event_and_result_test() {
    let (mut producer, stream) = ProviderEventStreamProducer::build();
    let (mut event_rx, result_rx) = stream.split();

    producer.push(start_event());
    producer.push(done_event());

    assert!(matches!(
      event_rx.recv().await,
      Some(ProviderEvent::Start { .. })
    ));
    assert!(matches!(
      event_rx.recv().await,
      Some(ProviderEvent::Done { .. })
    ));
    let message = result_rx.await.expect("result should be fulfilled");
    assert_eq!(message.provider, "test-provider");
  }

  // 场景：显式调用 close 关闭生产端。
  // 预期：生产端被消费并丢弃，消费端再拉取事件得到 None。
  #[tokio::test]
  async fn close_ends_stream_test() {
    let (producer, mut stream) = ProviderEventStreamProducer::build();
    producer.close();

    assert!(stream.next().await.is_none());
  }
}
