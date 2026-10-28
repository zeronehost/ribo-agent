//! 被观察的事件流：在 `ProviderEventStream` 外包一层熔断回报与并发配额管理。
//!
//! 网关建流成功后把原始流包装为 [`ObservedStream`]：
//!
//! - 终结时（`Done` / `Error`）向熔断器回报成功 / 失败，并释放在途配额；
//! - 支持空闲超时：超过时限未收到任何事件即回收原流，并合成
//!   `TIMEOUT` 错误终结事件；
//! - 取消（`Aborted` / `CANCELLED`）与内容过滤（`CONTENT_FILTERED`）
//!   不计入熔断失败——前者是用户行为，后者不是上游故障。
//!
//! 无论走 [`next`](ObservedStream::next) 还是 [`result`](ObservedStream::result)
//! 路径，熔断回报与配额释放都恰好执行一次（`observed` 标志保证）。

use std::{sync::Arc, time::Duration};

use ra_core::{
  error::RiboResult,
  message::{AssistantMessage, StopReason},
  utils::time::now_ms,
};
use tokio::sync::OwnedSemaphorePermit;

use crate::{
  error::{
    CANCELLED, CONTENT_FILTERED, STREAM_INTERRUPTED, TIMEOUT, in_stream,
  },
  gateway::circuit::{Admission, CircuitBreaker},
  stream::ProviderEvent,
};

use super::event_stream::ProviderEventStream;

/// 带熔断观察与配额管理的事件流。
///
/// 由 [`crate::gateway::Gateway::stream`] 创建并返回；终结语义与原始流
/// 完全一致，额外在终结时回报熔断器并释放在途配额。
pub struct ObservedStream {
  /// 被包装的原始流；空闲超时触发后被移除，后续读取直接结束。
  inner: Option<ProviderEventStream>,
  /// 建流时获取的在途配额；终结时释放。
  permit: Option<OwnedSemaphorePermit>,
  /// 本候选对应的熔断器。
  breaker: Arc<CircuitBreaker>,
  /// 建流时的准入结果（区分普通放行与半开探测）。
  admission: Admission,
  /// 流空闲超时；`None` 表示不限制。
  idle_timeout: Option<Duration>,
  /// 是否已回报过熔断（保证只回报一次）。
  observed: bool,
}

impl ObservedStream {
  /// 包装建流结果：绑定熔断器、准入结果与在途配额。
  pub(crate) fn new(
    inner: ProviderEventStream,
    permit: OwnedSemaphorePermit,
    breaker: Arc<CircuitBreaker>,
    admission: Admission,
    idle_timeout: Option<Duration>,
  ) -> Self {
    Self {
      inner: Some(inner),
      permit: Some(permit),
      breaker,
      admission,
      idle_timeout,
      observed: false,
    }
  }

  /// 观测到成功终结：回报熔断器成功并释放配额（只回报一次）。
  fn observe_done(&mut self) {
    self.observed = true;
    self.breaker.record_success();
    tracing::debug!(
      probe = matches!(self.admission, Admission::AllowProbe),
      "observed stream finished successfully"
    );
    self.release_permit();
  }

  /// 观测到失败终结：按错误类型回报熔断器并释放配额（只回报一次）。
  ///
  /// 取消（`Aborted` / `CANCELLED`）与内容过滤（`CONTENT_FILTERED`）
  /// 不推动熔断；其余错误均记一次失败。
  fn observe_error(&mut self, reason: &StopReason, code: Option<&'static str>) {
    self.observed = true;
    // 取消 / 内容过滤不触发熔断（保留空分支以显式表达该语义）
    if !(matches!(reason, StopReason::Aborted)
      || code == Some(CANCELLED)
      || code == Some(CONTENT_FILTERED))
    {
      self.breaker.record_failure();
    }
    tracing::debug!(
      probe = matches!(self.admission, Admission::AllowProbe),
      code = code.unwrap_or("none"),
      "observed stream terminated with error"
    );
    self.release_permit();
  }
  /// 释放在途配额（drop permit 即刻归还信号量）。
  fn release_permit(&mut self) {
    self.permit = None;
  }

