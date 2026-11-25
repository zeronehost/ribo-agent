//! 上下文构建边界的 Harness 实现：system prompt + skills + 压缩。
//!
//! 深度集成 [`SystemPromptBuilder`] 与技能集合：
//! 装配期一次性加载技能（v1 无热更新），
//! 每次构建时以「模板 → 环境 → 技能清单 → 自定义段 → 会话摘要」
//! 组装系统提示，摘要来自压缩产出。
//!
//! 取舍说明：`model: Model` 字段未引入——`Compactor`
//! 内部已持有主模型（提供 `context_window` 预算），重复持有会成为
//! 从不读取的死字段；压缩取消令牌改由 `build` 阶段从会话派生。

use std::sync::Arc;

use ra_agent::{
  error::context_build_error, session::Session, traits::ContextProvider,
  types::AgentContext,
};
use ra_core::{
  error::RiboResult,
  message::{Content, Message},
};
use ra_provider::options::CancelToken;

use crate::{
  compaction::{CompactionOutcome, Compactor},
  config::{CompactionConfig, ContextConfig},
  skills::Skill,
  system_prompt::SystemPromptBuilder,
};

/// Harness 上下文装配器。
///
/// 装配顺序（四阶段）：历史 + 本轮输入 → 压缩（可选，产摘要）→
/// 系统提示组装（template + env + skills + summary）→ 产物。
/// 唯一结构化失败是「系统提示为空」；永不 panic。
pub struct HarnessContext {
  /// 系统提示组装器（环境段开关由 `config.include_environment` 覆盖）。
  prompt: SystemPromptBuilder,
  /// 可选压缩器（`None` 表示跳过压缩阶段）。
  compactor: Option<Arc<Compactor>>,
  /// 装配期一次性加载的技能集合（v1 无热更新）。
  skills: Vec<Skill>,
  /// 唯一配置入口：压缩 + env 注入开关。
  config: ContextConfig,
}

impl HarnessContext {
  /// 组装上下文装配器。
  ///
  /// `config.include_environment` 是环境段的唯一真值，构造时覆盖
  /// builder 的同名开关，避免两处配置互相矛盾。
  ///
  /// # Arguments
  ///
  /// - `prompt`：系统提示组装器（模板 / 自定义段在此定制）。
  /// - `compactor`：`None` 时跳过压缩阶段；`config.compaction.enabled`
  ///   独立控制压缩开关。
  /// - `skills`：装配期由 [`SkillsLoader`](crate::skills::SkillsLoader)
  ///   加载的技能集合。
  /// - `config`：压缩 + env 注入配置。
  pub fn new(
    prompt: SystemPromptBuilder,
    compactor: Option<Arc<Compactor>>,
    skills: Vec<Skill>,
    config: ContextConfig,
  ) -> Self {
    let prompt = prompt.with_environment(config.include_environment);
    Self {
      prompt,
      compactor,
      skills,
      config,
    }
  }

  /// 便捷：无压缩、无技能、仅固定模板（环境段按 `include_environment` 开关）。
  ///
  /// 空模板 + 无环境段 + 无技能时 `build` 会返回
  /// `agent.context_build_error`（系统提示为空，见 [`Self::new`] 说明）。
  pub fn passthrough(template: impl Into<String>) -> Self {
    Self::new(
      SystemPromptBuilder::new().with_template(template),
      None,
      Vec::new(),
      ContextConfig {
        compaction: CompactionConfig::default(),
        include_environment: false,
      },
    )
  }
}

