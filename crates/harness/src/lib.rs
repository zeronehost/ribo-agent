//! ra-harness：Agent 会话容器。
//!
//! 在 `ra-agent` 外部组合三类能力，不侵入主循环：
//! JSONL 会话持久化、上下文压缩、系统提示词与技能注入。

pub mod compaction;
pub mod compaction_state;
pub mod config;
pub mod context;
pub mod instructions;
pub mod session_provider;
pub mod session_store;
pub mod skills;
pub mod system_prompt;
pub mod test_util;

use std::sync::Arc;

use ra_agent::{
  agent::BaseAgent,
  traits::{SessionProvider, user_text_message},
};
use ra_core::{error::RiboResult, message::Message};

pub use self::compaction::{
  CompactionOutcome, CompactionReason, Compactor, estimate_context_tokens,
};
pub use self::compaction_state::{
  CompactionCache, CompactionState, CompactionStateSnapshot,
  fingerprint_message, locate_checkpoint,
};
pub use self::config::{CompactionConfig, ContextConfig, HarnessConfig};
pub use self::context::HarnessContext;
pub use self::instructions::{
  InstructionRoot, Instructions, MAX_AGENTS_BYTES, load_instructions,
};
pub use self::session_provider::PersistentSessionProvider;
pub use self::session_store::{
  JsonlSessionStore, ReplayDiagnostic, ReplayDiagnosticCode, SessionKeyCodec,
  SessionReplay,
};
pub use self::skills::{
  Skill, SkillDiagnostic, SkillDiagnosticCode, SkillRoot, SkillsLoader,
};
pub use self::system_prompt::{
  SystemPromptBuilder, escape_xml, format_skill_invocation,
};

/// 会话门面：组合 [`BaseAgent`] + 持久化 Provider，并在回合成功后落盘。
///
/// 门面持有 `provider` 与 `store` 两份 `Arc`：`provider` 注入 `BaseAgent`
/// 作为 `SessionProvider`，`store` 用于回合成功后的增量落盘。两者必须指向
/// 同一存储根目录，由 [`Harness::new`] 的装配方保证。
pub struct Harness {
  agent: BaseAgent,
  provider: Arc<PersistentSessionProvider>,
  store: Arc<JsonlSessionStore>,
}

impl Harness {
  /// 组装门面。
  ///
  /// `provider` 与 `store` 必须指向同一存储根目录；两处都持有 `Arc` 以避免
  /// 门面与 `BaseAgent` 生命周期不一致。
  pub fn new(
    agent: BaseAgent,
    provider: Arc<PersistentSessionProvider>,
    store: Arc<JsonlSessionStore>,
  ) -> Self {
    Self {
      agent,
      provider,
      store,
    }
  }

  /// 订阅生命周期事件流。
  ///
  /// 透传 [`BaseAgent::subscribe`](ra_agent::agent::BaseAgent::subscribe)
  /// （`Agent` trait 方法）：REPL 等宿主由此获得 `AgentEvent` 广播接收器，
  /// 无须感知 `BaseAgent`。
  ///
  /// # 返回
  ///
  /// `broadcast::Receiver<AgentEvent>`；同一 Harness 可多次调用，
  /// 各接收器独立（broadcast 多消费者语义）。
  pub fn subscribe(
    &self,
  ) -> tokio::sync::broadcast::Receiver<ra_agent::events::AgentEvent> {
    use ra_agent::traits::Agent;
    self.agent.subscribe()
  }

  /// 发送一条用户文本并驱动完整回合。
  ///
  /// # Returns
  ///
  /// 本次**新增**消息（含输入、助手输出与工具结果）。
  pub async fn prompt(
    &self,
    key: &str,
    text: &str,
  ) -> RiboResult<Vec<Message>> {
    self
      .prompt_messages(key, vec![user_text_message(text)])
      .await
  }

  /// 发送一批消息并驱动完整回合（落盘只写 `new_messages`）。
  ///
  /// `LoopReport.new_messages` 已包含本轮输入，因此落盘绝不再次拼接入参，
  /// 否则冷启动回放会重复 user 轮。
  pub async fn prompt_messages(
    &self,
    key: &str,
    messages: Vec<Message>,
  ) -> RiboResult<Vec<Message>> {
    let new_messages = self.agent.prompt_messages(key, messages).await?;
    self.store.append(key, &new_messages).await?;
    Ok(new_messages)
  }

  /// 续跑（无新输入）。
  ///
  /// 冷启动必须先物化会话：`BaseAgent::continue_run` 内部走 `sessions.get`，
  /// 未知键会返回 `agent.session_not_found`。
  pub async fn continue_run(&self, key: &str) -> RiboResult<Vec<Message>> {
    self.provider.get_or_create(key).await?;
    let new_messages = self.agent.continue_run(key).await?;
    self.store.append(key, &new_messages).await?;
    Ok(new_messages)
  }

  /// 读取会话完整转录（冷启动会触发一次回放，但不产生磁盘写入）。
  pub async fn session_messages(&self, key: &str) -> RiboResult<Vec<Message>> {
    Ok(self.provider.get_or_create(key).await?.snapshot())
  }

  /// 读取最近一次上下文装配的压缩三态统计。
  ///
  /// 未装配且未由 estimator 回填（如未绑定估算器的内存 provider）时
  /// 返回 `None`（显示端显示「—」）；持久化 provider 在冷启动回放后
  /// 即有统计值。
  pub async fn session_compaction_stats(
    &self,
    key: &str,
  ) -> Option<ra_agent::session::CompactionStats> {
    self
      .provider
      .get_or_create(key)
      .await
      .ok()?
      .compaction_stats()
  }

  /// 列出全部会话键（内存 ∪ 磁盘，已排序）。
  pub async fn list_sessions(&self) -> Vec<String> {
    self.provider.list().await
  }

  /// 删除会话（内存 + 磁盘 + 回放锁）。
  pub async fn remove_session(&self, key: &str) -> bool {
    self.provider.remove(key).await
  }

  /// 请求取消某会话的当前运行。
  pub async fn stop(&self, key: &str) -> bool {
    self.agent.stop(key).await
  }
}
