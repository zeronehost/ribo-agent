//! 压缩状态：进程内 per-session 增量基 + 跨重启持久化子集。
//!
//! 承重约束（25 号方案 P1 / D4 / D11）：
//! - 压缩是「请求级视图」，增量状态是进程内派生数据，重启丢失可从完整转录重建；
//! - 只有四字段跨重启有价值（摘要文本 / 检查点指纹 / 摘要模型 / 生成时间），
//!   其余运行时字段重启一律归零（滞回基准重算、冷却消失、重建计数从零开始）；
//! - JSONL 中的摘要记录是纯缓存旁路记录：不截断 message、不决定模型读取起点。

use ra_core::message::{Content, Message, UserContent};
use serde::{Deserialize, Serialize};

/// 跨重启持久化的摘要缓存（JSONL `compaction` 记录承载的四字段，D11 / P1-3）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionCache {
  /// 上次成功摘要文本（已按 `max_summary_chars` 截断）。
  pub last_summary: String,
  /// 摘要覆盖到的 prefix 末尾消息指纹（FNV-1a 64）。
  pub checkpoint_fingerprint: u64,
  /// 生成该摘要的模型 ID 字符串（模型变更即失效）。
  pub summarizer_model: String,
  /// 摘要生成时间（Unix 毫秒）。
  pub generated_at: i64,
}

/// 供 `HarnessContext::build` 读取的只读快照（D2 / P1-2）。
///
/// 只暴露 overhead 基准，不暴露冷却 / 摘要等内部字段；
/// `HarnessContext` 不直接持有 [`CompactionState`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CompactionStateSnapshot {
  /// 上一轮 system prompt 的估算 token（循环依赖破解基准）。
  pub last_overhead: usize,
}

/// 单次压缩会话状态（进程内，按 session key 隔离）。
///
/// 全零 / None 即默认值：运行时字段重启重置（P1-3）的语义恰好与
/// `Default` 一致，直接派生。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CompactionState {
  /// 上次触发时的估算 token（D3 滞回基准；0 表示从未触发）。
  pub last_trigger_tokens: usize,
  /// 冷却剩余轮数（D3）。
  pub cooldown_turns_left: u32,
  /// 上次摘要文本（D4 增量基）。
  pub last_summary: Option<String>,
  /// 上次摘要覆盖到的消息指纹（prefix 末尾消息）。
  pub checkpoint_fingerprint: u64,
  /// 上次被移出的消息条数（保持 truncated 可观测一致）。
  pub last_truncated: usize,
  /// 已完成的增量次数（D4 强制重建计数）。
  pub incremental_count: u32,
  /// 上一轮 system prompt 的估算 token（D2 循环依赖破解）。
  pub last_overhead: usize,
  /// 自上次摘要以来的 build 轮数（D6 过期提示的 N；摘要成功时归零）。
  pub summary_age_turns: u32,
}

impl CompactionState {
  /// 由持久缓存恢复状态：只还原四字段，运行时字段全部归零。
  ///
  /// # Arguments
  ///
  /// - `cache`：从 JSONL 最后一条 `compaction` 记录解析出的缓存。
  pub fn from_cache(cache: &CompactionCache) -> Self {
    Self {
      last_summary: Some(cache.last_summary.clone()),
      checkpoint_fingerprint: cache.checkpoint_fingerprint,
      // 以下运行时字段重启后重置（P1-3）
      ..Self::default()
    }
  }

  /// 导出可持久化子集：仅当存在摘要文本时返回（摘要失败不写记录，D11）。
  ///
  /// # Arguments
  ///
  /// - `summarizer_model`：本次生成摘要的模型 ID 字符串。
  /// - `generated_at`：生成时间（Unix 毫秒）。
  pub fn to_cache(
    &self,
    summarizer_model: &str,
    generated_at: i64,
  ) -> Option<CompactionCache> {
    self.last_summary.as_ref().map(|summary| CompactionCache {
      last_summary: summary.clone(),
      checkpoint_fingerprint: self.checkpoint_fingerprint,
      summarizer_model: summarizer_model.to_string(),
      generated_at,
    })
  }

  /// 读取 overhead 基准的只读快照。
  pub fn snapshot(&self) -> CompactionStateSnapshot {
    CompactionStateSnapshot {
      last_overhead: self.last_overhead,
    }
  }
}

