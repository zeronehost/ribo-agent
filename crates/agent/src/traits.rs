//! Agent 边界 trait：门面、工具、上下文、会话与事件出口。
//!
//! 全部 `#[async_trait]` 且对象安全，供跨任务 `Arc<dyn _>` 共享；
//! 循环体只依赖 trait，具体实现见 `agent.rs`（门面）、`adapter.rs`
//! （工具）、`session.rs`（会话表）、`emit.rs`（事件出口）。

use std::{sync::Arc, time::Duration};

use ra_core::{
  error::RiboResult,
  message::{Content, Message, ToolCall, ToolSchema, UserContent, UserMessage},
  utils::time::now_ms,
};
use ra_tools::{
  context::ToolContext,
  types::{ToolExecutionMode, ToolExecutionResult},
};
use tokio::sync::broadcast;

use crate::{events::AgentEvent, session::Session, types::AgentContext};

/// Agent 门面契约：提交消息、取消、活动状态与事件订阅。
///
/// 生产实现为 `BaseAgent`（`agent.rs`）。
#[async_trait::async_trait]
pub trait Agent: Send + Sync {
  /// 提交一条用户消息并驱动循环跑完，返回本次新增消息。
  ///
  /// # Errors
  ///
  /// 会话创建失败、会话忙（重入拒绝）、上下文构建失败或迭代超限时
  /// 返回 `agent.*` 域错误；工具执行失败不产生错误（以
  /// `is_error: true` 结果回灌）。
  async fn prompt(
    &self,
    session_key: &str,
    text: &str,
  ) -> RiboResult<Vec<Message>>;

  /// 请求取消指定会话的当前运行；返回是否存在活动运行。
  async fn stop(&self, session_key: &str) -> bool;

  /// 指定会话是否有活动运行。
  async fn has_active_tasks(&self, session_key: &str) -> bool;

  /// 订阅生命周期事件（broadcast 扇出，多订阅方各自持副本）。
  fn subscribe(&self) -> broadcast::Receiver<AgentEvent>;
}

/// 工具边界：循环调度的最小接口（生产实现见 `adapter.rs`）。
#[async_trait::async_trait]
pub trait ToolProvider: Send + Sync {
  /// 当前可用工具声明（按名升序，由 ra-tools 保证）。
  async fn get_tool_schemas(&self) -> Vec<ToolSchema>;

  /// 执行一次工具调用；`Err` 的错误码决定回灌与批终止语义。
  ///
  /// # Errors
  ///
  /// 返回 `tool.*` 域错误码：`tool.cancelled` 触发批量终止
  /// （其余并行调用随子令牌级联中断）；其余错误以 `is_error: true`
  /// 结果回灌，循环继续（模型自我修正）。
  async fn execute(
    &self,
    call: ToolCall,
    ctx: ToolContext,
  ) -> RiboResult<ToolExecutionResult>;

  /// 指定工具的执行模式（并行/顺序），未知工具返回顺序（保守默认）。
  fn execution_mode(&self, name: &str) -> ToolExecutionMode;
}

/// 上下文构建边界（TODO_02 `ContextProvider` 收敛版）。
#[async_trait::async_trait]
pub trait ContextProvider: Send + Sync {
  /// 由会话转录 + 本轮输入消息构建请求前上下文。
  ///
  /// # Errors
  ///
  /// 构建失败（上下文装配错误等）返回 `agent.context_build_error`。
  async fn build(
    &self,
    session: &Session,
    input: Vec<Message>,
  ) -> RiboResult<AgentContext>;
}

/// 会话容器边界（TODO_02 `SessionProvider` 的 ra 化）。
#[async_trait::async_trait]
pub trait SessionProvider: Send + Sync {
  /// 取会话，不存在则创建。
  ///
  /// # Errors
  ///
  /// 创建失败返回 `agent.session_create_failed`。
  async fn get_or_create(&self, key: &str) -> RiboResult<Arc<Session>>;

  /// 取会话（不存在返回 `None`，调用方转 `agent.session_not_found`）。
  async fn get(&self, key: &str) -> Option<Arc<Session>>;

  /// 移除会话；返回是否确有删除。
  async fn remove(&self, key: &str) -> bool;

  /// 列出现存会话键。
  async fn list(&self) -> Vec<String>;

  /// 清理空闲超过 `idle_after` 的非活动会话，返回清理数。
  async fn cleanup_expired(&self, idle_after: Duration) -> usize;
}

/// 循环向外的唯一事件出口（pi-rust 同名 trait 的 async-trait 版）。
#[async_trait::async_trait]
pub trait AgentEmitter: Send + Sync {
  /// 异步发射：循环按调用序 await，保证事件顺序与发射顺序一致。
  async fn emit(&self, event: AgentEvent);

  /// 同步非阻塞发射：供工具 `on_update` 闭包（`&dyn Fn` 内不可 await）。
  /// 默认空实现；顺序由「工具执行期内独占调用」保证。
  fn try_emit(&self, _event: AgentEvent) {}
}

