//! OpenAI 系线格式的错误体映射。
//!
//! SSE 传输层（`SseFrame` / `SseParser`）已上移到 [`crate::stream::sse`]，
//! 这里保留 OpenAI 系错误体 → 内部错误码的映射，并以 `pub(super)` 重导出
//! 传输类型，供 `openai::{completion, responses}` 两个子树直接引用。

use crate::error::{
  AUTH_FAILED, CONTENT_FILTERED, INVALID_REQUEST, QUOTA_EXCEEDED, RATE_LIMITED,
  SERVER_ERROR, TIMEOUT,
};

/// 提供方私有错误体 → `&'static str` 错误码（始终以
/// `producer.error(StopReason::Error, .., Some(code))` 终结流）。
///
/// 映射优先级：**显式 `code` / `type` 字段 > `message` 关键词**；
/// 返回 `None` 表示无法分类，`Error` 事件的 `code` 落为 `None`
/// （事件模型允许），消费端按未分类流内错误处置。
///
/// 内容安全拦截必须映射为 [`CONTENT_FILTERED`]，不得归入
/// [`TRANSPORT`] 类可重试故障——两者处置方式相反（改写输入 vs 重试）。
pub(super) fn map_provider_error(
  body: &serde_json::Value,
) -> Option<&'static str> {
  // ===== 第一级：显式字段精确匹配 =====
  // `code` 与 `type` 各提供方取值不一，这里维护并集表；
  // 未识别的显式码不猜测类别，直接落入关键词兜底。
  let explicit = body
    .get("code")
    .and_then(serde_json::Value::as_str)
    .or_else(|| body.get("type").and_then(serde_json::Value::as_str));
  if let Some(raw) = explicit {
    let code = match raw {
      // 请求形态错误（含上下文超长——重试无法恢复）
      "invalid_request_error" | "context_length_exceeded" => INVALID_REQUEST,
      // 鉴权类
      "authentication_error" | "invalid_api_key" | "permission_error" => {
        AUTH_FAILED
      }
      // 配额类
      "insufficient_quota" => QUOTA_EXCEEDED,
      // 限流类
      "rate_limit_error" | "rate_limit_exceeded" => RATE_LIMITED,
      // 服务端类
      "api_error" | "server_error" | "overloaded_error" => SERVER_ERROR,
      // 超时类
      "timeout" | "timeout_error" => TIMEOUT,
      // 内容安全类
      "content_filter" | "content_management_policy" => CONTENT_FILTERED,
      // 未识别：不映射，交给关键词兜底
      _ => return map_provider_error_by_message(body),
    };
    return Some(code);
  }

  // ===== 第二级：message 关键词兜底 =====
  map_provider_error_by_message(body)
}

/// `message` 关键词兜底映射（显式字段缺失或未识别时调用）。
///
/// 命中顺序即优先级：内容安全 > 限流 > 配额 > 鉴权 > 超时——
/// 内容安全最优先，防止被误判为可重试故障后重放被拦截的输入。
/// 匹配在 `message` 小写化后做子串查找；除本函数外任何逻辑
/// 不得匹配错误文本内容（P2：结构化优先于字符串）。
fn map_provider_error_by_message(
  body: &serde_json::Value,
) -> Option<&'static str> {
  let message = body.get("message").and_then(serde_json::Value::as_str)?;
  let lowered = message.to_lowercase();

  if lowered.contains("content filter")
    || lowered.contains("content_filter")
    || lowered.contains("content management policy")
    || lowered.contains("safety system")
  {
    return Some(CONTENT_FILTERED);
  }
  if lowered.contains("rate limit") {
    return Some(RATE_LIMITED);
  }
  if lowered.contains("quota") {
    return Some(QUOTA_EXCEEDED);
  }
  if lowered.contains("api key") || lowered.contains("unauthorized") {
    return Some(AUTH_FAILED);
  }
  if lowered.contains("timed out") || lowered.contains("timeout") {
    return Some(TIMEOUT);
  }
  None
}
