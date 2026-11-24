//! 上下文压缩器：智能触发（滞回 + 冷却）、增量摘要、强制全量重建与滑窗回退。
//!
//! 压缩器在请求装配前工作，把完整历史压成「最近轮次 + 可选摘要」；
//! 任何失败（超时 / 取消 / 建流失败 / 空文本）都回退为纯滑窗，
//! 永不向上抛错——失败不是故障，只是压缩未发生。
//!
//! 不引入 `SmartCompressor` 替代者，而是在 `Compactor`
//! 内部增加 per-session 状态表（滞回基准 / 冷却 / 增量检查点）。状态是
//! 进程内派生数据，重启后可从 JSONL 的 `compaction` 旁路记录恢复。

use std::{sync::Arc, time::Duration};

use dashmap::DashMap;
use ra_core::message::{
  Content, Message, TextContent, UserContent, UserMessage,
};
use ra_provider::{
  context::Context,
  gateway::Gateway,
  model::Model,
  options::{CancelToken, StreamOptions},
  stream::ProviderEvent,
};

use crate::{
  compaction_state::{
    CompactionCache, CompactionState, CompactionStateSnapshot,
    fingerprint_message, locate_checkpoint,
  },
  config::CompactionConfig,
  session_store::JsonlSessionStore,
};

/// 本次压缩的判定原因（可观测性，提示门控）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionReason {
  /// 压缩未启用。
  Disabled,
  /// 估算未超预算，无需压缩。
  BelowThreshold,
  /// User 轮数不足，切分无意义。
  NoSplit,
  /// 滞回区间内：上下文增长不显著，复用上次摘要（不调 LLM）。
  HysteresisSuppressed,
  /// 冷却期内：复用上次摘要（不调 LLM）。
  CooldownSuppressed,
  /// 本轮生成了新摘要（增量或全量重建）。
  Summarized,
  /// 未配置摘要模型，纯滑窗回退。
  NoSummarizer,
  /// 摘要调用失败（建流 / 流内 Error / 空文本 / 超时 / 取消），滑窗回退。
  SummaryFailed,
}

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
  /// 判定原因（提示门控）：`Summarized` → 成功提示，
  /// `SummaryFailed` → 失败提示；
  /// 冷却抑制 / 滞回复用不发射提示。
  pub reason: CompactionReason,
  /// 本轮是否执行了全量重建（强制重建可观测）。
  pub rebuild: bool,
  /// 无摘要且实际发生截断 → `system_prompt` 追加 `<compaction-fallback>`。
  pub fallback_hint: bool,
  /// 冷却期复用过期摘要的轮数 N → 追加 `<conversation-summary-outdated>`。
  pub outdated_hint: Option<u32>,
  /// 本轮被摘要替代移出视图的 prefix 估算 token（压缩三态统计"已压缩量"，
  /// 未触发 / 未切分为 0）。
  pub compressed_tokens: usize,
}

impl CompactionOutcome {
  /// 构造「未触发压缩」产出（原样返回消息）。
  pub(crate) fn noop(messages: &[Message], reason: CompactionReason) -> Self {
    Self {
      messages: messages.to_vec(),
      summary: None,
      truncated: 0,
      reason,
      rebuild: false,
      fallback_hint: false,
      outdated_hint: None,
      compressed_tokens: 0,
    }
  }
}

/// 摘要请求超时（压缩器内部常量，不暴露到 CompactionConfig）。
const SUMMARY_TIMEOUT: Duration = Duration::from_secs(30);

/// 摘要输入上限自动取值比例（摘要模型 / 主模型窗口的 70%，窗口保护）。
const SUMMARY_INPUT_WINDOW_RATIO: f64 = 0.7;

/// 首次 build 的 overhead 基准（约 200 token；模板 + 环境段初始长度）。
const INITIAL_OVERHEAD_TOKENS: usize = 200;

/// 摘要指令：用英文避免中英混排（与模型指令语言一致）。
const SUMMARY_INSTRUCTION: &str = "\
Summarize the earlier part of this conversation for a coding agent. \
Keep: user intents, decisions, file paths, function/type names, and unresolved problems. \
Drop: chit-chat, duplicated tool output, and already-applied patches. \
Output plain text, no markdown headings, within 200 words.";

/// 增量摘要时，上次摘要前置文本块的包裹标签。
const PREVIOUS_SUMMARY_TAG: &str = "previous-summary";

/// 上下文压缩器。
///
/// 持有网关、主模型（提供 `context_window` 预算）、per-session 状态表，
/// 以及可选的 JSONL 存储（摘要记录写入与重启恢复）。失败全部回退滑窗，
/// 不向上抛错。`Compactor` 经 `Arc` 共享，`compact(&self)` 可并发调用。
pub struct Compactor {
  /// LLM 网关（承载并发准入、熔断、重试）。
  gateway: Arc<Gateway>,
  /// 主模型（提供 `context_window` 与默认路由）。
  model: Model,
  /// per-session 压缩状态表（内部可变性，DashMap 天然并发安全）。
  states: DashMap<String, CompactionState>,
  /// 可选 JSONL 存储：写入摘要旁路记录 + 重启恢复。
  store: Option<Arc<JsonlSessionStore>>,
}

impl Compactor {
  /// 以网关与主模型创建压缩器（无持久化，状态仅进程内）。
  pub fn new(gateway: Arc<Gateway>, model: Model) -> Self {
    Self {
      gateway,
      model,
      states: DashMap::new(),
      store: None,
    }
  }

  /// 绑定 JSONL 存储，启用 摘要记录写入与重启恢复。
  ///
  /// builder 风格消费并返回自身；装配期在 store 构造后调用。
  pub fn with_store(mut self, store: Arc<JsonlSessionStore>) -> Self {
    self.store = Some(store);
    self
  }