  /// 拉取下一个事件；终结事件回报熔断器并释放配额。
  ///
  /// 配置了空闲超时时，超过时限未收到任何事件则回收原流，
  /// 合成 `TIMEOUT` 错误终结事件返回（此后再调用返回 `None`）。
  pub async fn next(&mut self) -> Option<ProviderEvent> {
    let idle = self.idle_timeout;

    // 内部中间结果：区分"正常收到事件"与"空闲超时"
    enum NextOutcome {
      Event(Box<Option<ProviderEvent>>),
      IdleTimeout,
    }
    let outcome = {
      let inner = self.inner.as_mut()?;
      match idle {
        None => NextOutcome::Event(Box::new(inner.next().await)),
        Some(idle) => tokio::select! {
          ev = inner.next() => NextOutcome::Event(Box::new(ev)),
          _ = tokio::time::sleep(idle) => NextOutcome::IdleTimeout,
        },
      }
    };

    match outcome {
      NextOutcome::Event(event) => {
        let ev = *event;
        match ev {
          Some(ProviderEvent::Done { .. }) => self.observe_done(),
          Some(ProviderEvent::Error {
            ref reason,
            ref code,
            ..
          }) => self.observe_error(reason, *code),
          _ => {}
        }
        ev
      }
      NextOutcome::IdleTimeout => {
        // ===== 空闲超时：回收原流并合成 TIMEOUT 终结 =====
        let taken = self.inner.take();
        drop(taken);
        self.observe_error(&StopReason::Error, Some(TIMEOUT));
        let msg = AssistantMessage::terminal(
          String::new(),
          String::new(),
          StopReason::Error,
          "stream idle".to_string(),
          now_ms(),
        );
        Some(ProviderEvent::Error {
          reason: StopReason::Error,
          message: msg,
          code: Some(TIMEOUT),
        })
      }
    }
  }
  /// 消费整个流并取最终消息（见 [`ProviderEventStream::result`]）。
  ///
  /// 与 [`next`](Self::next) 共享同一套观察语义：按最终结果回报
  /// 熔断器并释放配额；空闲超时同样以 `TIMEOUT` 终结。
  pub async fn result(self) -> RiboResult<AssistantMessage> {
    let Self {
      inner,
      permit,
      breaker,
      admission,
      idle_timeout,
      observed,
    } = self;

    // ===== 等待终态 =====
    // inner 为 None 说明空闲超时已回收原流，直接以 TIMEOUT 收场。
    let awaited = match (inner, idle_timeout) {
      (None, _) => Err(in_stream(TIMEOUT, "stream idle")),
      (Some(inner), None) => inner.result().await,
      (Some(inner), Some(idle)) => tokio::select! {
        res = inner.result() => res,
        _ = tokio::time::sleep(idle) => {
          tracing::warn!("gateway stream idle timeout while awaiting result");
          Err(in_stream(TIMEOUT, "stream idle"))
        }
      },
    };

    // ===== 熔断回报与配额释放 =====
    // `if !observed` 守卫保证熔断回报恰好执行一次：若调用方先经
    // next() 消费到终结事件（已回报并置位标志），此处直接跳过。
    // 本函数随即返回，无需再置位 observed——该赋值在此路径无读取方。
    if !observed {
      match &awaited {
        // ===== 按终态消息的真实结果回报 =====
        // `Error` 终结事件经 oneshot 兑现后 `result()` 仍返回 `Ok(message)`，
        // 仅凭 `Ok/Err` 会把失败误记为成功；必须看 `stop_reason`。
        Ok(message) if message.stop_reason == StopReason::Error => {
          breaker.record_failure();
          tracing::debug!(
            probe = matches!(admission, Admission::AllowProbe),
            "observed stream terminated with error"
          );
        }
        // 取消是用户行为，不计入熔断失败，也不视为探测成功。
        Ok(message) if message.stop_reason == StopReason::Aborted => {}
        Ok(_) => {
          breaker.record_success();
          tracing::debug!(
            probe = matches!(admission, Admission::AllowProbe),
            "observed stream finished successfully"
          );
        }
        Err(e) => match e.code() {
          // 通道在终态前关闭属于契约破坏：单独记录错误日志，并计入熔断失败。
          STREAM_INTERRUPTED => {
            tracing::error!(
              "gateway stream closed before terminal event (contract violation)"
            );
            breaker.record_failure();
          }
          // `result()` 的 Err 侧只可能来自空闲超时（TIMEOUT），同样记一次失败。
          _ => breaker.record_failure(),
        },
      }
    }
    drop(permit);
    awaited
  }
}

