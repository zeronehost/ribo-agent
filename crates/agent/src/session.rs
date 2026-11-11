//! 内存会话容器：转录、串行锁、取消根、注入队列与过期清理。
//!
//! 最小实现（JSONL 持久化属阶段 4 ra-harness）；跨会话并行由
//! [`SessionManager`] 的分片锁保证，同会话串行由 [`Session::run_lock`]
//! 与运行标志 `running` 的 CAS 双检查保证。

use std::sync::{
  Arc, Mutex, RwLock,
  atomic::{AtomicBool, AtomicI64, Ordering},
};

use ra_core::{error::RiboResult, message::Message, utils::time::now_ms};

use crate::{
  abort::AbortHandle, queue::PendingMessageQueue, traits::SessionProvider,
};

/// 会话（内存版）：转录 + 串行锁 + 取消根。
pub struct Session {
  /// 会话键（本期 String；强类型身份随阶段 4 引入）。
  pub key: String,
  /// 完整转录（读经 [`Session::snapshot`]，写经 [`Session::append_messages`]）。
  messages: RwLock<Vec<Message>>,
  /// 同会话运行串行锁：循环入口 `try_lock` 探测，与 `running` CAS
  /// 构成双检查（CAS 快速拒绝、锁兜底竞态窗口）。
  pub run_lock: tokio::sync::Mutex<()>,
  /// 本会话取消根（`stop` 触发；运行收尾经 [`Session::refresh_cancel`]
  /// 换发新根——`AbortHandle` 一旦 abort 永久保持触发态，无复位语义）。
  cancel: Mutex<AbortHandle>,
  /// steering 队列：运行中插队消息（轮间注入）。
  pub steering_queue: Arc<Mutex<PendingMessageQueue>>,
  /// follow-up 队列：本应停止后续跑的消息。
  pub follow_up_queue: Arc<Mutex<PendingMessageQueue>>,
  /// 是否有活动运行（CAS 门禁，循环入口置位、收尾复位）。
  pub running: AtomicBool,
  /// 创建时间（Unix 毫秒）。
  pub created_at: i64,
  /// 最近活跃时间（Unix 毫秒，`cleanup_expired` 依据；读经
  /// [`Session::last_active_at`]）。
  last_active_at: AtomicI64,
}

impl core::fmt::Debug for Session {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    // 转录内容不打印（可能很大），只输出条数与状态字段
    f.debug_struct("Session")
      .field("key", &self.key)
      .field(
        "messages",
        &self.messages.read().map(|m| m.len()).unwrap_or(0),
      )
      .field("running", &self.running.load(Ordering::Acquire))
      .field(
        "last_active_at",
        &self.last_active_at.load(Ordering::Acquire),
      )
      .finish()
  }
}

impl Session {
  /// 创建空会话（转录为空、未运行、队列与取消根就绪）。
  pub fn new(key: impl Into<String>) -> Self {
    let now = now_ms();
    Self {
      key: key.into(),
      messages: RwLock::new(Vec::new()),
      run_lock: tokio::sync::Mutex::new(()),
      cancel: Mutex::new(AbortHandle::new()),
      steering_queue: Arc::new(Mutex::new(PendingMessageQueue::default())),
      follow_up_queue: Arc::new(Mutex::new(PendingMessageQueue::default())),
      running: AtomicBool::new(false),
      created_at: now,
      last_active_at: AtomicI64::new(now),
    }
  }

  /// 触发会话取消：当前取消根 abort，级联到全部子令牌（`stop` 路径）。
  pub fn abort_cancel(&self) {
    self.cancel.lock().expect("cancel root lock").abort();
  }

  /// 克隆当前取消根令牌（`LoopConfig::for_session` / 上下文压缩装配用）。
  pub fn cancel_token(&self) -> tokio_util::sync::CancellationToken {
    self
      .cancel
      .lock()
      .expect("cancel root lock")
      .token()
      .clone()
  }

  /// 取消根是否已触发（观测用）。
  pub fn cancel_is_aborted(&self) -> bool {
    self.cancel.lock().expect("cancel root lock").is_aborted()
  }

  /// 换发新取消根：上一次运行的 `stop` 会永久 abort 旧根
  /// （`AbortHandle` 无复位语义），运行收尾时调用保证会话在
  /// `/stop` 之后可继续对话（概设 v2.4 复位三分支的会话侧配套）。
  ///
  /// 并发安全：仅由「已持有 run_lock + running 标志」的运行收尾路径
  /// 调用；外部 `stop` 在无活动运行时是 no-op（`is_running` 检查），
  /// 不会与新根产生竞态。
  pub fn refresh_cancel(&self) {
    *self.cancel.lock().expect("cancel root lock") = AbortHandle::new();
  }