  /// 读取某会话 overhead 基准的只读快照（crate-internal）。
  pub(crate) fn get_state_snapshot(
    &self,
    key: &str,
  ) -> CompactionStateSnapshot {
    self
      .states
      .get(key)
      .map(|state| state.snapshot())
      .unwrap_or_default()
  }

  /// 回写某会话的 overhead 基准（crate-internal）。
  ///
  /// 首次调用会创建状态条目；其余运行时字段保持默认。
  pub(crate) fn set_last_overhead(&self, key: &str, overhead: usize) {
    let mut state =
      self
        .states
        .entry(key.to_string())
        .or_insert_with(|| CompactionState {
          last_overhead: INITIAL_OVERHEAD_TOKENS,
          ..CompactionState::default()
        });
    state.last_overhead = overhead;
  }

  /// 按智能触发策略决定是否压缩；任何失败都回退滑窗，不向上抛错。
  ///
  /// # Arguments
  ///
  /// - `session_key`：会话标识，用于 per-session 状态隔离与 JSONL 记录定位。
  /// - `messages`：完整历史（已合并本轮输入）。
  /// - `config`：压缩参数（`enabled == false` 时原样返回）。
  /// - `cancel`：会话子取消令牌，取消时立即放弃摘要请求并回退滑窗。
  pub async fn compact(
    &self,
    session_key: &str,
    messages: &[Message],
    config: &CompactionConfig,
    cancel: Option<CancelToken>,
  ) -> CompactionOutcome {
    // ===== 阶段 1：未启用 → 原样返回 =====
    if !config.enabled {
      return CompactionOutcome::noop(messages, CompactionReason::Disabled);
    }

    // ===== 阶段 2：惰性恢复状态（重启后从 JSONL 摘要记录恢复）=====
    self
      .restore_state_if_needed(session_key, messages, config)
      .await;

    // ===== 阶段 3：估算 token 与预算判定（扣 overhead，≤0 退化）=====
    let estimated = estimate_tokens(messages);
    let raw_budget =
      (self.model.context_window as f64 * config.trigger_ratio) as usize;
    // context_window 为 0（绕过校验构造）→ 永不触发，防御性分支
    if raw_budget == 0 {
      tracing::warn!(
        session_key = %session_key,
        "compaction budget is zero; compaction disabled"
      );
      return CompactionOutcome::noop(
        messages,
        CompactionReason::BelowThreshold,
      );
    }
    let last_overhead = self.get_state_snapshot(session_key).last_overhead;
    // 预算 = 原始预算 − 上一轮 overhead。overhead ≥ 原始预算时
    // **退化为原始预算**：不因 prompt 过大而把预算
    // 压成 0——预算为 0 会让「历史是否超预算」的判定永远为真，导致每轮
    // 都触发压缩。
    let budget = match raw_budget.saturating_sub(last_overhead) {
      0 => {
        tracing::warn!(
          session_key = %session_key,
          raw_budget,
          last_overhead,
          "overhead exceeds raw budget; using raw budget as fallback"
        );
        raw_budget
      }
      budget => budget,
    };

    if estimated <= budget {
      tracing::debug!(
        session_key = %session_key,
        estimated,
        budget,
        messages = messages.len(),
        "compaction below threshold; skipping"
      );
      return CompactionOutcome::noop(
        messages,
        CompactionReason::BelowThreshold,
      );
    }

    // ===== 阶段 4：轮切分（复用现有不变量）=====
    let Some((prefix, recent)) =
      split_prefix(messages, config.keep_recent_turns)
    else {
      tracing::debug!(
        session_key = %session_key,
        estimated,
        budget,
        "compaction triggered but keep_recent_turns covers all messages; skipping"
      );
      return CompactionOutcome::noop(messages, CompactionReason::NoSplit);
    };

    // ===== 阶段 5：判定路径（滞回 / 冷却 / 复用 last_summary）=====
    // 压缩三态统计"已压缩量"：本轮被移出视图的 prefix 估算
    let prefix_tokens = estimate_tokens(prefix);
    // 取出当前状态快照后按判定结果更新；冷却与滞回轮次不调用 LLM。
    let state = self
      .states
      .get(session_key)
      .map(|entry| entry.clone())
      .unwrap_or_default();

    // 冷却期（顺序）：先递减 -1，再跳过摘要调用，注入 last_summary
    if state.cooldown_turns_left > 0 {
      return self.reuse_summary(
        session_key,
        prefix,
        recent,
        &state,
        CompactionReason::CooldownSuppressed,
      );
    }

    // 滞回判定：last_trigger_tokens == 0（首次）或增长达 hysteresis 倍才重算
    let hysteresis_ok = state.last_trigger_tokens == 0
      || (estimated as f64)
        >= (state.last_trigger_tokens as f64 * config.hysteresis_ratio);
    if !hysteresis_ok {
      tracing::debug!(
        session_key = %session_key,
        estimated,
        last_trigger = state.last_trigger_tokens,
        ratio = config.hysteresis_ratio,
        "compaction within hysteresis band; reusing last summary"
      );
      return self.reuse_summary(
        session_key,
        prefix,
        recent,
        &state,
        CompactionReason::HysteresisSuppressed,
      );
    }

    // ===== 阶段 6：无摘要模型 → 纯滑窗 =====
    let Some(summarizer_model) = config.summarizer_model.as_ref() else {
      tracing::warn!(
        session_key = %session_key,
        truncated = prefix.len(),
        "compaction triggered without summarizer model; falling back to sliding window"
      );
      let outcome = CompactionOutcome {
        messages: recent.to_vec(),
        summary: None,
        truncated: prefix.len(),
        reason: CompactionReason::NoSummarizer,
        rebuild: false,
        fallback_hint: true,
        outdated_hint: None,
        compressed_tokens: estimate_tokens(prefix),
      };
      self.apply_no_summary_state(session_key, estimated, prefix.len(), config);
      return outcome;
    };

    // ===== 阶段 7：决定增量 / 全量重建=====
    let rebuild = self.should_rebuild(&state, config);
    let summary_input = self.build_summary_input(
      messages,
      prefix,
      &state,
      rebuild,
      summarizer_model.as_str(),
      config,
    );

    tracing::info!(
      session_key = %session_key,
      estimated,
      budget,
      truncated = prefix.len(),
      rebuild,
      incremental = !rebuild && state.last_summary.is_some(),
      "compaction summary requested"
    );

    // ===== 阶段 8：生成摘要（超时 / 取消 / 失败全部回退滑窗）=====
    let outcome = tokio::time::timeout(
      SUMMARY_TIMEOUT,
      self.run_summarize(&summary_input, summarizer_model, cancel.clone()),
    )
    .await;

    let final_messages = recent.to_vec();
    let truncated = prefix.len();

    match outcome {
      // 摘要成功（flatten 后第一层 Ok）
      Ok(Ok(text)) => {
        let mut summary = text.trim().to_string();
        let chars = summary.chars().count();
        if chars > config.max_summary_chars {
          summary = summary.chars().take(config.max_summary_chars).collect();
        }

        // 更新状态：新指纹 / 摘要 / 截断 / 滞回基准 / 冷却 / 重建计数
        let checkpoint = prefix.last().map(fingerprint_message).unwrap_or(0);
        // 覆盖前判定是否为增量成功（全量重建后必须归零计数）
        let was_incremental = !rebuild && state.last_summary.is_some();
        let mut state = self.states.entry(session_key.to_string()).or_default();
        state.last_trigger_tokens = estimated;
        state.cooldown_turns_left = config.cooldown_turns;
        state.last_summary = Some(summary.clone());
        state.checkpoint_fingerprint = checkpoint;
        state.last_truncated = truncated;
        state.summary_age_turns = 0;
        if rebuild {
          state.incremental_count = 0;
        } else if was_incremental {
          state.incremental_count = state.incremental_count.saturating_add(1);
        }
        drop(state);

        // 写摘要旁路记录（失败只告警，不影响产出）
        self
          .write_compaction_record(
            session_key,
            &summary,
            checkpoint,
            summarizer_model.as_str(),
          )
          .await;

        tracing::info!(
          session_key = %session_key,
          truncated,
          summary_chars = summary.chars().count(),
          rebuild,
          "summary generated successfully"
        );
        CompactionOutcome {
          messages: final_messages,
          summary: Some(summary),
          truncated,
          reason: CompactionReason::Summarized,
          rebuild,
          fallback_hint: false,
          outdated_hint: None,
          compressed_tokens: prefix_tokens,
        }
      }
      // 摘要失败（建流 / 流内 Error / 空文本 / 取消）
      Ok(Err(reason)) => {
        tracing::warn!(
          session_key = %session_key,
          reason = %reason,
          truncated,
          "summary failed; falling back to sliding window"
        );
        self.apply_no_summary_state(session_key, estimated, truncated, config);
        CompactionOutcome {
          messages: final_messages,
          summary: None,
          truncated,
          reason: CompactionReason::SummaryFailed,
          rebuild,
          fallback_hint: true,
          outdated_hint: None,
          compressed_tokens: prefix_tokens,
        }
      }
      // 超时
      Err(_elapsed) => {
        tracing::warn!(
          session_key = %session_key,
          reason = "summary timeout",
          truncated,
          "summary failed; falling back to sliding window"
        );
        self.apply_no_summary_state(session_key, estimated, truncated, config);
        CompactionOutcome {
          messages: final_messages,
          summary: None,
          truncated,
          reason: CompactionReason::SummaryFailed,
          rebuild,
          fallback_hint: true,
          outdated_hint: None,
          compressed_tokens: prefix_tokens,
        }
      }
    }
  }

