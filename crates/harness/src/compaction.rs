//! 上下文压缩器：阈值触发、摘要生成与滑窗回退。
//!
//! 压缩器在请求装配前工作，把完整历史压成「最近轮次 + 可选摘要」；
//! 任何失败（超时 / 取消 / 建流失败 / 空文本）都回退为纯滑窗，
//! 永不向上抛错——失败不是故障，只是压缩未发生。

use std::{sync::Arc, time::Duration};

use ra_core::message::{Content, Message};
use ra_provider::{
  context::Context,
  gateway::Gateway,
  model::Model,
  options::{CancelToken, StreamOptions},
  stream::ProviderEvent,
};

use crate::config::CompactionConfig;

/// 单次压缩的产出。
///
/// 失败路径（回退滑窗）仍返回完整结构体，只是 `summary = None`。
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionOutcome {
  /// 送入模型的最终消息序列。
  pub messages: Vec<Message>,
  /// 摘要文本；未触发、未配置摘要模型或摘要失败时为 `None`。
  pub summary: Option<String>,
  /// 被移出消息序列的消息条数（未触发为 0）。
  pub truncated: usize,
}

/// 摘要请求超时（压缩器内部常量，不暴露到 CompactionConfig；
/// 设计文档 §14 关注点 4 已闭合）。
const SUMMARY_TIMEOUT: Duration = Duration::from_secs(30);

/// 摘要指令：用英文避免中英混排（与模型指令语言一致）。
const SUMMARY_INSTRUCTION: &str = "\
Summarize the earlier part of this conversation for a coding agent. \
Keep: user intents, decisions, file paths, function/type names, and unresolved problems. \
Drop: chit-chat, duplicated tool output, and already-applied patches. \
Output plain text, no markdown headings, within 200 words.";

/// 上下文压缩器。
///
/// 仅依赖网关与主模型（`context_window` 用于阈值预算）。
/// 失败全部回退滑窗，不向上抛错。
pub struct Compactor {
  /// LLM 网关（承载并发准入、熔断、重试）。
  gateway: Arc<Gateway>,
  /// 主模型（提供 `context_window` 与默认路由）。
  model: Model,
}

impl Compactor {
  /// 以网关与主模型创建压缩器。
  pub fn new(gateway: Arc<Gateway>, model: Model) -> Self {
    Self { gateway, model }
  }

  /// 按阈值决定是否压缩；任何失败都回退滑窗，不向上抛错。
  ///
  /// # Arguments
  ///
  /// - `messages`：完整历史（已合并本轮输入）。
  /// - `config`：压缩参数（`enabled == false` 时原样返回）。
  /// - `cancel`：会话子取消令牌，取消时立即放弃摘要请求并回退滑窗。
  pub async fn compact(
    &self,
    messages: &[Message],
    config: &CompactionConfig,
    cancel: Option<CancelToken>,
  ) -> CompactionOutcome {
    // ===== 阶段 1：未启用 → 原样返回 =====
    if !config.enabled {
      return CompactionOutcome {
        messages: messages.to_vec(),
        summary: None,
        truncated: 0,
      };
    }

    // ===== 阶段 2：估算 token 与阈值判定 =====
    let estimated = estimate_tokens(messages);
    let budget =
      (self.model.context_window as f64 * config.trigger_ratio) as usize;

    // context_window 为 0（绕过校验构造）→ budget = 0，仅防御性分支，视为永不触发
    if budget == 0 || estimated <= budget {
      tracing::debug!(
        estimated,
        budget,
        messages = messages.len(),
        "compaction below threshold; skipping"
      );
      return CompactionOutcome {
        messages: messages.to_vec(),
        summary: None,
        truncated: 0,
      };
    }

    // ===== 阶段 3：轮切分 =====
    let Some((prefix, recent)) =
      split_prefix(messages, config.keep_recent_turns)
    else {
      tracing::debug!(
        estimated,
        budget,
        "compaction triggered but keep_recent_turns covers all messages; skipping"
      );
      return CompactionOutcome {
        messages: messages.to_vec(),
        summary: None,
        truncated: 0,
      };
    };

    tracing::info!(
      estimated,
      budget,
      truncated = prefix.len(),
      keep_recent = config.keep_recent_turns,
      "compaction triggered"
    );

    // ===== 阶段 4：无摘要模型 → 纯滑窗 =====
    let Some(summarizer_model) = config.summarizer_model.as_ref() else {
      tracing::warn!(
        truncated = prefix.len(),
        "compaction triggered without summarizer model; falling back to sliding window"
      );
      return CompactionOutcome {
        messages: recent.to_vec(),
        summary: None,
        truncated: prefix.len(),
      };
    };

    // ===== 阶段 5：生成摘要（超时 / 取消 / 失败全部回退滑窗）=====
    let outcome = tokio::time::timeout(
      SUMMARY_TIMEOUT,
      self.run_summarize(prefix, summarizer_model, cancel.clone()),
    )
    .await;

    let messages = recent.to_vec();
    let truncated = prefix.len();

    match outcome {
      // 超时未触发且摘要成功（flatten 后第一层 Ok）
      Ok(Ok(text)) => {
        let mut summary = text.trim().to_string();
        let chars = summary.chars().count();
        if chars > config.max_summary_chars {
          summary = summary.chars().take(config.max_summary_chars).collect();
        }
        tracing::info!(
          truncated,
          summary_chars = summary.chars().count(),
          "summary generated successfully"
        );
        CompactionOutcome {
          messages,
          summary: Some(summary),
          truncated,
        }
      }
      // 超时未触发但摘要失败（建流 / 流内 Error / 空文本 / 取消）
      Ok(Err(reason)) => {
        tracing::warn!(
          reason = %reason,
          truncated,
          "summary failed; falling back to sliding window"
        );
        CompactionOutcome {
          messages,
          summary: None,
          truncated,
        }
      }
      // 超时触发（Elapsed）
      Err(_elapsed) => {
        tracing::warn!(
          reason = "summary timeout",
          truncated,
          "summary failed; falling back to sliding window"
        );
        CompactionOutcome {
          messages,
          summary: None,
          truncated,
        }
      }
    }
  }