  /// 批量追加转录并刷新活跃时间（空切片直接返回）。
  ///
  /// # Panics
  ///
  /// 转录读写锁中毒时 panic（持有方 panic 时才会发生）。
  pub fn append_messages(&self, msgs: &[Message]) {
    if msgs.is_empty() {
      return;
    }
    self
      .messages
      .write()
      .expect("session transcript lock poisoned")
      .extend(msgs.iter().cloned());
    self.touch();
  }

  /// 取转录快照（全量克隆；大转录场景由调用方承担拷贝成本）。
  ///
  /// # Panics
  ///
  /// 转录读写锁中毒时 panic（持有方 panic 时才会发生）。
  pub fn snapshot(&self) -> Vec<Message> {
    self
      .messages
      .read()
      .expect("session transcript lock poisoned")
      .clone()
  }

  /// 刷新最近活跃时间（每次写入转录后调用）。
  pub fn touch(&self) {
    self.last_active_at.store(now_ms(), Ordering::Release);
  }

  /// 最近活跃时间（Unix 毫秒）。
  pub fn last_active_at(&self) -> i64 {
    self.last_active_at.load(Ordering::Acquire)
  }

  /// 向 steering 队列插队一条消息（运行中生效，轮间注入）。
  ///
  /// # Panics
  ///
  /// 队列锁中毒时 panic（持有方 panic 时才会发生）。
  pub fn enqueue_steering(&self, message: Message) {
    self
      .steering_queue
      .lock()
      .expect("steering queue lock poisoned")
      .enqueue(message);
  }

  /// 向 follow-up 队列排入一条消息（本次运行停止后续跑）。
  ///
  /// # Panics
  ///
  /// 队列锁中毒时 panic（持有方 panic 时才会发生）。
  pub fn enqueue_follow_up(&self, message: Message) {
    self
      .follow_up_queue
      .lock()
      .expect("follow-up queue lock poisoned")
      .enqueue(message);
  }

  /// 队列是否仍有待注入消息（steering 或 follow-up 任一非空）。
  ///
  /// # Panics
  ///
  /// 队列锁中毒时 panic（持有方 panic 时才会发生）。
  pub fn has_queued_messages(&self) -> bool {
    let steering = self
      .steering_queue
      .lock()
      .expect("steering queue lock poisoned");
    let follow_up = self
      .follow_up_queue
      .lock()
      .expect("follow-up queue lock poisoned");
    !steering.is_empty() || !follow_up.is_empty()
  }

  /// 是否有活动运行（同步快照）。
  pub fn is_running(&self) -> bool {
    self.running.load(Ordering::Acquire)
  }
}

/// 会话管理器（[`SessionProvider`] 的生产实现，内存版）。
pub struct SessionManager {
  sessions: dashmap::DashMap<String, Arc<Session>>,
}

// 无参 new 必须配 Default（clippy new_without_default）
impl Default for SessionManager {
  fn default() -> Self {
    Self {
      sessions: dashmap::DashMap::new(),
    }
  }
}

impl SessionManager {
  /// 创建空会话表。
  pub fn new() -> Self {
    Self::default()
  }
}

#[async_trait::async_trait]
impl SessionProvider for SessionManager {
  /// 取/建会话：先快速路径读，未命中再 `entry` 原子插入。
  ///
  /// fast-path 读优化（减少写锁竞争）；并发首建由 DashMap `entry`
  /// 的键级互斥保证只创建一个实例。
  ///
  /// # Errors
  ///
  /// 内存实现不产生错误，恒返回 [`Ok`]。
  async fn get_or_create(&self, key: &str) -> RiboResult<Arc<Session>> {
    // 快速路径：已存在直接返回，避免走 entry 写锁
    if let Some(session) = self.sessions.get(key) {
      return Ok(Arc::clone(session.value()));
    }
    let session = self
      .sessions
      .entry(key.to_string())
      .or_insert_with(|| Arc::new(Session::new(key)))
      .value()
      .clone();
    tracing::info!(session_key = %key, "session ready");
    Ok(session)
  }

  /// 取会话（不存在返回 `None`）。
  async fn get(&self, key: &str) -> Option<Arc<Session>> {
    self.sessions.get(key).map(|s| Arc::clone(s.value()))
  }