/// 直通上下文实现：转录 + 输入原样透传。
///
/// 接入压缩、system prompt 模板与检索前的默认实现；
/// 可携带固定系统提示（多模态，空表示无）。
#[derive(Default)]
pub struct PassthroughContextProvider {
  system_prompt: Vec<Content>,
}

impl PassthroughContextProvider {
  /// 创建直通实现，携带固定系统提示（空表示无）。
  pub fn new(system_prompt: Vec<Content>) -> Self {
    Self { system_prompt }
  }
}

#[async_trait::async_trait]
impl ContextProvider for PassthroughContextProvider {
  /// 直通构建：会话快照在前、本轮输入在后。
  ///
  /// # Errors
  ///
  /// 直通实现不产生错误，恒返回 [`Ok`]。
  async fn build(
    &self,
    session: &Session,
    input: Vec<Message>,
  ) -> RiboResult<AgentContext> {
    let mut messages = session.snapshot();
    messages.extend(input);
    Ok(AgentContext::new(
      self.system_prompt.clone(),
      messages,
      false,
      false,
    ))
  }
}

/// 便捷构造：单段文本的用户消息（时间戳取当前毫秒）。
///
/// # Examples
///
/// ```
/// use ra_agent::traits::user_text_message;
///
/// let message = user_text_message("hello");
/// assert!(matches!(
///   message,
///   ra_core::message::Message::User(_)
/// ));
/// ```
pub fn user_text_message(text: impl Into<String>) -> Message {
  Message::from(UserMessage {
    content: UserContent::Text(text.into()),
    timestamp: now_ms(),
  })
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use ra_core::message::{Content, Message, TextContent, UserContent};

  use super::*;
  use crate::session::Session;

  /// 取 User 消息的文本载荷（断言辅助）。
  fn user_text(message: &Message) -> String {
    match message {
      Message::User(user) => match &user.content {
        UserContent::Text(text) => text.clone(),
        other => panic!("应为纯文本内容，实际 {other:?}"),
      },
      other => panic!("应为 User 消息，实际 {other:?}"),
    }
  }

  // 场景：user_text_message 便捷构造。
  // 预期：产出 User 消息、文本载荷一致、时间戳为当前毫秒。
  #[test]
  fn user_text_message_builds_user_variant() {
    let message = user_text_message("hello");
    let Message::User(user) = &message else {
      panic!("user_text_message 应产出 User 消息");
    };

    assert_eq!(user.timestamp, now_ms(), "时间戳应取当前毫秒");
    assert_eq!(user_text(&message), "hello");
  }

  // 场景：AgentEmitter 的 try_emit 默认实现（空操作）。
  // 预期：仅实现 emit 的最小实现可直接调用默认 try_emit，不 panic。
  #[tokio::test]
  async fn emitter_default_try_emit_is_noop() {
    struct MinimalEmitter;

    #[async_trait::async_trait]
    impl AgentEmitter for MinimalEmitter {
      async fn emit(&self, _event: AgentEvent) {}
    }

    let emitter: Arc<dyn AgentEmitter> = Arc::new(MinimalEmitter);
    emitter.try_emit(AgentEvent::AgentStart);
    emitter.emit(AgentEvent::AgentStart).await;
  }

  // 场景：直通上下文构建的排列与系统提示透传。
  // 预期：会话快照在前、本轮输入在后；系统提示块原样携带。
  #[tokio::test]
  async fn passthrough_build_orders_snapshot_then_input() {
    let session = Session::new("s1");
    session.append_messages(&[user_text_message("history")]);
    let provider =
      PassthroughContextProvider::new(vec![Content::Text(TextContent {
        content: "system".to_string(),
        signature: None,
      })]);

    let context = provider
      .build(&session, vec![user_text_message("input")])
      .await
      .expect("直通构建不产生错误");

    assert_eq!(context.system_prompt.len(), 1);
    let texts: Vec<String> = context.messages.iter().map(user_text).collect();
    assert_eq!(texts, vec!["history".to_string(), "input".to_string()]);
  }

  // 场景：PassthroughContextProvider::default 且输入为空。
  // 预期：系统提示为空、构建结果为空（恒 Ok 的直通语义）。
  #[tokio::test]
  async fn passthrough_default_has_empty_system_prompt() {
    let session = Session::new("s1");
    let provider = PassthroughContextProvider::default();

    let context = provider
      .build(&session, Vec::new())
      .await
      .expect("直通构建不产生错误");

    assert!(context.system_prompt.is_empty());
    assert!(context.messages.is_empty());
  }

  // 场景：ContextProvider 的对象安全性（Arc<dyn _> 跨任务共享）。
  // 预期：trait 对象调用与具体类型行为一致。
  #[tokio::test]
  async fn context_provider_is_object_safe() {
    let session = Session::new("s1");
    let provider: Arc<dyn ContextProvider> =
      Arc::new(PassthroughContextProvider::default());

    let context = provider
      .build(&session, vec![user_text_message("hi")])
      .await
      .expect("直通构建不产生错误");

    assert_eq!(context.messages.len(), 1);
  }
}