  // ===== 内部辅助：摘要请求 =====

  /// 向网关发起摘要请求，流式拼接文本。
  ///
  /// 返回 `Ok(text)` 表示流正常结束且产出非空文本；`Err(reason)` 覆盖
  /// 建流失败、流内 Error 事件、空文本、取消、网关拒绝等所有非成功路径。
  async fn run_summarize(
    &self,
    prefix: &[Message],
    summarizer_model: &ra_provider::ids::ModelId,
    cancel: Option<CancelToken>,
  ) -> Result<String, String> {
    let context = Context {
      system_prompt: vec![Content::Text(ra_core::message::TextContent {
        content: SUMMARY_INSTRUCTION.to_string(),
        signature: None,
      })],
      message: prefix.to_vec(),
      ..Context::default()
    };

    // StreamOptions::default() 的 cancel 字段默认是一个新 CancelToken——
    // 与本会话取消树无关；显式替换为调用方传入的令牌以联动 stop
    let options = StreamOptions {
      cancel,
      ..StreamOptions::default()
    };

    let mut stream = self
      .gateway
      .stream(summarizer_model, &context, options)
      .await
      .map_err(|err| err.to_string())?;

    let mut text = String::new();
    while let Some(event) = stream.next().await {
      match event {
        ProviderEvent::TextDelta { delta, .. } => text.push_str(&delta),
        ProviderEvent::Done { .. } | ProviderEvent::TextEnd { .. } => {}
        ProviderEvent::Error { message, .. } => {
          // 从 message.error_message 取可读描述，回退到 StopReason
          return Err(message.error_message.clone().unwrap_or_else(|| {
            format!("stream error: {:?}", message.stop_reason)
          }));
        }
        _ => {}
      }
    }

    let trimmed = text.trim();
    if trimmed.is_empty() {
      return Err("summary stream produced no text".to_string());
    }
    Ok(trimmed.to_string())
  }
}

// ============================================================================
// 估算与切分辅助
// ============================================================================

/// 按字符 / 4（CVD 经验值）估算 token；偏保守，仅用于阈值触发。
fn estimate_tokens(messages: &[Message]) -> usize {
  let mut chars = 0;
  for msg in messages {
    chars += message_chars(msg);
  }
  chars / 4 + 4 * messages.len()
}

