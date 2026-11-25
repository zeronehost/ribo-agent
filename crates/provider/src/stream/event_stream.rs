//! 事件流传输机制：无界 mpsc 事件通道 + oneshot 终态。
//!
//! 生产端与消费端通过 [`ProviderEventStreamProducer::build`] 同时创建两对通道：
//!
//! - 无界 mpsc：承载所有事件（含终结），供逐事件消费路径 [`ProviderEventStream::next`]。
//! - oneshot：承载终态 [`AssistantMessage`]，供只取最终结果路径
//!   [`ProviderEventStream::result`]。
//!
//! 终结事件在推入 mpsc 的同一时刻兑现 oneshot，使"逐条消费"与"只取终态"
//! 两种用法共享同一份语义。
//!
//! # 不变量
//!
//! 1. `Start` 必须先于任何其他事件被推送；
//! 2. 终结只能发生一次——`done` / `error` 消费 `self`，编译期排除二次终结；
//! 3. 若生产端在未终结时被 drop（逻辑漏洞或任务异常退出），
//!    消费端 [`ProviderEventStream::result`] 以
//!    `Err(STREAM_INTERRUPTED, InStream)` 结束，保证永不永久挂起。

use ra_core::{
  error::RiboResult,
  message::{AssistantMessage, StopReason},
  utils::time::now_ms,
};
use tokio::sync::{mpsc, oneshot};

use crate::error::{STREAM_INTERRUPTED, in_stream};

use super::event::ProviderEvent;

// 消费端

/// 事件流的消费端。
///
/// 可任选两种方式消费：
///
/// - 逐条调用 [`next`](Self::next) 消费事件；
/// - 调用 [`result`](Self::result) 直接等待终结，丢弃中途事件。
///
/// 两种路径的终结语义完全一致：`Done → Ok(message)`、
/// `Error → Err(from_stream_terminal(code, message))`。
pub struct ProviderEventStream {
  /// 无界 mpsc 接收端：逐事件消费路径。
  rx: mpsc::UnboundedReceiver<ProviderEvent>,
  /// oneshot 终态接收端：`result()` 路径。
  res_rx: oneshot::Receiver<AssistantMessage>,
  /// 终结事件缓存：`next()` 读过终结事件后置位，后续一律返回 `None`。
  done: bool,
}

impl ProviderEventStream {
  /// 拉取下一个事件；终结事件被读取后，后续一律返回 `None`。
  ///
  /// 返回 `None` 有两种情况：
  /// - 流已终结（内部 `done = true`）；
  /// - 生产端未终结即 drop（通道关闭，按不变量 3 兜底）。
  pub async fn next(&mut self) -> Option<ProviderEvent> {
    if self.done {
      return None;
    }
    let event = self.rx.recv().await?;
    if event.is_terminal() {
      self.done = true;
    }
    Some(event)
  }

  /// 消费整个流并取最终消息。
  ///
  /// 中途的内容事件被丢弃，仅保留终结语义：
  /// - `Done` → `Ok(message)`；
  /// - `Error` → 内部合成 `Err(from_stream_terminal(code, message))`；
  /// - 通道未终结即关闭 → 内部合成 `Err(in_stream(STREAM_INTERRUPTED, "stream interrupted"))`。
  ///
  /// # 实现说明
  ///
  /// oneshot 在终结事件推入通道的同一时刻被兑现，与通道积压无关——
  /// 因此不需要逐条 drain mpsc，直接等 `res_rx` 即可。
  /// `self` 被 move，其中 mpsc 接收端随其余字段一并 drop，
  /// 生产端 `push` 会返回 `false`，触发协作式停止（见 `push` 注释）。
  pub async fn result(self) -> RiboResult<AssistantMessage> {
    match self.res_rx.await {
      Ok(message) => Ok(message),
      // 兜底：生产端未终结即消失
      // oneshot 发送端被 drop 而未兑现（逻辑漏洞或任务异常退出）。
      // 按不变量 3 兜底为 STREAM_INTERRUPTED，保证永不永久挂起。
      Err(_) => {
        tracing::warn!("provider event stream closed before terminal event");
        Err(in_stream(STREAM_INTERRUPTED, "stream interrupted"))
      }
    }
  }

  /// 拆分为事件流与终态通道，便于在不同任务中分别消费。
  ///
  /// 拆分后返回的 `(mpsc::UnboundedReceiver<ProviderEvent>,
  /// oneshot::Receiver<AssistantMessage>)` 与原结构体内部字段完全等价。
  /// 调用方可在事件任务中逐条 `next`（即 `rx.recv().await`），
  /// 同时在另一个任务 `res_rx.await` 等终态。
  pub fn split(
    self,
  ) -> (
    mpsc::UnboundedReceiver<ProviderEvent>,
    oneshot::Receiver<AssistantMessage>,
  ) {
    (self.rx, self.res_rx)
  }