#[cfg(test)]
mod tests {
  use std::time::Duration;

  use ra_core::message::{AssistantMessage, StopReason};
  use tokio::sync::{OwnedSemaphorePermit, Semaphore};

  use super::*;
  use crate::{
    error::{CANCELLED, CONTENT_FILTERED, SERVER_ERROR, STREAM_INTERRUPTED},
    gateway::circuit::CircuitState,
    stream::ProviderEventStreamProducer,
  };

  fn breaker(threshold: u64) -> Arc<CircuitBreaker> {
    crate::test_support::init();
    Arc::new(CircuitBreaker::new(threshold, Duration::from_secs(30)))
  }

  fn permits() -> (Arc<Semaphore>, OwnedSemaphorePermit) {
    let semaphore = Arc::new(Semaphore::new(1));
    let permit = semaphore
      .clone()
      .try_acquire_owned()
      .expect("测试信号量应有配额");
    (semaphore, permit)
  }

  fn terminal(text: &str) -> AssistantMessage {
    AssistantMessage::terminal("p1", "m1", StopReason::Error, text, 0)
  }

  fn done_message() -> AssistantMessage {
    AssistantMessage::terminal("p1", "m1", StopReason::Stop, "", 0)
  }

  /// 场景：流以 `Done` 正常终结。
  ///
  /// 预期：熔断器记录成功并保持 Closed，配额恰好在终结时归还。
  #[tokio::test]
  async fn done_records_success_and_releases_permit() {
    let (semaphore, permit) = permits();
    let breaker = breaker(1);
    let (producer, stream) = ProviderEventStreamProducer::build();
    let mut observed = ObservedStream::new(
      stream,
      permit,
      breaker.clone(),
      Admission::Allow,
      None,
    );

    producer.done(StopReason::Stop, done_message());
    let event = observed.next().await.expect("应收到 Done");
    assert!(format!("{event:?}").contains("Done"));

    assert_eq!(breaker.state(), CircuitState::Closed);
    assert_eq!(semaphore.available_permits(), 1);
    assert!(observed.next().await.is_none());
  }

  /// 场景：流以 `Error` 终结。
  ///
  /// 预期：熔断器记录失败并打开，配额归还。
  #[tokio::test]
  async fn error_records_failure_and_releases_permit() {
    let (semaphore, permit) = permits();
    let breaker = breaker(1);
    let (producer, stream) = ProviderEventStreamProducer::build();
    let mut observed = ObservedStream::new(
      stream,
      permit,
      breaker.clone(),
      Admission::Allow,
      None,
    );

    producer.error(StopReason::Error, terminal("boom"), Some(SERVER_ERROR));
    let event = observed.next().await.expect("应收到 Error");
    assert!(format!("{event:?}").contains("Error"));

    assert_eq!(breaker.state(), CircuitState::Open);
    assert_eq!(semaphore.available_permits(), 1);
  }

  /// 场景：用户取消导致的流终结。
  ///
  /// 预期：不推动熔断（状态保持 Closed），配额仍归还。
  #[tokio::test]
  async fn cancelled_error_does_not_trip_breaker() {
    let (semaphore, permit) = permits();
    let breaker = breaker(1);
    let (producer, stream) = ProviderEventStreamProducer::build();
    let mut observed = ObservedStream::new(
      stream,
      permit,
      breaker.clone(),
      Admission::Allow,
      None,
    );

    producer.error(StopReason::Aborted, terminal("cancelled"), Some(CANCELLED));
    let event = observed.next().await.expect("应收到 Error");
    assert!(format!("{event:?}").contains("Error"));

    assert_eq!(breaker.state(), CircuitState::Closed);
    assert_eq!(semaphore.available_permits(), 1);
  }