#[async_trait::async_trait]
impl ContextProvider for HarnessContext {
  async fn build(
    &self,
    session: &Session,
    input: Vec<Message>,
  ) -> RiboResult<AgentContext> {
    // 拼接历史 + 本轮输入
    // 必须先合并再压缩：若先压缩历史再追加输入，阈值判定会漏掉本轮输入，
    // 且轮切分可能把「输入开头的半轮」当成完整轮。
    let mut messages = session.snapshot();
    messages.extend(input);

    // 压缩（注入会话子取消令牌，联动 stop）
    let outcome = match &self.compactor {
      Some(compactor) => {
        let cancel = Some(CancelToken::from_token(session.cancel_token()));
        compactor
          .compact(&session.key, &messages, &self.config.compaction, cancel)
          .await
      }
      None => {
        if self.config.compaction.enabled {
          tracing::warn!(
            session_key = %session.key,
            "compaction enabled but compactor is None; skipping compaction"
          );
        }
        CompactionOutcome::noop(
          &messages,
          crate::compaction::CompactionReason::Disabled,
        )
      }
    };

    // 系统提示组装（template + env + skills + summary + 提示段）
    let system_prompt = self.prompt.build_compaction(
      &self.skills,
      outcome.summary.as_deref(),
      outcome.fallback_hint,
      outcome.outdated_hint,
    );
    if system_prompt.is_empty() {
      return Err(context_build_error(
        &session.key,
        "system prompt is empty; check template or builder configuration",
      ));
    }

    // system prompt 估算 + 回写 overhead 基准
    // 循环依赖破解：本轮预算用上一轮的 last_overhead（在 compact 内读取），
    // 本轮组装完成的实际 prompt 长度写回，作为下一轮的基准。
    let prompt_tokens = estimate_prompt_tokens(&system_prompt);
    if let Some(compactor) = &self.compactor {
      compactor.set_last_overhead(&session.key, prompt_tokens);
      tracing::debug!(
        session_key = %session.key,
        overhead = prompt_tokens,
        "compaction overhead baseline updated"
      );
    }

    // 压缩三态统计（总量 / 已压缩 / 当前）
    // 总量 = 压缩前完整历史（含本轮输入）；已压缩 = 被移出视图的 prefix；
    // 当前 = 最近请求视图（outcome.messages）+ system prompt（含摘要注入）。
    // 口径统一走 唯一真值估算函数。
    let stats = ra_agent::session::CompactionStats {
      total_tokens: ra_core::message::estimate_context_tokens(&messages),
      compressed_tokens: outcome.compressed_tokens,
      current_tokens: prompt_tokens
        + ra_core::message::estimate_context_tokens(&outcome.messages),
    };
    session.set_compaction_stats(stats);
    tracing::debug!(
      session_key = %session.key,
      total = stats.total_tokens,
      compressed = stats.compressed_tokens,
      current = stats.current_tokens,
      "compaction stats written to session"
    );

    // 产物
    // compaction_triggered / compaction_failed 由压缩原因推导（互斥）；
    // 冷却抑制 / 复用摘要二者皆为 false，不发射事件。
    Ok(AgentContext::new(
      system_prompt,
      outcome.messages,
      outcome.reason == crate::compaction::CompactionReason::Summarized,
      outcome.reason == crate::compaction::CompactionReason::SummaryFailed,
    ))
  }
}

/// system prompt 的 token 估算（唯一真值口径）。
///
/// 图片 / 工具调用占位在系统提示中不出现，保守计 0。
fn estimate_prompt_tokens(system_prompt: &[Content]) -> usize {
  system_prompt
    .iter()
    .map(|content| match content {
      Content::Text(text) => {
        ra_core::message::estimate_tokens_from_chars(&text.content)
      }
      _ => 0,
    })
    .sum()
}

/// 冷启动统计估算（[`SessionStatsEstimator`] 实现）。
///
/// 冷启动尚未发生压缩：总量 = 当前转录估算，已压缩 = 0。
/// 「当前使用量」为 **0**——本次进程尚未发起任何请求，窗口尚未被占用；
/// 首次 `build` 后由真实请求视图覆写（冷启动不预填当前量）。
impl crate::session_provider::SessionStatsEstimator for HarnessContext {
  fn estimate_stats(
    &self,
    messages: &[Message],
  ) -> ra_agent::session::CompactionStats {
    ra_agent::session::CompactionStats {
      total_tokens: ra_core::message::estimate_context_tokens(messages),
      compressed_tokens: 0,
      current_tokens: 0,
    }
  }
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use ra_agent::{
    session::Session,
    traits::{ContextProvider, user_text_message},
  };
  use ra_core::{
    error::RiboError,
    message::{AssistantMessage, Content},
  };
  use ra_provider::{
    adapters::mock::{MockProvider, MockStep},
    gateway::{Gateway, GatewayConfig},
    ids::{ModelId, ProviderId},
    model::Model,
    stream::ProviderEvent,
  };

  use super::*;
  use crate::config::{CompactionConfig, ContextConfig};
  use crate::system_prompt::SystemPromptBuilder;