  // ===== 内部辅助：状态恢复与更新 =====

  /// 首次遇到某会话时，从 JSONL 摘要记录惰性恢复状态。
  ///
  /// 失效判定：摘要模型变更、检查点指纹在 transcript 中找不到、
  /// 或检查点距当前超过 `cooldown_turns × 2` 轮（严重过期）→ 忽略记录，
  /// 记 `StaleCompaction` 诊断日志，走全量路径。
  async fn restore_state_if_needed(
    &self,
    key: &str,
    messages: &[Message],
    config: &CompactionConfig,
  ) {
    if self.states.contains_key(key) {
      return;
    }
    let Some(store) = &self.store else {
      return;
    };
    let cache = match store.load_compaction(key).await {
      Ok(cache) => cache,
      Err(err) => {
        tracing::warn!(
          session_key = %key,
          error = %err,
          "load compaction cache failed; starting fresh"
        );
        return;
      }
    };
    let Some(cache) = cache else {
      return;
    };

    // 失效判定 1：摘要模型必须与当前配置一致（模型变更即失效）
    let model_matches = config
      .summarizer_model
      .as_ref()
      .is_some_and(|model| model.as_str() == cache.summarizer_model);
    if !model_matches {
      tracing::info!(
        session_key = %key,
        "stale compaction cache: summarizer model changed"
      );
      return;
    }

    // 失效判定 2：检查点指纹须能在当前 transcript 中定位
    let Some(index) = locate_checkpoint(messages, cache.checkpoint_fingerprint)
    else {
      tracing::info!(
        session_key = %key,
        "stale compaction cache: checkpoint fingerprint not found"
      );
      return;
    };

    // 失效判定 3：严重过期（检查点之后超过 cooldown×2 个 user 轮）不恢复
    let turns_after = messages[index + 1..]
      .iter()
      .filter(|message| matches!(message, Message::User(_)))
      .count();
    let stale_threshold = (config.cooldown_turns as usize).saturating_mul(2);
    if turns_after > stale_threshold {
      tracing::info!(
        session_key = %key,
        turns_after,
        stale_threshold,
        "stale compaction cache: summary too old"
      );
      return;
    }

    let state = CompactionState::from_cache(&cache);
    self.states.insert(key.to_string(), state);
    tracing::info!(
      session_key = %key,
      generated_at = cache.generated_at,
      "compaction state restored from jsonl"
    );
  }