/// 单条消息的字符统计（不含内联图片 base64，避免严重高估）。
fn message_chars(msg: &Message) -> usize {
  match msg {
    Message::User(user) => match &user.content {
      ra_core::message::UserContent::Text(s) => s.chars().count(),
      ra_core::message::UserContent::Blocks(blocks) => {
        blocks.iter().map(content_chars).sum()
      }
    },
    Message::Assistant(assistant) => {
      assistant.content.iter().map(content_chars).sum()
    }
    Message::ToolResult(result) => {
      result.content.iter().map(content_chars).sum()
    }
  }
}

/// 单个 Content 块的字符统计。
fn content_chars(content: &Content) -> usize {
  match content {
    Content::Text(t) => t.content.chars().count(),
    Content::Thinking(t) => t.content.chars().count(),
    Content::ToolCall(tc) => {
      let json_len = serde_json::to_string(&tc.arguments)
        .map(|s| s.chars().count())
        .unwrap_or(0);
      tc.name.chars().count() + json_len
    }
    Content::Image { .. } => 0, // 不计 base64
  }
}

/// 按「User 消息为一轮开始」的规则切分消息序列。
///
/// 取 `keep_recent_turns` 个 User 消息作为 cut 点，之前为 prefix、之后为 recent。
/// User 消息不足、`keep_recent_turns == 0` 或 cut 点为 0 时返回 `None`
/// （不压缩——切分要么覆盖全部、要么无意义）。
fn split_prefix(
  messages: &[Message],
  keep_recent_turns: usize,
) -> Option<(&[Message], &[Message])> {
  let user_indices: Vec<usize> = messages
    .iter()
    .enumerate()
    .filter_map(|(i, msg)| matches!(msg, Message::User(_)).then_some(i))
    .collect();

  // 防御：保留 0 轮 → recent 为空，切分无意义；消息不足也不压缩
  if keep_recent_turns == 0 || user_indices.len() <= keep_recent_turns {
    return None;
  }
  let cut = user_indices[user_indices.len() - keep_recent_turns];
  if cut == 0 {
    return None;
  }
  Some(messages.split_at(cut))
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use ra_core::message::{
    AssistantMessage, Content, Message, StopReason, TextContent, UserContent,
    UserMessage,
  };
  use ra_provider::{
    adapters::mock::{MockProvider, MockStep},
    gateway::{Gateway, GatewayConfig},
    ids::{ModelId, ProviderId},
    model::Model,
  };

  use super::*;
  use crate::config::CompactionConfig;

  /// 构造注册了 mock 提供方的网关 + Model（context_window 可配置）。
  ///
  /// max_tokens 取 `min(context_window, 1024)` 避免网关校验失败。
  fn make_gateway_and_model(
    steps: Vec<MockStep>,
    context_window: u32,
  ) -> (Arc<Gateway>, Model) {
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
    gateway.register(mock).expect("mock 提供方注册成功");
    (Arc::new(gateway), model)
  }

  /// 构造固定时间戳的 user 消息。
  fn user(text: &str) -> Message {
    Message::User(UserMessage {
      content: UserContent::Text(text.to_string()),
      timestamp: 0,
    })
  }

  /// 构造带固定文本的 assistant 消息。
  fn assistant(text: &str) -> Message {
    let mut msg = AssistantMessage::empty("mock", "mock-model", 0);
    msg.content = vec![Content::Text(TextContent {
      content: text.to_string(),
      signature: None,
    })];
    msg.stop_reason = StopReason::Stop;
    Message::Assistant(msg)
  }

  /// 关闭压缩 + 足够大的 history：compact 返回原样且 truncated = 0。
  #[tokio::test]
  async fn compact_disabled_is_noop() {
    let (gateway, model) = make_gateway_and_model(Vec::new(), 8192);
    let compactor = Compactor::new(gateway, model);

    let messages: Vec<_> = (0..50)
      .flat_map(|i| [user(&format!("q{i}")), assistant(&format!("a{i}"))])
      .collect();
    let config = CompactionConfig::default(); // enabled = false

    let outcome = compactor.compact(&messages, &config, None).await;

    assert_eq!(outcome.truncated, 0);
    assert!(outcome.summary.is_none());
    assert_eq!(outcome.messages, messages);
  }

  /// 启用但 budget 足够大：原样返回。
  #[tokio::test]
  async fn compact_below_threshold_is_noop() {
    let (gateway, model) = make_gateway_and_model(Vec::new(), 8192);
    let compactor = Compactor::new(gateway, model);

    let messages = vec![user("hi"), assistant("hello")];
    let config = CompactionConfig {
      enabled: true,
      trigger_ratio: 0.75,
      ..CompactionConfig::default()
    };

    let outcome = compactor.compact(&messages, &config, None).await;

    assert_eq!(outcome.truncated, 0);
    assert!(outcome.summary.is_none());
    assert_eq!(outcome.messages, messages);
  }

  /// 启用 + 触发 + 无 summarizer_model：滑窗回退。
  #[tokio::test]
  async fn compact_without_summarizer_falls_back_to_sliding_window() {
    let (gateway, model) = make_gateway_and_model(Vec::new(), 64); // 极小窗口强制触发
    let compactor = Compactor::new(gateway, model);

    // 构造足够多的消息（估算 ≈ 800 字符 → 估算 ≈ 200 token，而 budget = 64 * 0.75 = 48）
    let mut messages = Vec::new();
    for i in 0..10 {
      messages.push(user(&format!(
        "question number {i} with some padding text to inflate char count"
      )));
      messages.push(assistant(&format!(
        "answer {i} also padded with additional text to ensure budget overflow"
      )));
    }

    let config = CompactionConfig {
      enabled: true,
      trigger_ratio: 0.75,
      keep_recent_turns: 2,
      summarizer_model: None, // 无摘要模型
      ..CompactionConfig::default()
    };

    let outcome = compactor.compact(&messages, &config, None).await;

    assert!(outcome.truncated > 0, "应触发截断");
    assert!(outcome.summary.is_none(), "无摘要模型应为 None");
    // recent 只保留 2 个完整 user 轮 = 4 条消息（2 user + 2 assistant）
    assert_eq!(outcome.messages.len(), 4);
    // recent 的前条应是倒数第 3 个 user
    match &outcome.messages[0] {
      Message::User(u) => match &u.content {
        ra_core::message::UserContent::Text(s) => {
          assert_eq!(
            s,
            "question number 8 with some padding text to inflate char count"
          );
        }
        _ => panic!("expected UserContent::Text"),
      },
      _ => panic!("expected Message::User"),
    }
  }

  /// split_prefix 辅助的边界：User 消息不足 keep_recent_turns → None。
  #[test]
  fn split_prefix_insufficient_users_returns_none() {
    let messages = vec![user("a"), assistant("b"), user("c")];
    assert!(split_prefix(&messages, 4).is_none());
    assert!(split_prefix(&messages, 3).is_none()); // len == 3 也返回 None（需要 > 才能 split）
    assert!(split_prefix(&messages, 0).is_none()); // 防御：keep_recent_turns = 0
  }

  /// split_prefix 正常切分：prefix / recent 边界正确。
  #[test]
  fn split_prefix_cuts_on_boundary_user() {
    let mut messages = Vec::new();
    for i in 0..6 {
      messages.push(user(&format!("q{i}")));
      messages.push(assistant(&format!("a{i}")));
    }

    // keep_recent_turns = 2，总共有 6 个 User 消息
    let (prefix, recent) = split_prefix(&messages, 2).unwrap();
    // User indices = [0,2,4,6,8,10]，cut = user_indices[6-2=4] = 8
    assert_eq!(prefix.len(), 8); // 0..8（前 4 轮）
    assert_eq!(recent.len(), 4); // 8..12（后 2 轮）
    assert!(matches!(recent[0], Message::User(_)));
    assert!(matches!(prefix.last(), Some(Message::Assistant(_))));
  }

  /// estimate_tokens 覆盖所有消息变体，不 panic。
  #[test]
  fn estimate_tokens_covers_all_variants() {
    let messages = vec![
      user("hello world"),
      assistant("hi there"),
      Message::ToolResult(ra_core::message::ToolResultMessage {
        tool_call_id: "c1".to_string(),
        tool_name: "echo".to_string(),
        content: vec![Content::Text(TextContent {
          content: "ok".to_string(),
          signature: None,
        })],
        is_error: false,
        timestamp: 0,
        details: None,
        usage: None,
        added_tool_names: Vec::new(),
      }),
    ];
    let tokens = estimate_tokens(&messages);
    assert!(tokens > 0);
  }
}
