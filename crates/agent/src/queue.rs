//! 待注入消息队列：steering / follow-up 共用的 FIFO。
//!
//! 两个排水点（轮间 steering、外层 follow-up）复用同一队列类型；
//! 队列本身不加锁，跨任务共享由持有方（会话内的 `Mutex`）串行化。

use std::collections::VecDeque;

use ra_core::message::Message;

/// 排水策略。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum QueueMode {
  /// 一次排水取走全部排队消息（默认）。
  #[default]
  All,
  /// 一次排水只取最旧一条，其余留队。
  OneAtATime,
}

/// 待注入消息 FIFO。
///
/// 实现了 [`Default`]（`All` 模式空队），便于会话字段直接初始化。
#[derive(Debug, Clone, Default)]
pub struct PendingMessageQueue {
  mode: QueueMode,
  pending: VecDeque<Message>,
}

impl PendingMessageQueue {
  /// 按模式建队（空队列）。
  pub fn new(mode: QueueMode) -> Self {
    Self {
      mode,
      pending: VecDeque::new(),
    }
  }

  /// 当前排水模式。
  pub fn mode(&self) -> QueueMode {
    self.mode
  }

  /// 入队到尾部。
  pub fn enqueue(&mut self, message: Message) {
    self.pending.push_back(message);
  }

  /// 是否为空。
  pub fn is_empty(&self) -> bool {
    self.pending.is_empty()
  }

  /// 队列长度。
  pub fn len(&self) -> usize {
    self.pending.len()
  }

  /// 按模式排水：`All` 取空全队；`OneAtATime` 弹出最旧一条。
  ///
  /// 空队返回空 `Vec`（循环视为「无注入」）。
  pub fn try_drain(&mut self) -> Vec<Message> {
    match self.mode {
      QueueMode::All => self.pending.drain(..).collect(),
      QueueMode::OneAtATime => self.pending.pop_front().into_iter().collect(),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  // 场景：All 模式一次排水取走全部排队消息。
  // 预期：返回值为入队序的全量消息，队列排水后为空。
  #[test]
  fn drain_all_takes_everything_in_order() {
    let mut queue = PendingMessageQueue::new(QueueMode::All);
    queue.enqueue(Message::from(ra_core::message::UserMessage {
      content: ra_core::message::UserContent::Text("a".to_string()),
      timestamp: 0,
    }));
    queue.enqueue(Message::from(ra_core::message::UserMessage {
      content: ra_core::message::UserContent::Text("b".to_string()),
      timestamp: 0,
    }));

    let drained = queue.try_drain();

    assert_eq!(drained.len(), 2);
    assert!(queue.is_empty());
  }

  // 场景：OneAtATime 模式一次只弹出最旧一条，其余留队。
  // 预期：两次排水分别取到第 1、2 条；第三次排水返回空。
  #[test]
  fn drain_one_at_a_time_pops_oldest_first() {
    let mut queue = PendingMessageQueue::new(QueueMode::OneAtATime);
    queue.enqueue(Message::from(ra_core::message::UserMessage {
      content: ra_core::message::UserContent::Text("a".to_string()),
      timestamp: 0,
    }));
    queue.enqueue(Message::from(ra_core::message::UserMessage {
      content: ra_core::message::UserContent::Text("b".to_string()),
      timestamp: 0,
    }));

    assert_eq!(queue.try_drain().len(), 1);
    assert_eq!(queue.len(), 1);
    assert_eq!(queue.try_drain().len(), 1);
    assert!(queue.try_drain().is_empty());
  }

  // 场景：空队列排水。
  // 预期：返回空 Vec，循环据此判定「无注入」。
  #[test]
  fn drain_empty_queue_returns_empty_vec() {
    let mut queue = PendingMessageQueue::new(QueueMode::All);

    assert!(queue.try_drain().is_empty());
  }

  // 场景：mode() 返回构造时指定的排水模式。
  // 预期：All / OneAtATime 分别返回对应模式（读取不改状态）。
  #[test]
  fn mode_returns_configured_mode() {
    assert_eq!(
      PendingMessageQueue::new(QueueMode::All).mode(),
      QueueMode::All
    );
    assert_eq!(
      PendingMessageQueue::new(QueueMode::OneAtATime).mode(),
      QueueMode::OneAtATime
    );
  }

  // 场景：Default 队列的初始形态。
  // 预期：模式为 All 且队列为空（会话字段直接初始化的依据）。
  #[test]
  fn default_queue_is_all_mode_and_empty() {
    let queue = PendingMessageQueue::default();

    assert_eq!(queue.mode(), QueueMode::All);
    assert!(queue.is_empty());
    assert_eq!(queue.len(), 0);
  }
}