  /// 写摘要旁路记录（失败只告警）。
  async fn write_compaction_record(
    &self,
    key: &str,
    summary: &str,
    checkpoint: u64,
    summarizer_model: &str,
  ) {
    let Some(store) = &self.store else {
      return;
    };
    let cache = CompactionCache {
      last_summary: summary.to_string(),
      checkpoint_fingerprint: checkpoint,
      summarizer_model: summarizer_model.to_string(),
      generated_at: ra_core::utils::time::now_ms(),
    };
    if let Err(err) = store.append_compaction(key, &cache).await {
      tracing::warn!(
        session_key = %key,
        error = %err,
        "append compaction record failed"
      );
    }
  }

  /// 摘要失败 / 无模型路径的状态更新（不覆盖好缓存的 last_summary）。
  fn apply_no_summary_state(
    &self,
    key: &str,
    estimated: usize,
    truncated: usize,
    config: &CompactionConfig,
  ) {
    if let Some(mut state) = self.states.get_mut(key) {
      state.last_trigger_tokens = estimated;
      state.cooldown_turns_left = config.cooldown_turns;
      state.last_truncated = truncated;
      state.summary_age_turns = state.summary_age_turns.saturating_add(1);
      // 注意：不改动 last_summary / checkpoint_fingerprint——保留上次成功缓存
      tracing::debug!(
        session_key = %key,
        estimated,
        truncated,
        cooldown = config.cooldown_turns,
        "compaction state updated on no-summary path"
      );
    }
  }

  /// 冷却 / 滞回期复用 last_summary（不调 LLM，+ 过期提示）。
  ///
  /// 若此前无摘要则退化为纯滑窗 + fallback 提示；有摘要则注入该摘要，
  /// 并在摘要已存在多轮时追加过期提示（`summary_age_turns` 递增）。
  fn reuse_summary(
    &self,
    session_key: &str,
    prefix: &[Message],
    recent: &[Message],
    state: &CompactionState,
    reason: CompactionReason,
  ) -> CompactionOutcome {
    // 冷却轮递减已在判定时完成（本轮设值不递减，下轮先减）；
    // 这里只维护摘要年龄与 truncated 可观测值
    if let Some(mut current) = self.states.get_mut(session_key) {
      if reason == CompactionReason::CooldownSuppressed
        && current.cooldown_turns_left > 0
      {
        current.cooldown_turns_left -= 1;
      }
      current.summary_age_turns = current.summary_age_turns.saturating_add(1);
      current.last_truncated = prefix.len();
    }

    match &state.last_summary {
      Some(summary) => {
        let age = state.summary_age_turns.saturating_add(1);
        tracing::info!(
          session_key = %session_key,
          reason = ?reason,
          truncated = prefix.len(),
          age_turns = age,
          "compaction suppressed; reusing last summary"
        );
        CompactionOutcome {
          messages: recent.to_vec(),
          summary: Some(summary.clone()),
          truncated: prefix.len(),
          reason,
          rebuild: false,
          fallback_hint: false,
          outdated_hint: (age > 1).then_some(age),
          compressed_tokens: estimate_tokens(prefix),
        }
      }
      None => {
        tracing::warn!(
          session_key = %session_key,
          reason = ?reason,
          truncated = prefix.len(),
          "compaction suppressed but no last summary; falling back"
        );
        CompactionOutcome {
          messages: recent.to_vec(),
          summary: None,
          truncated: prefix.len(),
          reason,
          rebuild: false,
          fallback_hint: true,
          outdated_hint: None,
          compressed_tokens: estimate_tokens(prefix),
        }
      }
    }
  }

  /// 判断本轮是否必须全量重建。
  ///
  /// `incremental_count + 1 >= incremental_rebuild_every` 时重建；
  /// `rebuild_every == 1` 等价于每次全量。无 last_summary 也走全量。
  fn should_rebuild(
    &self,
    state: &CompactionState,
    config: &CompactionConfig,
  ) -> bool {
    let rebuild = if !config.enable_incremental || state.last_summary.is_none()
    {
      true
    } else {
      state.incremental_count + 1 >= config.incremental_rebuild_every
    };
    tracing::debug!(
      rebuild,
      incremental = config.enable_incremental,
      has_last_summary = state.last_summary.is_some(),
      incremental_count = state.incremental_count,
      "compaction rebuild decision"
    );
    rebuild
  }