  /// 场景：内容被提供方安全策略过滤。
  ///
  /// 预期：不视为上游故障，熔断保持 Closed。
  #[tokio::test]
  async fn content_filtered_does_not_trip_breaker() {
    let (_, permit) = permits();
    let breaker = breaker(1);
    let (producer, stream) = ProviderEventStreamProducer::build();
    let mut observed = ObservedStream::new(
      stream,
      permit,
      breaker.clone(),
      Admission::Allow,
      None,
    );

    producer.error(
      StopReason::Error,
      terminal("filtered"),
      Some(CONTENT_FILTERED),
    );
    let event = observed.next().await.expect("应收到 Error");
    assert!(format!("{event:?}").contains("Error"));

    assert_eq!(breaker.state(), CircuitState::Closed);
  }

  /// 场景：配置了空闲超时且上游迟迟不发事件。
  ///
  /// 预期：合成 `TIMEOUT` 终结事件、记录一次失败并回收原流；
  /// 后续再调用 `next()` 返回 `None`。
  #[tokio::test(start_paused = true)]
  async fn idle_timeout_synthesizes_timeout_and_trips_breaker() {
    let (semaphore, permit) = permits();
    let breaker = breaker(1);
    let (_producer, stream) = ProviderEventStreamProducer::build();
    let mut observed = ObservedStream::new(
      stream,
      permit,
      breaker.clone(),
      Admission::Allow,
      Some(Duration::from_millis(10)),
    );

    let event = observed.next().await.expect("空闲超时应合成终结事件");
    assert!(format!("{event:?}").contains("Error"));
    assert!(format!("{event:?}").contains(crate::error::TIMEOUT));

    assert_eq!(breaker.state(), CircuitState::Open);
    assert_eq!(semaphore.available_permits(), 1);
    assert!(observed.next().await.is_none());

    // inner 已被回收，result() 路径应直接以 TIMEOUT 收场。
    let err = observed
      .result()
      .await
      .expect_err("回收后的流应以 TIMEOUT 结束");
    assert_eq!(err.code(), crate::error::TIMEOUT);
  }

  /// 场景：配置空闲超时但事件及时到达。
  ///
  /// 预期：`next()` 的事件分支胜出，不触发空闲超时。
  #[tokio::test]
  async fn next_event_wins_over_idle_timeout() {
    let (semaphore, permit) = permits();
    let breaker = breaker(1);
    let (producer, stream) = ProviderEventStreamProducer::build();
    let mut observed = ObservedStream::new(
      stream,
      permit,
      breaker.clone(),
      Admission::Allow,
      Some(Duration::from_secs(1)),
    );
    producer.done(StopReason::Stop, done_message());

    let event = observed.next().await.expect("应收到 Done");
    assert!(format!("{event:?}").contains("Done"));
    assert_eq!(breaker.state(), CircuitState::Closed);
    assert_eq!(semaphore.available_permits(), 1);
  }

  /// 场景：配置空闲超时但终态及时到达。
  ///
  /// 预期：`result()` 的终态分支胜出，返回最终消息。
  #[tokio::test]
  async fn result_wins_over_idle_timeout() {
    let (_, permit) = permits();
    let breaker = breaker(1);
    let (producer, stream) = ProviderEventStreamProducer::build();
    let observed = ObservedStream::new(
      stream,
      permit,
      breaker.clone(),
      Admission::Allow,
      Some(Duration::from_secs(1)),
    );
    producer.done(StopReason::Stop, done_message());

    let message = observed.result().await.expect("应及时返回终态");

    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(breaker.state(), CircuitState::Closed);
  }

  /// 场景：`result()` 路径下生产端未终结即被丢弃。
  ///
  /// 预期：返回 STREAM_INTERRUPTED，并记录一次熔断失败。
  #[tokio::test]
  async fn result_path_dropped_producer_marks_failure() {
    let (_, permit) = permits();
    let breaker = breaker(1);
    let (producer, stream) = ProviderEventStreamProducer::build();
    let observed = ObservedStream::new(
      stream,
      permit,
      breaker.clone(),
      Admission::Allow,
      None,
    );
    drop(producer);

    let err = observed.result().await.expect_err("未终结的流应报错");

    assert_eq!(err.code(), STREAM_INTERRUPTED);
    assert_eq!(breaker.state(), CircuitState::Open);
  }

