//! 持久化会话 Provider：内存表 + JSONL 冷启动回放。
//!
//! 运行期读写全部命中内存（[`SessionManager`]）；仅 `get_or_create` 未命中时
//! 做一次磁盘回放；删除 / 清理时联动删除 JSONL 文件与回放锁条目。

use std::{collections::BTreeSet, sync::Arc, time::Duration};

use async_trait::async_trait;
use dashmap::DashMap;
use ra_agent::{
  session::{Session, SessionManager},
  traits::SessionProvider,
};
use ra_core::error::RiboResult;
use tokio::sync::Mutex;

use crate::session_store::JsonlSessionStore;

/// 组合式持久化会话 Provider：内存表 + JSONL 冷启动回放。
///
/// - 运行期读写全部命中内存（`SessionManager`）；
/// - 仅 `get_or_create` 未命中时做一次磁盘回放；
/// - 删除 / 清理时联动删除 JSONL。
pub struct PersistentSessionProvider {
  /// 内存会话表。
  inner: SessionManager,
  /// JSONL 存储。
  store: Arc<JsonlSessionStore>,
  /// 逐键冷启动回放互斥（避免并发首次访问重复回放、重复双写消息）。
  init_locks: DashMap<String, Arc<Mutex<()>>>,
}

impl PersistentSessionProvider {
  /// 以存储实例创建 Provider（内存表为空）。
  pub fn new(store: Arc<JsonlSessionStore>) -> Self {
    Self {
      inner: SessionManager::new(),
      store,
      init_locks: DashMap::new(),
    }
  }

  /// 取（必要时创建）某键的回放锁，返回克隆的 `Arc` 以便跨 await 使用。
  fn init_lock(&self, key: &str) -> Arc<Mutex<()>> {
    self
      .init_locks
      .entry(key.to_string())
      .or_insert_with(|| Arc::new(Mutex::new(())))
      .clone()
  }
}

#[async_trait]
impl SessionProvider for PersistentSessionProvider {
  /// 取会话，不存在则从 JSONL 回放创建。
  ///
  /// 运行期命中内存即零 IO；冷启动走逐键回放锁，保证并发首次访问
  /// 只回放一次、不产生重复消息。
  ///
  /// # Errors
  ///
  /// 回放失败返回 `agent.context_build_error`（继承自 [`JsonlSessionStore::load`]）；
  /// 内存创建失败返回 `agent.session_create_failed`（内存实现恒 `Ok`）。
  async fn get_or_create(&self, key: &str) -> RiboResult<Arc<Session>> {
    // ===== 阶段 1：快速路径（运行期零 IO）=====
    if let Some(session) = self.inner.get(key).await {
      return Ok(session);
    }

    // ===== 阶段 2：逐键串行化冷启动 =====
    // 先在引用结束前克隆 Arc，避免跨 await 持有 DashMap Ref
    let lock = self.init_lock(key);
    let _guard = lock.lock().await;

    // ===== 阶段 3：锁内二次检查（并发首次访问只回放一次）=====
    if let Some(session) = self.inner.get(key).await {
      return Ok(session);
    }

    // ===== 阶段 4：回放并物化内存会话 =====
    let replay = self.store.load(key).await?;
    let session = self.inner.get_or_create(key).await?;
    session.append_messages(&replay.messages);

    if !replay.diagnostics.is_empty() {
      tracing::warn!(
        session_key = %key,
        messages = replay.messages.len(),
        diagnostics = replay.diagnostics.len(),
        "cold start replay completed with diagnostics"
      );
    } else {
      tracing::info!(
        session_key = %key,
        messages = replay.messages.len(),
        "session materialized from jsonl"
      );
    }
    Ok(session)
  }

  /// 取会话（不存在返回 `None`），**不触发回放**（纯读语义）。
  async fn get(&self, key: &str) -> Option<Arc<Session>> {
    self.inner.get(key).await
  }