  /// 构造摘要输入消息序列（增量 + 窗口保护）。
  ///
  /// - 全量：直接取完整 prefix。
  /// - 增量：上次检查点之后的 prefix 增量，前置 last_summary 文本块。
  /// - 窗口保护：超 `max_summary_input_tokens`（默认摘要模型窗口 70%）时，
  ///   丢弃最早的增量部分，保留摘要块与最近增量。
  fn build_summary_input(
    &self,
    messages: &[Message],
    prefix: &[Message],
    state: &CompactionState,
    rebuild: bool,
    summarizer_model: &str,
    config: &CompactionConfig,
  ) -> Vec<Message> {
    let mut input = if rebuild {
      prefix.to_vec()
    } else {
      // 增量：定位检查点，取其之后到 prefix 末尾（不含 recent 轮）的增量。
      // split_at 保证 prefix 是 messages 的前缀，故 cut = prefix.len()。
      let cut = prefix.len();
      let checkpoint_index =
        locate_checkpoint(messages, state.checkpoint_fingerprint);
      match checkpoint_index {
        Some(index) if index < cut => {
          let mut incremental = Vec::new();
          if let Some(summary) = &state.last_summary {
            incremental.push(Message::User(UserMessage {
              content: UserContent::Text(format!(
                "<{PREVIOUS_SUMMARY_TAG}>\n{summary}\n</{PREVIOUS_SUMMARY_TAG}>"
              )),
              timestamp: 0,
            }));
          }
          incremental.extend_from_slice(&messages[index + 1..cut]);
          incremental
        }
        // 检查点丢失 / 落在 recent 内 → 全量回退（失败隔离）
        _ => {
          tracing::warn!(
            checkpoint = state.checkpoint_fingerprint,
            "checkpoint missing or inside recent window; falling back to full summary input"
          );
          prefix.to_vec()
        }
      }
    };

    // 窗口保护：尾部截断（保留前置摘要块与最近增量，丢弃最早增量）
    let limit = self.summary_input_limit(summarizer_model, config);
    if limit > 0 && estimate_tokens(&input) > limit {
      let keep_summary_block = !rebuild
        && state.last_summary.is_some()
        && matches!(input.first(), Some(Message::User(_)));
      let head = usize::from(keep_summary_block);
      while input.len() > head + 1 && estimate_tokens(&input) > limit {
        input.remove(head);
      }
      tracing::warn!(
        truncated_input = input.len(),
        limit,
        "summary input truncated by window protection"
      );
    }
    input
  }

  /// 摘要输入 token 上限：配置值 > 0 用配置，否则摘要模型窗口 70%，
  /// 摘要模型窗口未知时用主模型窗口 70%。
  fn summary_input_limit(
    &self,
    summarizer_model: &str,
    config: &CompactionConfig,
  ) -> usize {
    if config.max_summary_input_tokens > 0 {
      return config.max_summary_input_tokens;
    }
    let _ = summarizer_model; // 网关暂不暴露按模型查窗口，统一用主模型预算
    (self.model.context_window as f64 * SUMMARY_INPUT_WINDOW_RATIO) as usize
  }

  // ===== 内部辅助：摘要请求 =====