  /// 把建流阶段结果折叠为纯事件流。
  ///
  /// - `Ok(stream)` → 原样返回；
  /// - `Err(err)` → 构造一条只含 `Error` 终结事件的即时终结流。
  ///
  /// 折叠后上层可被统一事件路径处理，且原始错误码被保留
  /// （`Error { code: Some(err.code()) }`），可被分类谓词继续判别。
  pub fn from_result(result: RiboResult<ProviderEventStream>) -> Self {
    match result {
      Ok(stream) => stream,
      Err(e) => {
        // 从错误 context 还原 provider / model
        // 约定构造器会注入这两个键；极早期失败（如 PROVIDER_NOT_FOUND）
        // 时可能缺失，回退为空串占位。
        let (provider, model) = e
          .context()
          .map(|c| {
            (
              c.get("provider").cloned().unwrap_or_default(),
              c.get("model").cloned().unwrap_or_default(),
            )
          })
          .unwrap_or_default();

        // 构造承载错误文本的空消息；建流阶段失败还没拿到任何内容、
        // 用量为零，stop_reason 固定为 Error。
        let msg = AssistantMessage::terminal(
          provider,
          model,
          StopReason::Error,
          e.message().to_string(),
          now_ms(),
        );

        // 构造生产端、立即终结、取消费端
        let (producer, stream) = ProviderEventStreamProducer::build();
        producer.error(StopReason::Error, msg, Some(e.code()));
        stream
      }
    }
  }
}

// 生产端

/// 事件流的生产端。
///
/// # 不变量
///
/// 1. `Start` 必须先于任何其他事件被推送；
/// 2. 终结只能发生一次——[`done`](Self::done) / [`error`](Self::error) 消费 `self`，
///    编译期排除二次终结；
/// 3. 若生产端在未终结时被 drop（`res_tx` 仍为 `Some`），
///    消费端 [`ProviderEventStream::result`] 收到 oneshot `Err`，
///    兜底为 `STREAM_INTERRUPTED`，保证永不永久挂起。
pub struct ProviderEventStreamProducer {
  /// 事件通道发送端（无界 mpsc）。
  tx: mpsc::UnboundedSender<ProviderEvent>,
  /// 终态通道发送端（`Option` 持有，`done`/`error` 时 take 兑现）。
  res_tx: Option<oneshot::Sender<AssistantMessage>>,
}

impl ProviderEventStreamProducer {
  /// 创建一对生产/消费端。
  ///
  /// 同时构造事件通道与终态通道，两者完全独立。
  /// 消费端可任选 `next()` 逐条消费，或 `result()` 直接等待终态。
  pub fn build() -> (Self, ProviderEventStream) {
    let (tx, rx) = mpsc::unbounded_channel::<ProviderEvent>();
    let (res_tx, res_rx) = oneshot::channel::<AssistantMessage>();

    (
      Self {
        tx,
        res_tx: Some(res_tx),
      },
      ProviderEventStream {
        rx,
        res_rx,
        done: false,
      },
    )
  }

  /// 推送一个非终结事件。
  ///
  /// # 返回
  ///
  /// `false` 表示消费端已关闭（被 drop），生产端应尽快停止解析并释放连接；
  /// 继续推送不会 panic，但事件将被直接丢弃。
  ///
  /// # 实现说明
  ///
  /// 旧版本使用 `&mut self` 持有发送端——`tokio` 的 `UnboundedSender`
  /// 本身实现 `Clone` 且 `send` 只需 `&self`，`&mut self` 是多余约束，
  /// 已改为 `&self`（更通用，适配器可在任务内多处调用）。
  pub fn push(&self, event: ProviderEvent) -> bool {
    self.tx.send(event).is_ok()
  }

  /// 以成功终结流：推送 [`ProviderEvent::Done`] 并兑现 oneshot 终态。
  ///
  /// 生产端由此被消费，终结唯一性在类型层面成立。
  pub fn done(mut self, reason: StopReason, message: AssistantMessage) {
    // 克隆给终态通道
    // 事件通道与 oneshot 各自持有一份 message，保证两条路径独立。
    let msg = message.clone();

    // 推入终结事件
    // 收方关闭时 send 返回 Err(event)，用 `let _ =` 忽略——
    // 生产端不应因对端已放弃而 panic。
    let _ = self.tx.send(ProviderEvent::Done { reason, message });

    // 兑现 oneshot
    // 终结只会发生一次（`done` 消费 `self`），因此发送端必定存在。
    let tx = self
      .res_tx
      .take()
      .expect("terminal event fired more than once");
    let _ = tx.send(msg);
  }

