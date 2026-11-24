//! 事件出口实现：广播扇出与测试收集器。
//!
//! - [`BroadcastEmitter`]：`tokio::sync::broadcast` 扇出，供多订阅方
//!   观测循环生命周期；
//! - [`CollectorEmitter`]：存全量事件快照，供测试断言事件序列。

use std::sync::Mutex;

use tokio::sync::broadcast;

use crate::{events::AgentEvent, traits::AgentEmitter};

/// 广播实现：`tokio::sync::broadcast` 扇出，绝不反压循环。
///
/// 发射语义由 broadcast 通道保证：缓冲区满时自动覆盖最旧事件
/// （订阅方收到 `Lagged`，属订阅方自担）；无订阅者时事件被丢弃并记
/// debug 日志。
#[derive(Debug, Clone)]
pub struct BroadcastEmitter {
  tx: broadcast::Sender<AgentEvent>,
}

impl BroadcastEmitter {
  /// 创建广播出口并返回订阅端；`buffer` 建议不小于 512。
  pub fn new(buffer: usize) -> (Self, broadcast::Receiver<AgentEvent>) {
    let (tx, rx) = broadcast::channel(buffer);
    (Self { tx }, rx)
  }

  /// 追加订阅（多订阅方各自持独立副本）。
  pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
    self.tx.subscribe()
  }

  /// 统一发射：无订阅者时静默丢弃（仅记 debug），不影响循环。
  fn send(&self, event: AgentEvent) {
    if let Err(err) = self.tx.send(event) {
      tracing::debug!(tag = err.0.type_tag(), "event dropped: no subscribers");
    }
  }
}

#[async_trait::async_trait]
impl AgentEmitter for BroadcastEmitter {
  async fn emit(&self, event: AgentEvent) {
    self.send(event);
  }

  fn try_emit(&self, event: AgentEvent) {
    self.send(event);
  }
}

/// 收集器实现：`Mutex<Vec<AgentEvent>>` 存全量，供测试断言事件序列。
#[derive(Debug, Default)]
pub struct CollectorEmitter {
  events: Mutex<Vec<AgentEvent>>,
}

impl CollectorEmitter {
  /// 创建空收集器。
  pub fn new() -> Self {
    Self::default()
  }

  /// 取当前已收集事件的克隆快照。
  ///
  /// # Panics
  ///
  /// 内部锁中毒时 panic（持有方 panic 时才会发生）。
  pub fn snapshot(&self) -> Vec<AgentEvent> {
    self.events.lock().expect("collector lock poisoned").clone()
  }

  /// 取当前已收集事件的类型标签序列（测试断言用）。
  ///
  /// # Panics
  ///
  /// 内部锁中毒时 panic（持有方 panic 时才会发生）。
  pub fn tags(&self) -> Vec<&'static str> {
    self.snapshot().iter().map(AgentEvent::type_tag).collect()
  }

  /// 清空已收集事件（长测试分段断言用）。
  ///
  /// # Panics
  ///
  /// 内部锁中毒时 panic（持有方 panic 时才会发生）。
  pub fn clear(&self) {
    self.events.lock().expect("collector lock poisoned").clear();
  }
}

#[async_trait::async_trait]
impl AgentEmitter for CollectorEmitter {
  async fn emit(&self, event: AgentEvent) {
    self
      .events
      .lock()
      .expect("collector lock poisoned")
      .push(event);
  }

  fn try_emit(&self, event: AgentEvent) {
    self
      .events
      .lock()
      .expect("collector lock poisoned")
      .push(event);
  }
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use tokio::sync::broadcast;

  use super::*;

  // 场景：BroadcastEmitter 广播到全部订阅端。
  // 预期：emit 与 try_emit 的事件均可在每个订阅端按序接收。
  #[tokio::test]
  async fn broadcast_reaches_all_subscribers() {
    let (emitter, mut first) = BroadcastEmitter::new(16);
    let mut second = emitter.subscribe();

    emitter.try_emit(AgentEvent::AgentStart);
    emitter.emit(AgentEvent::TurnStart).await;

    assert!(matches!(first.try_recv(), Ok(AgentEvent::AgentStart)));
    assert!(matches!(first.try_recv(), Ok(AgentEvent::TurnStart)));
    assert!(matches!(second.try_recv(), Ok(AgentEvent::AgentStart)));
    assert!(matches!(second.try_recv(), Ok(AgentEvent::TurnStart)));
  }

  // 场景：无订阅者（订阅端已丢弃）时发射。
  // 预期：静默丢弃不 panic；重新订阅后仍可接收后续事件。
  #[tokio::test]
  async fn broadcast_without_subscribers_does_not_panic() {
    let (emitter, rx) = BroadcastEmitter::new(16);
    drop(rx);

    emitter.try_emit(AgentEvent::AgentStart);
    emitter
      .emit(AgentEvent::AgentEnd {
        messages: Vec::new(),
      })
      .await;

    let mut late = emitter.subscribe();
    emitter.try_emit(AgentEvent::TurnStart);
    assert!(matches!(late.try_recv(), Ok(AgentEvent::TurnStart)));
  }

  // 场景：BroadcastEmitter 克隆共享同一通道（BaseAgent 每次运行克隆出口）。
  // 预期：克隆体发射的事件在原订阅端可见。
  #[tokio::test]
  async fn cloned_broadcast_emitter_shares_channel() {
    let (emitter, mut rx) = BroadcastEmitter::new(16);
    let clone = emitter.clone();

    clone.try_emit(AgentEvent::AgentStart);
    assert!(matches!(rx.try_recv(), Ok(AgentEvent::AgentStart)));
  }

  // 场景：缓冲区满载时最旧事件被覆盖。
  // 预期：容量 2 内连续发射 4 个事件后，订阅端 try_recv 先返回 Lagged。
  #[tokio::test]
  async fn broadcast_lags_when_buffer_overflows() {
    let (emitter, mut rx) = BroadcastEmitter::new(2);
    for _ in 0..4 {
      emitter.try_emit(AgentEvent::TurnStart);
    }

    assert!(matches!(
      rx.try_recv(),
      Err(broadcast::error::TryRecvError::Lagged(_))
    ));
  }

  // 场景：CollectorEmitter 的收集、快照、标签与清空。
  // 预期：emit/try_emit 均入集；tags 输出类型标签序列；clear 后为空。
  #[tokio::test]
  async fn collector_records_snapshots_tags_and_clears() {
    let collector = Arc::new(CollectorEmitter::new());
    let emitter: Arc<dyn AgentEmitter> = collector.clone();

    emitter.emit(AgentEvent::AgentStart).await;
    emitter.try_emit(AgentEvent::TurnStart);

    assert_eq!(collector.snapshot().len(), 2);
    assert_eq!(collector.tags(), vec!["agent_start", "turn_start"]);
    collector.clear();
    assert!(collector.snapshot().is_empty());
    assert!(collector.tags().is_empty());
  }

  // 场景：CollectorEmitter::default 与 new 等价。
  // 预期：初始为空（无预置事件）。
  #[test]
  fn collector_default_is_empty() {
    let collector = CollectorEmitter::default();

    assert!(collector.snapshot().is_empty());
    assert!(collector.tags().is_empty());
  }
}