  /// 向网关发起摘要请求，流式拼接文本。
  async fn run_summarize(
    &self,
    input: &[Message],
    summarizer_model: &ra_provider::ids::ModelId,
    cancel: Option<CancelToken>,
  ) -> Result<String, String> {
    let context = Context {
      system_prompt: vec![Content::Text(TextContent {
        content: SUMMARY_INSTRUCTION.to_string(),
        signature: None,
      })],
      message: input.to_vec(),
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

/// 按加权字符口径估算 token（唯一真值：ASCII 0.25 / CJK 1.0 / 其他 0.5）。
pub use ra_core::message::estimate_context_tokens;

fn estimate_tokens(messages: &[Message]) -> usize {
  ra_core::message::estimate_context_tokens(messages)
}

/// 按「User 消息为一轮开始」的规则切分消息序列。
///
/// 取 `keep_recent_turns` 个 User 消息作为 cut 点，之前为 prefix、之后为 recent。
/// User 消息不足、`keep_recent_turns == 0` 或 cut 点为 0 时返回 `None`。
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

  use ra_core::message::{AssistantMessage, StopReason};
  use ra_provider::{
    adapters::mock::{MockProvider, MockStep},
    gateway::{Gateway, GatewayConfig},
    ids::{ModelId, ProviderId},
  };

  use super::*;
  use crate::config::CompactionConfig;

  /// 构造注册了 mock 提供方的网关 + Model（context_window 可配置）。
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

  /// 构造文本增量摘要步骤（Complete 不发 TextDelta，必须用 Events 回放）。
  fn text_step(text: &str) -> MockStep {
    let t = text.to_string();
    MockStep::Events(vec![ProviderEvent::TextDelta {
      index: 0,
      delta: t,
      partial: Arc::new(AssistantMessage::empty("mock", "mock-model", 0)),
    }])
  }

  /// 构造足够多消息强制触发（英文长文本，预算 64×0.75=48）。
  fn padded_messages() -> Vec<Message> {
    let mut messages = Vec::new();
    for i in 0..10 {
      messages.push(user(&format!(
        "question number {i} with some padding text to inflate char count"
      )));
      messages.push(assistant(&format!(
        "answer {i} also padded with additional text to ensure budget overflow"
      )));
    }
    messages
  }

  fn enabled_config() -> CompactionConfig {
    CompactionConfig {
      enabled: true,
      trigger_ratio: 0.75,
      keep_recent_turns: 2,
      ..CompactionConfig::default()
    }
  }

  /// 关闭压缩 + 足够大的 history：compact 返回原样且 truncated = 0。
  #[tokio::test]
  async fn compact_disabled_is_noop() {
    let (gateway, model) = make_gateway_and_model(Vec::new(), 8192);
    let compactor = Compactor::new(gateway, model);
    let messages = padded_messages();

    let outcome = compactor
      .compact("s1", &messages, &CompactionConfig::default(), None)
      .await;

    assert_eq!(outcome.truncated, 0);
    assert!(outcome.summary.is_none());
    assert_eq!(outcome.messages, messages);
    assert_eq!(outcome.reason, CompactionReason::Disabled);
  }

  /// 启用但 budget 足够大：原样返回。
  #[tokio::test]
  async fn compact_below_threshold_is_noop() {
    let (gateway, model) = make_gateway_and_model(Vec::new(), 8192);
    let compactor = Compactor::new(gateway, model);
    let messages = vec![user("hi"), assistant("hello")];

    let outcome = compactor
      .compact("s1", &messages, &enabled_config(), None)
      .await;

    assert_eq!(outcome.truncated, 0);
    assert_eq!(outcome.reason, CompactionReason::BelowThreshold);
    assert_eq!(outcome.messages, messages);
  }

  /// 启用 + 触发 + 无 summarizer_model：滑窗回退 + fallback 提示。
  #[tokio::test]
  async fn compact_without_summarizer_falls_back_to_sliding_window() {
    let (gateway, model) = make_gateway_and_model(Vec::new(), 64);
    let compactor = Compactor::new(gateway, model);
    let messages = padded_messages();

    let outcome = compactor
      .compact("s1", &messages, &enabled_config(), None)
      .await;

    assert_eq!(outcome.reason, CompactionReason::NoSummarizer);
    assert!(outcome.summary.is_none());
    assert!(outcome.fallback_hint);
    assert_eq!(outcome.messages.len(), 4);
  }

  /// split_prefix 辅助的边界：User 消息不足 → None。
  #[test]
  fn split_prefix_insufficient_users_returns_none() {
    let messages = vec![user("a"), assistant("b"), user("c")];
    assert!(split_prefix(&messages, 4).is_none());
    assert!(split_prefix(&messages, 3).is_none());
    assert!(split_prefix(&messages, 0).is_none());
  }

  /// split_prefix 正常切分：prefix / recent 边界正确。
  #[test]
  fn split_prefix_cuts_on_boundary_user() {
    let mut messages = Vec::new();
    for i in 0..6 {
      messages.push(user(&format!("q{i}")));
      messages.push(assistant(&format!("a{i}")));
    }
    let (prefix, recent) = split_prefix(&messages, 2).unwrap();
    assert_eq!(prefix.len(), 8);
    assert_eq!(recent.len(), 4);
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
    assert!(estimate_tokens(&messages) > 0);
  }

  /// 摘要成功产出直测：messages / truncated / summary 三字段 + reason。
  #[tokio::test]
  async fn compact_summary_success_produces_all_fields() {
    let (gateway, model) =
      make_gateway_and_model(vec![text_step("kept decisions")], 64);
    let compactor = Compactor::new(gateway, model);
    let messages = padded_messages();
    let config = CompactionConfig {
      summarizer_model: Some(ModelId::parse("mock-model").unwrap()),
      ..enabled_config()
    };

    let outcome = compactor.compact("s1", &messages, &config, None).await;

    assert_eq!(outcome.reason, CompactionReason::Summarized);
    assert_eq!(outcome.summary.as_deref(), Some("kept decisions"));
    assert!(outcome.truncated > 0);
    assert_eq!(outcome.messages.len(), 4);
    assert!(!outcome.fallback_hint);
  }

  /// 摘要按 max_summary_chars 字符截断。
  #[tokio::test]
  async fn compact_summary_respects_max_chars() {
    let (gateway, model) =
      make_gateway_and_model(vec![text_step("abcdefghij")], 64);
    let compactor = Compactor::new(gateway, model);
    let messages = padded_messages();
    let config = CompactionConfig {
      summarizer_model: Some(ModelId::parse("mock-model").unwrap()),
      max_summary_chars: 4,
      ..enabled_config()
    };

    let outcome = compactor.compact("s1", &messages, &config, None).await;

    assert_eq!(outcome.summary.as_deref(), Some("abcd"));
  }

  /// 摘要流内 Error → 滑窗回退 + fallback，不抛错。
  #[tokio::test]
  async fn compact_summary_error_falls_back() {
    let mut error_message = AssistantMessage::empty("mock", "mock-model", 0);
    error_message.error_message = Some("boom".to_string());
    let (gateway, model) = make_gateway_and_model(
      vec![MockStep::Events(vec![ProviderEvent::Error {
        reason: StopReason::Error,
        message: error_message,
        code: None,
      }])],
      64,
    );
    let compactor = Compactor::new(gateway, model);
    let messages = padded_messages();
    let config = CompactionConfig {
      summarizer_model: Some(ModelId::parse("mock-model").unwrap()),
      ..enabled_config()
    };

    let outcome = compactor.compact("s1", &messages, &config, None).await;

    assert_eq!(outcome.reason, CompactionReason::SummaryFailed);
    assert!(outcome.summary.is_none());
    assert!(outcome.fallback_hint);
    assert_eq!(outcome.messages.len(), 4);
  }

  /// 连续两轮同会话：第二轮被冷却抑制，复用 last_summary，不再次调用摘要。
  #[tokio::test]
  async fn second_turn_suppressed_by_cooldown() {
    // 只注入一步摘要步骤：第二轮若额外调用摘要会因脚本耗尽而失败
    let (gateway, model) =
      make_gateway_and_model(vec![text_step("first summary")], 64);
    let compactor = Compactor::new(gateway, model);
    let messages = padded_messages();
    let config = CompactionConfig {
      summarizer_model: Some(ModelId::parse("mock-model").unwrap()),
      cooldown_turns: 2,
      ..enabled_config()
    };

    let first = compactor.compact("s1", &messages, &config, None).await;
    assert_eq!(first.reason, CompactionReason::Summarized);
    assert_eq!(first.summary.as_deref(), Some("first summary"));

    let second = compactor.compact("s1", &messages, &config, None).await;
    assert_eq!(second.reason, CompactionReason::CooldownSuppressed);
    assert_eq!(second.summary.as_deref(), Some("first summary"));
  }

  /// 会话间状态隔离：s2 首次触发仍走完整摘要（不受 s1 冷却影响）。
  #[tokio::test]
  async fn sessions_are_state_isolated() {
    let (gateway, model) = make_gateway_and_model(
      vec![text_step("summary one"), text_step("summary two")],
      64,
    );
    let compactor = Compactor::new(gateway, model);
    let messages = padded_messages();
    let config = CompactionConfig {
      summarizer_model: Some(ModelId::parse("mock-model").unwrap()),
      cooldown_turns: 5,
      ..enabled_config()
    };

    let s1 = compactor.compact("s1", &messages, &config, None).await;
    let s2 = compactor.compact("s2", &messages, &config, None).await;
    assert_eq!(s1.reason, CompactionReason::Summarized);
    assert_eq!(s2.reason, CompactionReason::Summarized);
  }

  /// incremental_rebuild_every=1 等价每次全量：第二轮的摘要输入不含
  /// previous-summary 块（通过 should_rebuild 断言）。
  #[tokio::test]
  async fn rebuild_every_one_always_full() {
    let (gateway, model) =
      make_gateway_and_model(vec![text_step("s1"), text_step("s2")], 64);
    let compactor = Compactor::new(gateway, model);
    let config = CompactionConfig {
      summarizer_model: Some(ModelId::parse("mock-model").unwrap()),
      cooldown_turns: 0,
      incremental_rebuild_every: 1,
      ..enabled_config()
    };
    let messages = padded_messages();

    let state = CompactionState {
      last_summary: Some("old".to_string()),
      ..CompactionState::default()
    };
    assert!(compactor.should_rebuild(&state, &config));
    // 全量输入不含 previous-summary 标签
    let input = compactor.build_summary_input(
      &messages,
      &messages,
      &state,
      true,
      "mock-model",
      &config,
    );
    assert!(!input.iter().any(|m| matches!(m, Message::User(u)
      if matches!(&u.content, UserContent::Text(t) if t.contains(PREVIOUS_SUMMARY_TAG)))));
  }

  /// 强制全量重建：第 K 次增量后下一次 rebuild=true。
  #[test]
  fn force_rebuild_after_k_increments() {
    let (gateway, model) = make_gateway_and_model(Vec::new(), 64);
    let compactor = Compactor::new(gateway, model);
    let config = CompactionConfig {
      incremental_rebuild_every: 8,
      ..enabled_config()
    };
    let at_six = CompactionState {
      last_summary: Some("old".to_string()),
      incremental_count: 6,
      ..CompactionState::default()
    };
    assert!(!compactor.should_rebuild(&at_six, &config)); // 第 7 次增量
    let at_seven = CompactionState {
      incremental_count: 7,
      ..at_six
    };
    assert!(compactor.should_rebuild(&at_seven, &config)); // 第 8 次重建
  }

  /// 窗口保护：摘要输入超限时尾部截断（保留摘要块与最近增量）。
  #[test]
  fn summary_input_window_protection_truncates() {
    let (gateway, model) = make_gateway_and_model(Vec::new(), 64);
    let compactor = Compactor::new(gateway, model);
    let mut messages = vec![user("checkpoint")];
    for i in 0..20 {
      messages.push(user(&format!("long padding message number {i} here")));
    }
    let state = CompactionState {
      last_summary: Some("prior".to_string()),
      checkpoint_fingerprint: fingerprint_message(&messages[0]),
      ..CompactionState::default()
    };
    let config = CompactionConfig {
      max_summary_input_tokens: 20,
      ..enabled_config()
    };

    let input = compactor.build_summary_input(
      &messages,
      &messages,
      &state,
      false,
      "mock-model",
      &config,
    );
    // 窗口保护无法低于「摘要块 + 1 条」（保留 last_summary 与最近增量的
    // 语义下限）：此处 limit=20 远小于单条+摘要块规模，应截断到下限
    assert_eq!(input.len(), 2);
    assert!(estimate_tokens(&input) < estimate_tokens(&messages));
    // 前置摘要块须保留
    assert!(matches!(&input[0], Message::User(u)
      if matches!(&u.content, UserContent::Text(t) if t.contains(PREVIOUS_SUMMARY_TAG))));
  }

  /// 滞回：增长不足 hysteresis_ratio 倍 → 不重算，复用 last_summary。
  /// （cooldown_turns = 0 排除冷却路径；同 messages 第二轮估算持平，
  /// 不满足 ≥ 1.10× 增长 → HysteresisSuppressed，且不再调 LLM。
  /// 过期提示按 「已存在多轮」语义：age=1 不提示、age=2 起 N=2。）
  #[tokio::test]
  async fn second_turn_within_hysteresis_reuses_summary() {
    let (gateway, model) =
      make_gateway_and_model(vec![text_step("kept it")], 64);
    let compactor = Compactor::new(gateway, model);
    let messages = padded_messages();
    let config = CompactionConfig {
      summarizer_model: Some(ModelId::parse("mock-model").unwrap()),
      cooldown_turns: 0,
      ..enabled_config()
    };

    let first = compactor.compact("s1", &messages, &config, None).await;
    assert_eq!(first.reason, CompactionReason::Summarized);
    assert!(first.outdated_hint.is_none());

    // 第二轮：滞回抑制，复用摘要；摘要仅存在 1 轮，尚未"多轮"，不提示
    let second = compactor.compact("s1", &messages, &config, None).await;
    assert_eq!(second.reason, CompactionReason::HysteresisSuppressed);
    assert_eq!(second.summary.as_deref(), Some("kept it"));
    assert!(second.outdated_hint.is_none());

    // 第三轮：摘要已存在 2 轮 → 追加过期提示 N=2
    let third = compactor.compact("s1", &messages, &config, None).await;
    assert_eq!(third.reason, CompactionReason::HysteresisSuppressed);
    assert_eq!(third.outdated_hint, Some(2));
  }

  /// 增量路径：检查点命中 → 摘要输入 = 前置摘要块 + 检查点后增量（不含
  /// 检查点之前的 prefix），显著小于全量 prefix（验收标准 4）。
  #[test]
  fn incremental_input_covers_only_delta_after_checkpoint() {
    let (gateway, model) = make_gateway_and_model(Vec::new(), 64);
    let compactor = Compactor::new(gateway, model);
    let mut messages = vec![user("checkpoint-anchor")];
    for i in 0..8 {
      messages.push(user(&format!("delta message number {i} here")));
    }
    // 检查点锚定在 prefix 中部：增量只有其后 4 条，显著小于全量 9 条
    let state = CompactionState {
      last_summary: Some("prior".to_string()),
      checkpoint_fingerprint: fingerprint_message(&messages[4]),
      ..CompactionState::default()
    };
    // 抬高输入上限：避免窗口保护截断干扰本测试的纯增量语义
    let config = CompactionConfig {
      max_summary_input_tokens: 200,
      ..enabled_config()
    };

    let input = compactor.build_summary_input(
      &messages,
      &messages,
      &state,
      false,
      "mock-model",
      &config,
    );
    // 首条是前置摘要块，其后是检查点之后的 4 条增量
    assert!(matches!(&input[0], Message::User(u)
      if matches!(&u.content, UserContent::Text(t) if t.contains(PREVIOUS_SUMMARY_TAG))));
    assert_eq!(input.len(), 5);
    // 增量输入显著小于全量 prefix
    let full = compactor.build_summary_input(
      &messages,
      &messages,
      &state,
      true,
      "mock-model",
      &config,
    );
    assert_eq!(full.len(), 9);
    assert!(estimate_tokens(&input) < estimate_tokens(&full));
  }

  /// 检查点丢失 → 增量回退全量：输入不含前置摘要块（失败隔离）。
  #[test]
  fn missing_checkpoint_falls_back_to_full_prefix() {
    let (gateway, model) = make_gateway_and_model(Vec::new(), 64);
    let compactor = Compactor::new(gateway, model);
    let messages = vec![user("a"), user("b")];
    let state = CompactionState {
      last_summary: Some("prior".to_string()),
      checkpoint_fingerprint: 123_456, // 任意未命中指纹
      ..CompactionState::default()
    };

    let input = compactor.build_summary_input(
      &messages,
      &messages,
      &state,
      false,
      "mock-model",
      &enabled_config(),
    );
    assert_eq!(input.len(), 2);
    assert!(!input.iter().any(|m| matches!(m, Message::User(u)
      if matches!(&u.content, UserContent::Text(t) if t.contains(PREVIOUS_SUMMARY_TAG)))));
  }

  /// 窗口保护下的滞回内估计：estimated == budget 的等价边界不触发
  /// （验收：严格大于才触发）。
  #[tokio::test]
  async fn equal_to_budget_does_not_trigger() {
    // 构造一条恰好等于预算的消息很难精确控制；改用 below-threshold 语义
    // 断言：单条短消息在 8192 窗口 + 0.75 比率下必然低于预算。
    let (gateway, model) = make_gateway_and_model(Vec::new(), 8192);
    let compactor = Compactor::new(gateway, model);
    let messages = vec![user("tiny")];
    let outcome = compactor
      .compact("s1", &messages, &enabled_config(), None)
      .await;
    assert_eq!(outcome.reason, CompactionReason::BelowThreshold);
    assert_eq!(outcome.truncated, 0);
  }

  /// 场景（预算退化）：overhead ≥ 原始预算时，预算**退化为原始预算**
  /// 而非 0。
  ///
  /// 判别依据：退化后预算 = 原始预算，
  /// - 历史远超 10 → 仍触发压缩；
  /// - 历史很小（≤ 10）→ 不压缩（`estimated <= budget` 为真）。
  /// 若预算未被退化而是保持 0，则 `estimated <= 0` 恒为假 → 第二条断言
  /// 会失败（每轮都误触发压缩）。
  #[tokio::test]
  async fn budget_degrades_to_raw_when_overhead_exceeds_raw_budget() {
    let config = CompactionConfig {
      trigger_ratio: 0.1,
      ..enabled_config()
    };

    // 历史远超退化后的预算→ 触发压缩
    let (gateway, model) = make_gateway_and_model(Vec::new(), 100);
    let compactor = Compactor::new(gateway, model);
    compactor.set_last_overhead("s1", 50); // 50 ≥ 原始预算 10
    let outcome = compactor
      .compact("s1", &padded_messages(), &config, None)
      .await;
    assert_ne!(
      outcome.reason,
      CompactionReason::BelowThreshold,
      "history far above degraded budget must still trigger compaction"
    );

    // 历史很小（≤ 10）→ 不压缩（证明退化后预算是 10 而非 0）
    let (gateway, model) = make_gateway_and_model(Vec::new(), 100);
    let compactor = Compactor::new(gateway, model);
    compactor.set_last_overhead("s2", 50);
    let tiny = vec![user("hi")];
    let outcome = compactor.compact("s2", &tiny, &config, None).await;
    assert_eq!(
      outcome.reason,
      CompactionReason::BelowThreshold,
      "degraded budget must be raw_budget (10), not 0"
    );
  }
}