  /// 移除会话：内存 + 磁盘 + 回放锁联动；返回是否确有删除。
  async fn remove(&self, key: &str) -> bool {
    // ===== 阶段 1：内存移除 =====
    let removed_memory = self.inner.remove(key).await;

    // ===== 阶段 2：磁盘删除（失败只告警，不回滚内存移除）=====
    let existed_on_disk = self.store.exists(key).await;
    if let Err(err) = self.store.delete(key).await {
      tracing::warn!(session_key = %key, error = %err, "delete session file failed");
    }

    // ===== 阶段 3：回收回放锁条目 =====
    // 与并发 get_or_create 的竞态：若回放已在进行，等待者释放后最多重建一次
    // 空会话（此刻文件已删），不会产生重复消息；属可接受边缘场景
    self.init_locks.remove(key);

    let removed = removed_memory || existed_on_disk;
    if removed {
      tracing::info!(session_key = %key, "session removed");
    }
    removed
  }

  /// 列出现存会话键（内存 ∪ 磁盘，已排序）。
  async fn list(&self) -> Vec<String> {
    let mut keys: BTreeSet<String> =
      self.inner.list().await.into_iter().collect();
    match self.store.list_keys().await {
      Ok(disk) => keys.extend(disk),
      Err(err) => tracing::warn!(error = %err, "list session files failed"),
    }
    keys.into_iter().collect()
  }

  /// 清理空闲超过 `idle_after` 的非活动会话，并联动删除磁盘文件。
  ///
  /// 两段式：先 `inner.cleanup_expired`，再以快照差集删文件 + 回收回放锁。
  /// 运行中的会话由 `SessionManager` 跳过；磁盘孤儿文件不在本方法职责内。
  async fn cleanup_expired(&self, idle_after: Duration) -> usize {
    // ===== 阶段 1：清理前快照（用于磁盘差集）=====
    let before: BTreeSet<String> =
      self.inner.list().await.into_iter().collect();

    // ===== 阶段 2：内存清理（运行中会话由 SessionManager 跳过）=====
    let cleaned = self.inner.cleanup_expired(idle_after).await;

    // ===== 阶段 3：差集 = 真正被清理的键 → 删文件 + 回收锁 =====
    if cleaned > 0 {
      let after: BTreeSet<String> =
        self.inner.list().await.into_iter().collect();
      for key in before.difference(&after) {
        if let Err(err) = self.store.delete(key).await {
          tracing::warn!(
            session_key = %key,
            error = %err,
            "cleanup delete session file failed"
          );
        }
        self.init_locks.remove(key);
      }
    }
    cleaned
  }
}

#[cfg(test)]
mod tests {
  use std::{sync::Arc, time::Duration};

  use ra_agent::traits::SessionProvider;
  use ra_core::message::{
    AssistantMessage, Content, Message, TextContent, UserContent,
  };

  use super::*;
  use crate::session_store::JsonlSessionStore;

  /// 构造一条用户文本消息（固定时间戳便于整体相等断言）。
  fn user_msg(text: &str) -> Message {
    Message::User(ra_core::message::UserMessage {
      content: UserContent::Text(text.to_string()),
      timestamp: 1_000,
    })
  }

  /// 构造一条助手文本消息（固定时间戳便于整体相等断言）。
  fn assistant_msg(text: &str) -> Message {
    let mut msg = AssistantMessage::empty("mock", "mock-model", 1_001);
    msg.content = vec![Content::Text(TextContent {
      content: text.to_string(),
      signature: None,
    })];
    Message::Assistant(msg)
  }

  /// SP-01：预写 JSONL → 新 Provider get_or_create 回放成功，内存快照与文件一致。
  #[tokio::test]
  async fn cold_start_materializes_messages() {
    let dir = crate::test_util::test_dir("cold_start_materializes_messages");
    let store = Arc::new(JsonlSessionStore::new(dir));

    // 预写两条消息到磁盘
    let msgs = vec![user_msg("hi"), assistant_msg("hello")];
    store.append("s1", &msgs).await.unwrap();

    // 新建 Provider（内存为空），get_or_create 应回放
    let provider = PersistentSessionProvider::new(store);
    let session = provider.get_or_create("s1").await.unwrap();

    assert_eq!(session.snapshot(), msgs);
  }