  /// 构造注册 mock 提供方的网关 + Model（可注入摘要步骤与窗口大小）。
  fn make_compactor_with(
    steps: Vec<MockStep>,
    context_window: u32,
  ) -> Arc<Compactor> {
    let provider_id = ProviderId::parse("mock").unwrap();
    let max_tokens = context_window.clamp(1, 1024);
    let model = Model::new(
      provider_id.clone(),
      ModelId::parse("mock-model").unwrap(),
      "Mock Model",
      "http://localhost",
    )
    .with_context_window(context_window)
    .with_max_tokens(max_tokens);
    let mock =
      Arc::new(MockProvider::new(provider_id, vec![model.clone()], steps));
    let mut gateway = Gateway::new(GatewayConfig::default());
    gateway.register(mock).unwrap();
    Arc::new(Compactor::new(Arc::new(gateway), model))
  }

  /// 无摘要步骤的默认压缩器（仅滑窗回退路径用）。
  fn make_compactor(context_window: u32) -> Arc<Compactor> {
    make_compactor_with(Vec::new(), context_window)
  }

  /// 产出固定文本增量的摘要步骤。
  ///
  /// `MockStep::Complete` 同步路径只推 `Start` + `Done`、不发
  /// `TextDelta`，摘要拼接会得到空文本；必须用 `Events` 回放文本增量，
  /// 终结事件由适配器自动补发。
  fn text_step(text: &str) -> MockStep {
    let t = text.to_string();
    MockStep::Events(vec![ProviderEvent::TextDelta {
      index: 0,
      delta: t,
      partial: Arc::new(AssistantMessage::empty("mock", "mock-model", 0)),
    }])
  }

  fn builder(template: &str) -> SystemPromptBuilder {
    SystemPromptBuilder::new()
      .with_template(template)
      .with_environment(false)
  }

  /// 构造足够多的填充消息让压缩触发。
  fn padded_transcript() -> Vec<Message> {
    (0..10)
      .flat_map(|i| {
        vec![
          user_text_message(format!(
            "q{i} some padding text to inflate char count for budget overflow"
          )),
          user_text_message(format!(
            "a{i} also padded with additional text content here"
          )),
        ]
      })
      .collect()
  }

  /// passthrough 退化为直通，snapshot + input 原样拼接。
  #[tokio::test]
  async fn passthrough_builds_transcript_plus_input() {
    let session = Session::new("s1");
    session.append_messages(&[user_text_message("history")]);

    let provider = HarnessContext::passthrough("system");
    let ctx = provider
      .build(&session, vec![user_text_message("input")])
      .await
      .unwrap();

    assert_eq!(ctx.system_prompt.len(), 1);
    assert_eq!(ctx.messages.len(), 2);
  }

  /// enabled + 有 compactor 触发压缩，messages 被压缩但 system prompt 保留。
  #[tokio::test]
  async fn compaction_enabled_triggers_compactor() {
    let session = Session::new("s1");
    let msgs = padded_transcript();
    session.append_messages(&msgs);

    let compactor = make_compactor(64); // 极小窗口 → 强制触发
    let provider = HarnessContext::new(
      builder("system"),
      Some(compactor),
      Vec::new(),
      ContextConfig {
        compaction: CompactionConfig {
          enabled: true,
          trigger_ratio: 0.75,
          keep_recent_turns: 2,
          summarizer_model: None,
          ..CompactionConfig::default()
        },
        include_environment: false,
      },
    );

    let input = vec![user_text_message("new input")];
    let ctx = provider.build(&session, input).await.unwrap();

    // 压缩后 messages 少于原始 20 条（滑窗保留最近 2 轮 = 4 条 + 本轮 1 条 = 5 条）
    assert!(ctx.messages.len() < msgs.len());
    assert_eq!(ctx.system_prompt.len(), 1);
  }

  /// enabled 但 compactor=None → 降级直通 + 告警日志，messages 不压缩。
  #[tokio::test]
  async fn compaction_enabled_without_compactor_degrades_gracefully() {
    let session = Session::new("s1");
    let msgs = padded_transcript();
    session.append_messages(&msgs);

    let provider = HarnessContext::new(
      builder("system"),
      None,
      Vec::new(),
      ContextConfig {
        compaction: CompactionConfig {
          enabled: true,
          trigger_ratio: 0.0, // 强制触发
          ..CompactionConfig::default()
        },
        include_environment: false,
      },
    );

    let ctx = provider.build(&session, Vec::new()).await.unwrap();
    assert_eq!(ctx.messages.len(), msgs.len());
  }