  /// 场景：调用方直接 `result()` 等待终态且流成功结束。
  ///
  /// 预期：返回最终消息、熔断记录成功、配额归还。
  #[tokio::test]
  async fn result_path_records_success() {
    let (semaphore, permit) = permits();
    let breaker = breaker(1);
    let (producer, stream) = ProviderEventStreamProducer::build();
    let observed = ObservedStream::new(
      stream,
      permit,
      breaker.clone(),
      Admission::Allow,
      None,
    );

    producer.done(StopReason::Stop, done_message());
    let message = observed.result().await.expect("成功流应返回最终消息");

    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(breaker.state(), CircuitState::Closed);
    assert_eq!(semaphore.available_permits(), 1);
  }

  /// 场景：调用方直接 `result()` 且流以失败终结。
  ///
  /// 预期：终态消息标记为 `Error`、熔断记录失败。
  #[tokio::test]
  async fn result_path_records_failure() {
    let (_, permit) = permits();
    let breaker = breaker(1);
    let (producer, stream) = ProviderEventStreamProducer::build();
    let observed = ObservedStream::new(
      stream,
      permit,
      breaker.clone(),
      Admission::Allow,
      None,
    );

    producer.error(StopReason::Error, terminal("boom"), Some(SERVER_ERROR));
    let message = observed.result().await.expect("终态消息应返回");

    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(breaker.state(), CircuitState::Open);
  }

  /// 场景：调用方直接 `result()` 且流因取消终结。
  ///
  /// 预期：终态消息标记为 `Aborted`，熔断保持 Closed。
  #[tokio::test]
  async fn result_path_cancel_does_not_trip_breaker() {
    let (_, permit) = permits();
    let breaker = breaker(1);
    let (producer, stream) = ProviderEventStreamProducer::build();
    let observed = ObservedStream::new(
      stream,
      permit,
      breaker.clone(),
      Admission::Allow,
      None,
    );

    producer.error(
      StopReason::Aborted,
      AssistantMessage::terminal(
        "p1",
        "m1",
        StopReason::Aborted,
        "cancelled",
        0,
      ),
      Some(CANCELLED),
    );
    let message = observed.result().await.expect("终态消息应返回");

    assert_eq!(message.stop_reason, StopReason::Aborted);
    assert_eq!(breaker.state(), CircuitState::Closed);
  }

  /// 场景：先经 `next()` 读到终结事件，再调用 `result()`。
  ///
  /// 预期：熔断只回报一次——阈值 2 时一次失败不应打开熔断。
  #[tokio::test]
  async fn terminal_is_reported_only_once() {
    let (_, permit) = permits();
    let breaker = breaker(2);
    let (producer, stream) = ProviderEventStreamProducer::build();
    let mut observed = ObservedStream::new(
      stream,
      permit,
      breaker.clone(),
      Admission::Allow,
      None,
    );

    producer.error(StopReason::Error, terminal("boom"), Some(SERVER_ERROR));
    let event = observed.next().await.expect("应收到 Error");
    assert!(format!("{event:?}").contains("Error"));
    assert_eq!(breaker.state(), CircuitState::Closed, "首次失败未达阈值");

    let message = observed.result().await.expect("终态消息应返回");
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(
      breaker.state(),
      CircuitState::Closed,
      "result 不应重复回报同一次失败"
    );
  }

  /// 场景：`result()` 路径下等待终态超时。
  ///
  /// 预期：返回 TIMEOUT 错误并记录失败。
  #[tokio::test(start_paused = true)]
  async fn result_path_idle_timeout() {
    let (_, permit) = permits();
    let breaker = breaker(1);
    let (_producer, stream) = ProviderEventStreamProducer::build();
    let observed = ObservedStream::new(
      stream,
      permit,
      breaker.clone(),
      Admission::Allow,
      Some(Duration::from_millis(10)),
    );

    let err = observed.result().await.expect_err("等待终态应超时");
    assert_eq!(err.code(), crate::error::TIMEOUT);
    assert_eq!(breaker.state(), CircuitState::Open);
  }
}