  /// 移除会话；返回是否确有删除。
  async fn remove(&self, key: &str) -> bool {
    let removed = self.sessions.remove(key).is_some();
    if removed {
      tracing::info!(session_key = %key, "session removed");
    }
    removed
  }

  /// 列出现存会话键（无稳定顺序）。
  async fn list(&self) -> Vec<String> {
    self.sessions.iter().map(|e| e.key().clone()).collect()
  }

  /// 清理空闲超过 `idle_after` 的会话。
  ///
  /// 两段式：先收集过期键（不持写锁），再逐个移除——`remove` 与
  /// `get_or_create` 在 DashMap 键级互斥下并发安全；运行中的会话
  /// （`is_running`）跳过不清理。
  async fn cleanup_expired(&self, idle_after: std::time::Duration) -> usize {
    let now = now_ms();
    let threshold = idle_after.as_millis() as i64;
    // 第一段：快照式收集过期键，避免边遍历边写
    let expired_keys: Vec<String> = self
      .sessions
      .iter()
      .filter(|entry| {
        let session = entry.value();
        !session.is_running() && now - session.last_active_at() > threshold
      })
      .map(|entry| entry.key().clone())
      .collect();
    // 第二段：逐个移除；remove 与 get_or_create 并发安全（键级互斥）
    let mut removed = 0;
    for key in expired_keys {
      if self.sessions.remove(&key).is_some() {
        tracing::info!(session_key = %key, "expired session removed");
        removed += 1;
      }
    }
    removed
  }
}

#[cfg(test)]
mod tests {
  use std::{
    sync::{
      Arc,
      atomic::{AtomicI64, Ordering},
    },
    time::Duration,
  };

  use super::*;

  /// 构造固定时间戳的用户消息（转录整体相等断言用）。
  fn fixed_user_message(text: &str) -> Message {
    Message::from(ra_core::message::UserMessage {
      content: ra_core::message::UserContent::Text(text.to_string()),
      timestamp: 42,
    })
  }

  // 场景：Session::new 初始状态。
  // 预期：键写入、转录为空、未运行、注入队列为空、时间戳为当前毫秒。
  #[test]
  fn new_session_starts_empty_and_idle() {
    let session = Session::new("s1");

    assert_eq!(session.key, "s1");
    assert!(session.snapshot().is_empty());
    assert!(!session.is_running());
    assert!(!session.has_queued_messages());
    assert!(session.created_at > 0);
    assert!(session.last_active_at() > 0);
  }

  // 场景：append_messages 传入空切片。
  // 预期：直接返回，转录保持为空（不触发 touch 的空写路径）。
  #[test]
  fn append_empty_slice_leaves_transcript_unchanged() {
    let session = Session::new("s1");

    session.append_messages(&[]);

    assert!(session.snapshot().is_empty());
  }

  // 场景：批量追加后取快照。
  // 预期：快照与入参整体相等（转录序保持）。
  #[test]
  fn append_then_snapshot_roundtrips() {
    let session = Session::new("s1");
    let messages = vec![fixed_user_message("a"), fixed_user_message("b")];

    session.append_messages(&messages);

    assert_eq!(session.snapshot(), messages);
  }

  // 场景：touch 刷新最近活跃时间。
  // 预期：touch 后 last_active_at 不小于原值。
  #[test]
  fn touch_refreshes_last_active() {
    let session = Session::new("s1");
    let before = session.last_active_at();

    std::thread::sleep(Duration::from_millis(5));
    session.touch();

    assert!(session.last_active_at() >= before);
  }

  // 场景：两个注入队列与 has_queued_messages 的组合。
  // 预期：任一队列非空即 true；分别排空后才恢复 false。
  #[test]
  fn queue_occupancy_reflects_both_queues() {
    let session = Session::new("s1");

    assert!(!session.has_queued_messages());

    session.enqueue_steering(fixed_user_message("s"));
    assert!(session.has_queued_messages());

    session.enqueue_follow_up(fixed_user_message("f"));
    assert!(session.has_queued_messages());

    // 排空 steering 后仍因 follow-up 非空而保持 true
    session
      .steering_queue
      .lock()
      .expect("steering 锁有效")
      .try_drain();
    assert!(session.has_queued_messages());

    session
      .follow_up_queue
      .lock()
      .expect("follow-up 锁有效")
      .try_drain();
    assert!(!session.has_queued_messages());
  }