/// 计算消息指纹（FNV-1a 64 位，跨 Rust 版本稳定）。
///
/// 对消息序列化后的字节做哈希。**序列化失败时不得静默降级**——旧实现
/// `unwrap_or_default()` 会让所有失败消息得到同一个指纹（空串哈希），
/// 破坏检查点唯一性、导致 `locate_checkpoint` 匹配到错误位置（增量摘要
/// 取错增量范围）。现改为：记录告警并退回**元数据指纹**（角色 + 块数 +
/// 各块文本长度 + 工具标识），区分度远高于空串，且不把消息正文写入日志。
///
/// 触发条件说明：当前 `Message` 使用派生序列化，且 serde_json 把
/// NaN / Infinity 写为 `null`，故实际不会失败——此分支为防御性保留
/// （未来引入可能失败的自定义序列化时生效），但「绝不静默降级」的
/// 语义必须成立。
///
/// 用于检查点定位与 D11 失效判定（重启后与 transcript 比对）。
pub fn fingerprint_message(message: &Message) -> u64 {
  match serde_json::to_string(message) {
    Ok(text) => fnv1a64(text.as_bytes()),
    Err(err) => {
      tracing::warn!(
        error = %err,
        "message serialization failed; falling back to metadata fingerprint"
      );
      // 前缀标记：避免与正常消息的指纹混淆
      let source = format!("fallback:{}", metadata_fingerprint_source(message));
      fnv1a64(source.as_bytes())
    }
  }
}

/// 序列化失败时的元数据指纹源（不含消息正文，避免敏感内容外泄）。
///
/// 组成：角色标签 + 内容块数 + 每块的「类型:文本长度」+ 工具标识
/// （工具名与调用 id）。长度用字符数而非字节数，保证跨编码稳定。
fn metadata_fingerprint_source(message: &Message) -> String {
  fn block_tag(block: &Content) -> String {
    let len = match block {
      Content::Thinking(text) => text.content.chars().count(),
      Content::Text(text) => text.content.chars().count(),
      Content::Image(_) => 0,
      Content::ToolCall(call) => {
        // arguments 是 serde_json::Value（不是字符串）→ 取其 JSON 文本长度
        call.arguments.to_string().chars().count()
      }
    };
    let kind = match block {
      Content::Thinking(_) => "think",
      Content::Text(_) => "text",
      Content::Image(_) => "image",
      Content::ToolCall(_) => "call",
    };
    format!("{kind}:{len}")
  }

  match message {
    Message::User(user) => {
      let mut parts = vec!["user".to_string()];
      match &user.content {
        UserContent::Text(text) => {
          parts.push(format!("text:{}", text.chars().count()));
        }
        UserContent::Blocks(blocks) => {
          parts.push(format!("blocks:{}", blocks.len()));
          for block in blocks {
            parts.push(block_tag(block));
          }
        }
      }
      parts.join("|")
    }
    Message::Assistant(assistant) => {
      let mut parts = vec!["assistant".to_string()];
      parts.push(format!("blocks:{}", assistant.content.len()));
      for block in &assistant.content {
        parts.push(block_tag(block));
      }
      parts.join("|")
    }
    Message::ToolResult(result) => {
      let mut parts = vec!["tool_result".to_string()];
      parts.push(format!("tool:{}", result.tool_name));
      parts.push(format!("call:{}", result.tool_call_id));
      parts.push(format!("error:{}", result.is_error));
      parts.push(format!("blocks:{}", result.content.len()));
      for block in &result.content {
        parts.push(block_tag(block));
      }
      parts.join("|")
    }
  }
}

/// 在完整消息序列中定位指纹对应的检查点下标。
///
/// 从后向前匹配（多条同指纹时取最近的），找不到返回 `None`
/// → 增量路径回退全量（D4 检查点丢失）。
pub fn locate_checkpoint(
  messages: &[Message],
  fingerprint: u64,
) -> Option<usize> {
  messages
    .iter()
    .rposition(|message| fingerprint_message(message) == fingerprint)
}

/// FNV-1a 64 位哈希（纯 std，无依赖，跨版本确定）。
fn fnv1a64(bytes: &[u8]) -> u64 {
  const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
  const PRIME: u64 = 0x0000_0100_0000_01b3;
  let mut hash = OFFSET_BASIS;
  for &byte in bytes {
    hash ^= u64::from(byte);
    hash = hash.wrapping_mul(PRIME);
  }
  hash
}

#[cfg(test)]
mod tests {
  use ra_core::message::{
    AssistantMessage, TextContent, UserContent, UserMessage,
  };

  use super::*;

  fn user(text: &str) -> Message {
    Message::User(UserMessage {
      content: UserContent::Text(text.to_string()),
      timestamp: 0,
    })
  }

  // 场景：指纹对同一消息稳定、对不同消息不同（检查点定位基础）。
  // 预期：同文本消息指纹相等，异文本不等，空指纹常量为 0。
  #[test]
  fn fingerprint_is_stable_and_discriminating() {
    assert_eq!(
      fingerprint_message(&user("hello")),
      fingerprint_message(&user("hello"))
    );
    assert_ne!(
      fingerprint_message(&user("hello")),
      fingerprint_message(&user("world"))
    );
  }

