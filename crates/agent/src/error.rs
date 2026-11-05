//! Agent 域错误码与构造辅助（D7）。
//!
//! 常量命名沿用 ra-provider 惯例：常量名大写、值点分小写，
//! 与 `domain = "agent"` 配对；每个构造函数统一设置严重程度并附加
//! 结构化 context（仅内部键值，不含敏感信息）。
//!
//! 工具失败**不**转 `agent.tool_exec_error`——D8 回灌路径上错误以
//! `ToolResultMessage { is_error: true }` 表达；[`RiboError`] 仅用于
//! 循环自身的结构性失败（会话、上下文、迭代超限等）。

use ra_core::error::{RiboError, Severity};

/// 错误域标识：所有 agent 域错误共用。
pub(crate) const DOMAIN: &str = "agent";

/// 会话不存在。
pub const SESSION_NOT_FOUND: &str = "agent.session_not_found";
/// 会话创建失败。
pub const SESSION_CREATE_FAILED: &str = "agent.session_create_failed";
/// 会话已有活动运行（重入拒绝）。
pub const SESSION_BUSY: &str = "agent.session_busy";

/// 超过最大迭代轮数。
pub const MAX_ITERATIONS: &str = "agent.max_iterations";
/// 请求被取消。
pub const CANCELLED: &str = "agent.cancelled";

/// 上下文构建错误。
pub const CONTEXT_BUILD_ERROR: &str = "agent.context_build_error";
/// 工具执行错误（聚合上报场景）。
pub const TOOL_EXEC_ERROR: &str = "agent.tool_exec_error";

/// 检索服务错误（预留，本期不产生）。
pub const RETRIEVAL_ERROR: &str = "agent.retrieval_error";
/// 子代理错误（预留，本期不产生）。
pub const SUBAGENT_ERROR: &str = "agent.subagent_error";

/// 构造「会话不存在」错误（Warning：调用方可先建会话后重试）。
pub fn session_not_found(key: impl Into<String>) -> RiboError {
  let key = key.into();
  RiboError::new(
    DOMAIN,
    SESSION_NOT_FOUND,
    format!("session not found: {key}"),
  )
  .with_severity(Severity::Warning)
  .with_context("session_key", key)
}

/// 构造「会话创建失败」错误（Error：存储层故障，需人工介入）。
pub fn session_create_failed(
  key: impl Into<String>,
  reason: impl Into<String>,
) -> RiboError {
  let key = key.into();
  let reason = reason.into();
  RiboError::new(
    DOMAIN,
    SESSION_CREATE_FAILED,
    format!("session creation failed for {}: {}", key, reason),
  )
  .with_severity(Severity::Error)
  .with_context("session_key", key)
  .with_context("reason", reason)
}

/// 构造「会话忙」错误（Debug：正常重入拒绝，属预期流量形态）。
pub fn session_busy(key: impl Into<String>) -> RiboError {
  let key = key.into();
  RiboError::new(
    DOMAIN,
    SESSION_BUSY,
    format!("session already running: {key}"),
  )
  .with_severity(Severity::Debug)
  .with_context("session_key", key)
}

/// 构造「迭代超限」错误（Debug：循环保护触发，已产出消息保留在转录）。
pub fn max_iterations(turns: usize, limit: usize) -> RiboError {
  RiboError::new(
    DOMAIN,
    MAX_ITERATIONS,
    format!("max iterations reached: {turns} > {limit}"),
  )
  .with_severity(Severity::Debug)
  .with_context("turns", turns.to_string())
  .with_context("limit", limit.to_string())
}

/// 构造「运行被取消」错误（Debug：取消不是故障路径）。
pub fn cancelled(key: impl Into<String>) -> RiboError {
  let key = key.into();
  RiboError::new(DOMAIN, CANCELLED, format!("run cancelled: {key}"))
    .with_severity(Severity::Debug)
    .with_context("session_key", key)
}

/// 构造「上下文构建失败」错误（Error：转录状态不满足续跑前置）。
pub fn context_build_error(
  key: impl Into<String>,
  reason: impl Into<String>,
) -> RiboError {
  let key = key.into();
  let reason = reason.into();
  RiboError::new(
    DOMAIN,
    CONTEXT_BUILD_ERROR,
    format!("context build failed for {key}: {reason}"),
  )
  .with_severity(Severity::Error)
  .with_context("session_key", key)
  .with_context("reason", reason)
}