  // 场景：running 原子标志的置位与复位。
  // 预期：is_running 同步反映标志当前值（CAS 门禁的基础）。
  #[test]
  fn running_flag_roundtrip() {
    let session = Session::new("s1");

    session.running.store(true, Ordering::Release);
    assert!(session.is_running());

    session.running.store(false, Ordering::Release);
    assert!(!session.is_running());
  }

  // 场景：Session 的 Debug 输出。
  // 预期：包含会话键与转录条数；转录内容不得出现（防泄露）。
  #[test]
  fn debug_contains_key_and_message_count() {
    let session = Session::new("s1");
    session.append_messages(&[fixed_user_message("secret-content")]);

    let debug = format!("{session:?}");

    assert!(debug.contains("s1"), "调试输出应含会话键: {debug}");
    assert!(
      debug.contains("messages: 1"),
      "调试输出应含转录条数: {debug}"
    );
    assert!(
      !debug.contains("secret-content"),
      "转录内容不得出现在调试输出"
    );
  }

  // 场景：SessionManager 初始为空表，get_or_create 幂等。
  // 预期：两次取/建返回同一 Arc 实例（指针相等）。
  #[tokio::test]
  async fn get_or_create_is_idempotent() {
    let manager = SessionManager::default();
    assert!(manager.list().await.is_empty());

    let first = manager
      .get_or_create("s1")
      .await
      .expect("内存实现不产生错误");
    let second = manager
      .get_or_create("s1")
      .await
      .expect("内存实现不产生错误");

    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(first.key, "s1");
  }

  // 场景：get 对不存在与已存在会话的行为。
  // 预期：不存在返回 None；已创建返回 Some。
  #[tokio::test]
  async fn get_returns_existing_session_only() {
    let manager = SessionManager::new();

    assert!(manager.get("ghost").await.is_none());
    manager.get_or_create("s1").await.expect("创建应成功");
    assert!(manager.get("s1").await.is_some());
  }

  // 场景：remove 的删除语义。
  // 预期：存在时返回 true 并移除；再次移除返回 false。
  #[tokio::test]
  async fn remove_reports_actual_deletion() {
    let manager = SessionManager::new();
    manager.get_or_create("s1").await.expect("创建应成功");

    assert!(manager.remove("s1").await);
    assert!(!manager.remove("s1").await);
    assert!(manager.get("s1").await.is_none());
  }

  // 场景：list 列出现存会话键。
  // 预期：返回全部键（无稳定顺序，排序后整体比较）。
  #[tokio::test]
  async fn list_returns_all_keys() {
    let manager = SessionManager::new();
    for key in ["s1", "s2", "s3"] {
      manager.get_or_create(key).await.expect("创建应成功");
    }

    let mut keys = manager.list().await;
    keys.sort();

    assert_eq!(keys, vec!["s1", "s2", "s3"]);
  }

  // 场景：cleanup_expired 三分支——过期清除、刚活跃保留、运行中保留。
  // 预期：仅「非运行且空闲超限」的会话被移除，返回移除数 1。
  #[tokio::test]
  async fn cleanup_expired_only_removes_idle_inactive_sessions() {
    let manager = SessionManager::new();

    // ===== 准备：expired 与 running 同批创建，随后一同闲置超限 =====
    manager.get_or_create("expired").await.expect("创建应成功");
    let running = manager.get_or_create("running").await.expect("创建应成功");
    running.running.store(true, Ordering::Release);
    tokio::time::sleep(Duration::from_millis(30)).await;

    // ===== 场景 2：刚活跃（闲置约 0ms，低于阈值） =====
    manager.get_or_create("fresh").await.expect("创建应成功");

    let removed = manager.cleanup_expired(Duration::from_millis(10)).await;

    assert_eq!(removed, 1);
    assert!(manager.get("expired").await.is_none());
    assert!(manager.get("fresh").await.is_some());
    assert!(manager.get("running").await.is_some());
  }

  // 场景：last_active_at 读接口与内部原子字段一致（AtomicI64 语义）。
  // 预期：经 touch 写入后读回同一值（Release/Acquire 配对可见）。
  #[test]
  fn last_active_reads_back_written_value() {
    let session = Session::new("s1");
    let probe = AtomicI64::new(session.last_active_at());

    session.touch();
    probe.store(session.last_active_at(), Ordering::Release);

    assert_eq!(
      session.last_active_at(),
      probe.load(Ordering::Acquire),
      "读写接口应观测同一原子字段"
    );
  }
}