  /// 以失败终结流：推送 [`ProviderEvent::Error`] 并兑现 oneshot 终态（Err 侧）。
  ///
  /// `reason = StopReason::Aborted` 时表示用户取消（不计入上游故障）。
  /// 旧版本的 code 是 `Option<ErrorKind>`——现已统一为 `Option<&'static str>`，
  /// 取自 `crate::error` 的错误码常量模块。
  pub fn error(
    mut self,
    reason: StopReason,
    message: AssistantMessage,
    code: Option<&'static str>,
  ) {
    // 克隆给终态通道
    let msg = message.clone();

    // 推入终结事件
    let _ = self.tx.send(ProviderEvent::Error {
      reason,
      message,
      code,
    });

    // 兑现 oneshot
    // 终结只会发生一次（`error` 消费 `self`），因此发送端必定存在。
    let tx = self
      .res_tx
      .take()
      .expect("terminal event fired more than once");
    let _ = tx.send(msg);
  }
}

#[cfg(test)]
mod tests {
  use ra_core::message::{AssistantMessage, StopReason};

  use super::*;
  use crate::error::{SERVER_ERROR, STREAM_INTERRUPTED, pre_stream};

  fn done_message() -> AssistantMessage {
    crate::test_support::init();
    AssistantMessage::terminal("p1", "m1", StopReason::Stop, "", 0)
  }

  /// 场景：建流失败折叠为即时终结流。
  ///
  /// 预期：`next()` 收到的 `Error` 事件保留原错误码与错误文本。
  #[tokio::test]
  async fn from_result_folds_error_and_preserves_code() {
    let err = pre_stream(SERVER_ERROR, "boom");
    let mut stream = ProviderEventStream::from_result(Err(err));

    let event = stream.next().await.expect("应收到终结事件");
    assert!(format!("{event:?}").contains(SERVER_ERROR));
    assert!(format!("{event:?}").contains("boom"));
  }

  /// 场景：建流成功时 `from_result` 原样透传。
  ///
  /// 预期：`result()` 返回原始流的最终消息。
  #[tokio::test]
  async fn from_result_returns_original_stream() {
    let (producer, stream) = ProviderEventStreamProducer::build();
    producer.done(StopReason::Stop, done_message());

    let message = ProviderEventStream::from_result(Ok(stream))
      .result()
      .await
      .expect("成功流应返回最终消息");
    assert_eq!(message.stop_reason, StopReason::Stop);
  }

  /// 场景：错误 context 中带有 provider / model 元信息。
  ///
  /// 预期：折叠消息能还原这两个字段。
  #[tokio::test]
  async fn from_result_restores_metadata_from_context() {
    let err = pre_stream(SERVER_ERROR, "boom")
      .with_context("provider", "p1")
      .with_context("model", "m1");
    let message = ProviderEventStream::from_result(Err(err))
      .result()
      .await
      .expect("折叠流应产出终态消息");

    assert_eq!(message.provider, "p1");
    assert_eq!(message.model, "m1");
  }

  /// 场景：错误构造器写入 provider / model 元信息槽位（对应审查报告 R8）。
  ///
  /// 预期：`pre_stream` 生成的错误自带两个 context 键，`from_result`
  /// 可直接读取；适配器可用真实值覆盖。
  #[test]
  fn pre_stream_error_carries_provider_metadata() {
    let err = pre_stream(SERVER_ERROR, "boom");
    let context = err.context().expect("错误应携带 context");
    assert!(context.contains_key("provider"));
    assert!(context.contains_key("model"));
  }

  /// 场景：生产端未终结即被丢弃。
  ///
  /// 预期：`result()` 兜底为 STREAM_INTERRUPTED，永不永久挂起。
  #[tokio::test]
  async fn producer_dropped_before_terminal_yields_interrupted() {
    let (producer, stream) = ProviderEventStreamProducer::build();
    drop(producer);

    let err = stream.result().await.expect_err("未终结的流应报错");
    assert_eq!(err.code(), STREAM_INTERRUPTED);
  }

  /// 场景：`split` 拆出的事件通道与终态通道各自独立可用。
  ///
  /// 预期：事件通道收到 `Done`，终态通道收到同一条最终消息。
  #[tokio::test]
  async fn split_exposes_event_and_result_channels() {
    let (producer, stream) = ProviderEventStreamProducer::build();
    let (mut rx, res_rx) = stream.split();

    producer.done(StopReason::Stop, done_message());

    let event = rx.recv().await.expect("应收到终结事件");
    assert!(format!("{event:?}").contains("Done"));
    let message = res_rx.await.expect("应收到终态消息");
    assert_eq!(message.stop_reason, StopReason::Stop);
  }

  /// 场景：读到终结事件后继续调用 `next`。
  ///
  /// 预期：后续读取一律返回 `None`，不会挂起或重复返回终结事件。
  #[tokio::test]
  async fn next_after_terminal_returns_none() {
    let (producer, mut stream) = ProviderEventStreamProducer::build();
    producer.done(StopReason::Stop, done_message());

    let event = stream.next().await.expect("应收到 Done");
    assert!(format!("{event:?}").contains("Done"));
    assert!(stream.next().await.is_none());
  }
}