  /// SP-02：并发 8 路同名 get_or_create 只回放一次，快照长度等于文件消息数。
  #[tokio::test]
  async fn concurrent_get_or_create_replays_once() {
    let dir =
      crate::test_util::test_dir("concurrent_get_or_create_replays_once");
    let store = Arc::new(JsonlSessionStore::new(dir));

    let msgs = vec![user_msg("a"), assistant_msg("b")];
    store.append("s1", &msgs).await.unwrap();

    let provider = Arc::new(PersistentSessionProvider::new(store));

    // 8 路并发首次访问
    let handles: Vec<_> = (0..8)
      .map(|_| {
        let provider = provider.clone();
        tokio::spawn(async move { provider.get_or_create("s1").await.unwrap() })
      })
      .collect();

    // 手动 await 所有 JoinHandle，避免引入 futures 依赖
    let mut sessions = Vec::with_capacity(handles.len());
    for handle in handles {
      sessions.push(handle.await.unwrap());
    }

    // 全部返回同一 Arc 实例
    let first = &sessions[0];
    for session in &sessions {
      assert!(Arc::ptr_eq(first, session));
    }

    // 快照长度等于文件消息数（无重复）
    assert_eq!(first.snapshot().len(), msgs.len());
  }

  /// SP-03：物化后 remove 删除内存、文件与回放锁；再次 get_or_create 为空会话。
  #[tokio::test]
  async fn remove_deletes_memory_file_and_lock() {
    let dir = crate::test_util::test_dir("remove_deletes_memory_file_and_lock");
    let store = Arc::new(JsonlSessionStore::new(dir));

    store.append("s1", &[user_msg("hi")]).await.unwrap();

    let provider = PersistentSessionProvider::new(store.clone());
    // 先物化（建立内存会话 + 回放锁）
    provider.get_or_create("s1").await.unwrap();

    assert!(provider.remove("s1").await);
    assert!(!store.exists("s1").await);

    // 再次 get_or_create：磁盘已空 → 空会话
    let session = provider.get_or_create("s1").await.unwrap();
    assert!(session.snapshot().is_empty());
  }

  /// SP-04：list 合并内存与磁盘；cleanup_expired 联动删文件。
  #[tokio::test]
  async fn list_unions_memory_and_disk_and_cleanup_deletes_files() {
    let dir = crate::test_util::test_dir(
      "list_unions_memory_and_disk_and_cleanup_deletes_files",
    );
    let store = Arc::new(JsonlSessionStore::new(dir));

    // 磁盘一键（内存未物化）
    store.append("disk-only", &[user_msg("d")]).await.unwrap();

    let provider = PersistentSessionProvider::new(store.clone());
    // 内存一键（也落盘）
    provider.get_or_create("mem-and-disk").await.unwrap();
    store
      .append("mem-and-disk", &[user_msg("m")])
      .await
      .unwrap();

    let keys = provider.list().await;
    assert_eq!(
      keys,
      vec!["disk-only".to_string(), "mem-and-disk".to_string()]
    );

    // idle_after 极小：非活动会话全部过期 → 清理后文件同步删除
    tokio::time::sleep(Duration::from_millis(10)).await;
    let cleaned = provider.cleanup_expired(Duration::from_millis(1)).await;
    assert_eq!(cleaned, 1); // 仅 mem-and-disk 在内存中被清理

    assert!(!store.exists("mem-and-disk").await);
    // disk-only 是磁盘孤儿，不在内存，cleanup 不处理（由 list 暴露）
    assert!(store.exists("disk-only").await);
  }

  /// 补充：get 纯读语义，新 Provider 对磁盘已有文件返回 None。
  #[tokio::test]
  async fn get_does_not_replay() {
    let dir = crate::test_util::test_dir("get_does_not_replay");
    let store = Arc::new(JsonlSessionStore::new(dir));
    store.append("s1", &[user_msg("hi")]).await.unwrap();

    let provider = PersistentSessionProvider::new(store);
    // get 不触发回放，内存未命中 → None
    assert!(provider.get("s1").await.is_none());
  }
}