  /// 构造一条带 NaN 费用的助手消息。
  ///
  /// 注意：**serde_json 把 NaN / Infinity 序列化为 `null`**（不是报错），
  /// 因此 `Message` 的派生序列化实际不会失败——本函数用于构造
  /// 「内容差异明显」的消息，验证元数据指纹的区分度。
  fn assistant_with_nan_cost(text: &str) -> Message {
    let mut assistant = AssistantMessage::empty("prov", "model", 0);
    assistant.usage.cost.total = f64::NAN;
    assistant.content = vec![Content::Text(TextContent {
      content: text.to_string(),
      signature: None,
    })];
    Message::Assistant(assistant)
  }

  // 场景：序列化失败时的降级源（元数据指纹）必须稳定、区分不同消息、
  // 且不含消息正文。
  //
  // 前提说明：serde_json 将 NaN / Infinity 写为 `null`，故 `Message` 的
  // 派生序列化当前不会失败——fallback 分支是防御性保留（未来 Message
  // 引入可能失败的自定义序列化时生效）。因此此处直接验证降级源本身。
  #[test]
  fn metadata_fingerprint_source_is_stable_and_discriminating() {
    let first = assistant_with_nan_cost("alpha");
    let second = assistant_with_nan_cost("bravo-longer");

    let source_first = metadata_fingerprint_source(&first);
    let source_second = metadata_fingerprint_source(&second);

    // 同一条消息的元数据源稳定
    assert_eq!(source_first, metadata_fingerprint_source(&first));
    // 不同内容的消息 → 不同元数据源（旧实现会退化为同一个空串哈希）
    assert_ne!(source_first, source_second);
    // 不含消息正文（避免敏感内容进入指纹源 / 日志）
    assert!(!source_first.contains("alpha"));
    assert!(!source_second.contains("bravo"));
    // fallback 指纹不等于旧实现的空串哈希值
    let fallback = fnv1a64(format!("fallback:{source_first}").as_bytes());
    assert_ne!(fallback, fnv1a64(b""));
  }

  // 场景：正常消息（可序列化）指纹路径不受降级逻辑影响 —— 回归护栏。
  #[test]
  fn fingerprint_of_serializable_message_unchanged() {
    let message = user("hello");
    assert!(serde_json::to_string(&message).is_ok());
    // 正常路径 = 直接对序列化字节取哈希（非 fallback 前缀）
    let expected = fnv1a64(serde_json::to_string(&message).unwrap().as_bytes());
    assert_eq!(fingerprint_message(&message), expected);
  }

  // 场景：检查点定位——同指纹多条时取最近，找不到返回 None。
  // 预期：rposition 语义；缺失指纹触发全量回退。
  #[test]
  fn locate_checkpoint_prefers_latest_and_handles_missing() {
    let messages = vec![user("a"), user("b"), user("a")];
    let fp_a = fingerprint_message(&user("a"));
    assert_eq!(locate_checkpoint(&messages, fp_a), Some(2));
    assert_eq!(locate_checkpoint(&messages, 9_999), None);
  }

  // 场景：from_cache 只还原四字段中的两项到运行时状态，其余归零。
  // 预期：last_summary / checkpoint 还原，冷却 / 重建计数 / overhead / age 全零。
  #[test]
  fn from_cache_resets_runtime_fields() {
    let cache = CompactionCache {
      last_summary: "kept".to_string(),
      checkpoint_fingerprint: 42,
      summarizer_model: "glm".to_string(),
      generated_at: 100,
    };
    let state = CompactionState::from_cache(&cache);
    assert_eq!(state.last_summary.as_deref(), Some("kept"));
    assert_eq!(state.checkpoint_fingerprint, 42);
    assert_eq!(state.cooldown_turns_left, 0);
    assert_eq!(state.incremental_count, 0);
    assert_eq!(state.last_overhead, 0);
    assert_eq!(state.summary_age_turns, 0);
    assert_eq!(state.last_trigger_tokens, 0);
  }

  // 场景：to_cache 仅在有摘要时产出，且只含四字段。
  // 预期：无摘要返回 None；有摘要时 model / ts 原样带入，不含运行时字段。
  #[test]
  fn to_cache_emits_only_when_summary_present() {
    let mut state = CompactionState::default();
    assert!(state.to_cache("glm", 100).is_none());

    state.last_summary = Some("kept".to_string());
    state.checkpoint_fingerprint = 7;
    state.incremental_count = 3; // 运行时字段不应影响缓存
    let cache = state.to_cache("glm-4.5", 200).unwrap();
    assert_eq!(cache.last_summary, "kept");
    assert_eq!(cache.checkpoint_fingerprint, 7);
    assert_eq!(cache.summarizer_model, "glm-4.5");
    assert_eq!(cache.generated_at, 200);
  }

  // 场景：快照只暴露 overhead。
  #[test]
  fn snapshot_exposes_only_overhead() {
    let state = CompactionState {
      last_overhead: 320,
      last_summary: Some("secret".to_string()),
      ..CompactionState::default()
    };
    let snapshot = state.snapshot();
    assert_eq!(snapshot.last_overhead, 320);
  }
}