/// 构造「工具执行错误」（Warning：聚合上报场景使用，不进入回灌路径）。
pub fn tool_exec_error(message: impl Into<String>) -> RiboError {
  let message = message.into();
  RiboError::new(
    DOMAIN,
    TOOL_EXEC_ERROR,
    format!("tool exec error: {message}"),
  )
  .with_severity(Severity::Warning)
  .with_context("reason", message)
}

#[cfg(test)]
mod tests {
  use ra_core::error::Severity;

  use super::*;

  // 场景：session_not_found 构造——会话不存在。
  // 预期：域为 agent、码为 agent.session_not_found、严重程度 Warning、
  // context 携带 session_key，消息含会话键。
  #[test]
  fn session_not_found_sets_code_severity_and_context() {
    let err = session_not_found("s1");

    assert_eq!(err.domain(), DOMAIN);
    assert_eq!(err.code(), SESSION_NOT_FOUND);
    assert_eq!(err.severity(), &Severity::Warning);
    assert_eq!(err.message(), "session not found: s1");
    let context = err.context().expect("应携带 context");
    assert_eq!(context.get("session_key").map(String::as_str), Some("s1"));
  }

  // 场景：session_create_failed 构造——会话创建失败。
  // 预期：严重程度 Error，context 同时携带 session_key 与 reason。
  #[test]
  fn session_create_failed_sets_code_severity_and_context() {
    let err = session_create_failed("s1", "disk full");

    assert_eq!(err.code(), SESSION_CREATE_FAILED);
    assert_eq!(err.severity(), &Severity::Error);
    let context = err.context().expect("应携带 context");
    assert_eq!(context.get("session_key").map(String::as_str), Some("s1"));
    assert_eq!(context.get("reason").map(String::as_str), Some("disk full"));
  }

  // 场景：session_busy 构造——重入拒绝。
  // 预期：严重程度 Debug（预期流量形态），context 携带 session_key。
  #[test]
  fn session_busy_sets_code_severity_and_context() {
    let err = session_busy("s1");

    assert_eq!(err.code(), SESSION_BUSY);
    assert_eq!(err.severity(), &Severity::Debug);
    let context = err.context().expect("应携带 context");
    assert_eq!(context.get("session_key").map(String::as_str), Some("s1"));
  }

  // 场景：max_iterations 构造——迭代超限。
  // 预期：严重程度 Debug，消息含轮数对比，context 携带 turns 与 limit。
  #[test]
  fn max_iterations_sets_code_severity_and_context() {
    let err = max_iterations(3, 2);

    assert_eq!(err.code(), MAX_ITERATIONS);
    assert_eq!(err.severity(), &Severity::Debug);
    assert_eq!(err.message(), "max iterations reached: 3 > 2");
    let context = err.context().expect("应携带 context");
    assert_eq!(context.get("turns").map(String::as_str), Some("3"));
    assert_eq!(context.get("limit").map(String::as_str), Some("2"));
  }

  // 场景：cancelled 构造——运行被取消。
  // 预期：严重程度 Debug（取消不是故障路径），context 携带 session_key。
  #[test]
  fn cancelled_sets_code_severity_and_context() {
    let err = cancelled("s1");

    assert_eq!(err.code(), CANCELLED);
    assert_eq!(err.severity(), &Severity::Debug);
    let context = err.context().expect("应携带 context");
    assert_eq!(context.get("session_key").map(String::as_str), Some("s1"));
  }

  // 场景：context_build_error 构造——上下文构建失败。
  // 预期：严重程度 Error，context 同时携带 session_key 与 reason。
  #[test]
  fn context_build_error_sets_code_severity_and_context() {
    let err = context_build_error("s1", "empty transcript");

    assert_eq!(err.code(), CONTEXT_BUILD_ERROR);
    assert_eq!(err.severity(), &Severity::Error);
    let context = err.context().expect("应携带 context");
    assert_eq!(context.get("session_key").map(String::as_str), Some("s1"));
    assert_eq!(
      context.get("reason").map(String::as_str),
      Some("empty transcript")
    );
  }

  // 场景：tool_exec_error 构造——聚合上报场景。
  // 预期：严重程度 Warning，context 携带 reason（不进入回灌路径）。
  #[test]
  fn tool_exec_error_sets_code_severity_and_context() {
    let err = tool_exec_error("injected failure");

    assert_eq!(err.code(), TOOL_EXEC_ERROR);
    assert_eq!(err.severity(), &Severity::Warning);
    let context = err.context().expect("应携带 context");
    assert_eq!(
      context.get("reason").map(String::as_str),
      Some("injected failure")
    );
  }
}