  /// include_environment=true 时系统提示含 `<environment>` XML 段。
  #[tokio::test]
  async fn include_env_adds_environment_block() {
    let session = Session::new("s1");
    let provider = HarnessContext::new(
      builder("TPL"),
      None,
      Vec::new(),
      ContextConfig {
        include_environment: true,
        compaction: CompactionConfig::default(),
      },
    );

    let ctx = provider.build(&session, Vec::new()).await.unwrap();
    assert_eq!(ctx.system_prompt.len(), 1);
    match &ctx.system_prompt[0] {
      Content::Text(t) => {
        assert!(t.content.contains("<environment>"));
        assert!(t.content.contains("<platform>"));
      }
      _ => panic!("expected Text content"),
    }
  }

  /// skills 注入 → 系统提示含 `<available-skills>` 清单。
  #[tokio::test]
  async fn skills_are_listed_in_system_prompt() {
    let session = Session::new("s1");
    let skill = Skill {
      name: "terminal".to_string(),
      description: "Run shell commands".to_string(),
      content: "body".to_string(),
      location: std::path::PathBuf::from("/skills/terminal.md"),
      origin: "project".to_string(),
    };

    let provider = HarnessContext::new(
      builder("TPL"),
      None,
      vec![skill],
      ContextConfig {
        include_environment: false,
        compaction: CompactionConfig::default(),
      },
    );

    let ctx = provider.build(&session, Vec::new()).await.unwrap();
    match &ctx.system_prompt[0] {
      Content::Text(t) => {
        assert!(t.content.contains("<available-skills>"));
        assert!(t.content.contains("name=\"terminal\""));
      }
      _ => panic!("expected Text content"),
    }
  }

  /// 摘要成功 → `<conversation-summary>` 进入系统提示。
  #[tokio::test]
  async fn compaction_summary_flows_into_system_prompt() {
    let session = Session::new("s1");
    let msgs = padded_transcript();
    session.append_messages(&msgs);

    // 摘要步骤固定返回文本，mock 网关按调用顺序消费
    let compactor =
      make_compactor_with(vec![text_step("kept decisions and paths")], 64);
    let provider = HarnessContext::new(
      builder("TPL"),
      Some(compactor),
      Vec::new(),
      ContextConfig {
        compaction: CompactionConfig {
          enabled: true,
          trigger_ratio: 0.75,
          keep_recent_turns: 2,
          summarizer_model: Some(ModelId::parse("mock-model").unwrap()),
          ..CompactionConfig::default()
        },
        include_environment: false,
      },
    );

    let ctx = provider.build(&session, Vec::new()).await.unwrap();
    match &ctx.system_prompt[0] {
      Content::Text(t) => {
        assert!(t.content.contains("<conversation-summary>"));
        assert!(t.content.contains("kept decisions and paths"));
      }
      _ => panic!("expected Text content"),
    }
  }

  /// 系统提示为空（空模板 + 无 env + 无技能）→ `context_build_error`。
  #[tokio::test]
  async fn empty_system_prompt_fails_context_build() {
    let session = Session::new("s1");
    let provider = HarnessContext::passthrough("");

    let err: RiboError = provider
      .build(&session, Vec::new())
      .await
      .expect_err("empty system prompt must fail");

    assert_eq!(err.code(), ra_agent::error::CONTEXT_BUILD_ERROR);
  }

  /// 摘要成功 → `compaction_triggered` 置位（事件门控）。
  #[tokio::test]
  async fn compaction_triggered_flag_flows_into_agent_context() {
    let session = Session::new("s1");
    let msgs = padded_transcript();
    session.append_messages(&msgs);

    let compactor =
      make_compactor_with(vec![text_step("kept decisions and paths")], 64);
    let provider = HarnessContext::new(
      builder("TPL"),
      Some(compactor),
      Vec::new(),
      ContextConfig {
        compaction: CompactionConfig {
          enabled: true,
          trigger_ratio: 0.75,
          keep_recent_turns: 2,
          summarizer_model: Some(ModelId::parse("mock-model").unwrap()),
          ..CompactionConfig::default()
        },
        include_environment: false,
      },
    );

    let ctx = provider.build(&session, Vec::new()).await.unwrap();
    assert!(ctx.compaction_triggered, "摘要成功必须置位");
  }

  /// 压缩三态统计写入会话——总量 = 完整历史估算，
  /// 已压缩 = 被移出 prefix 估算（>0），当前 = 请求视图 + system prompt。
  #[tokio::test]
  async fn compaction_stats_flow_into_session() {
    let session = Session::new("s1");
    let msgs = padded_transcript();
    session.append_messages(&msgs);

    let compactor = make_compactor(64); // 极小窗口 → 强制触发
    let provider = HarnessContext::new(
      builder("TPL"),
      Some(compactor),
      Vec::new(),
      ContextConfig {
        compaction: CompactionConfig {
          enabled: true,
          trigger_ratio: 0.75,
          keep_recent_turns: 2,
          summarizer_model: None,
          ..CompactionConfig::default()
        },
        include_environment: false,
      },
    );

    let ctx = provider.build(&session, Vec::new()).await.unwrap();
    let stats = session
      .compaction_stats()
      .expect("build must write compaction stats");

    // 总量 = 压缩前完整历史估算；已压缩 = prefix 估算（触发压缩 → >0）；
    // 当前 = 压缩后视图 + system prompt 估算
    let total_expected = ra_core::message::estimate_context_tokens(&msgs);
    assert_eq!(stats.total_tokens, total_expected);
    assert!(stats.compressed_tokens > 0);
    assert!(
      stats.compressed_tokens < stats.total_tokens,
      "compressed must be a subset of total"
    );
    let view_expected =
      ra_core::message::estimate_context_tokens(&ctx.messages);
    assert!(
      stats.current_tokens >= view_expected,
      "current includes view plus system prompt: {} vs {view_expected}",
      stats.current_tokens
    );
    // 未触发压缩时 compressed 归零口径：再次以大窗口 build → 无压缩
  }

  /// overhead 回写——摘要成功后第二次 build 仍正常
  /// （budget 扣除 overhead 后若 ≤0 退化为 raw_budget，不因 overhead 而
  /// 永不触发），且压缩状态被会话 key 隔离。
  #[tokio::test]
  async fn overhead_writeback_keeps_second_build_alive() {
    let session = Session::new("s1");
    let msgs = padded_transcript();
    session.append_messages(&msgs);

    // 只注入一步摘要：第二次 build 若再次调 LLM 会脚本耗尽失败回退——
    // 但只要状态正确，第二次要么被冷却/滞回抑制（不调 LLM），要么回退滑窗。
    let compactor =
      make_compactor_with(vec![text_step("kept decisions and paths")], 64);
    let provider = HarnessContext::new(
      builder("TPL"),
      Some(compactor),
      Vec::new(),
      ContextConfig {
        compaction: CompactionConfig {
          enabled: true,
          trigger_ratio: 0.75,
          keep_recent_turns: 2,
          summarizer_model: Some(ModelId::parse("mock-model").unwrap()),
          cooldown_turns: 2,
          ..CompactionConfig::default()
        },
        include_environment: false,
      },
    );

    let first = provider.build(&session, Vec::new()).await.unwrap();
    assert!(first.compaction_triggered);
    // 第二轮：冷却期内复用摘要，不调 LLM，状态位复位
    let second = provider.build(&session, Vec::new()).await.unwrap();
    assert!(!second.compaction_triggered, "冷却抑制不得置位状态位");
    // 摘要仍注入（冷却期不丢 prefix 记忆）
    match &second.system_prompt[0] {
      Content::Text(t) => {
        assert!(t.content.contains("kept decisions and paths"));
      }
      _ => panic!("expected Text content"),
    }
  }

  /// 估算口径——总量 = 转录估算，已压缩 = 0，
  /// 当前 = 0（本次进程尚未发起请求，窗口未被占用）。
  #[test]
  fn cold_start_estimate_stats_has_zero_compressed() {
    use crate::session_provider::SessionStatsEstimator;

    let provider = HarnessContext::passthrough("system prompt text");
    let messages = vec![user_text_message("hello")];
    let stats = provider.estimate_stats(&messages);

    assert_eq!(stats.compressed_tokens, 0);
    assert_eq!(
      stats.total_tokens,
      ra_core::message::estimate_context_tokens(&messages)
    );
    // 冷启动未发起请求 → 当前使用量为 0（首次 build 后由真实视图覆写）
    assert_eq!(stats.current_tokens, 0);
  }
}
