//! 工具域错误：错误码常量与构造辅助。
//!
//! 全部错误码以 `tool.` 为前缀；构造函数按语义预设
//! [`Severity`](ra_core::error::Severity) 并附 `tool_name` /
//! `reason` context，供上层分类与排障。

use ra_core::error::{RiboError, Severity};

/// 错误域标识：所有工具域错误共用的 domain 字段。
pub(crate) const DOMAIN: &str = "tool";

/// 错误码：工具未注册。
pub const NOT_FOUND: &str = "tool.not_found";
/// 错误码：同名工具重复注册。
pub const ALREADY_REGISTERED: &str = "tool.already_registered";

/// 错误码：入参不满足工具声明（含 Schema 本身非法）。
pub const INVALID_ARGUMENTS: &str = "tool.invalid_arguments";
/// 错误码：工具执行失败（IO、外部服务等）。
pub const EXEC_FAILED: &str = "tool.exec_failed";
/// 错误码：执行被取消。
pub const CANCELLED: &str = "tool.cancelled";
/// 错误码：熔断打开，调用被拒绝。
pub const RATE_LIMITED: &str = "tool.rate_limited";

/// 构造「工具未注册」错误（Severity: Error）。
pub fn not_found(tool_name: impl Into<String>) -> RiboError {
  let name = tool_name.into();
  RiboError::new(DOMAIN, NOT_FOUND, format!("tool not found: {name}"))
    .with_severity(Severity::Error)
    .with_context("tool_name", name)
}

/// 构造「入参非法」错误（Severity: Error）。
///
/// `reason` 应指向具体问题（缺失字段 / 校验条目），供模型定位修正。
pub fn invalid_arguments(
  tool_name: impl Into<String>,
  reason: impl Into<String>,
) -> RiboError {
  let tool_name = tool_name.into();
  let reason = reason.into();
  RiboError::new(
    DOMAIN,
    INVALID_ARGUMENTS,
    format!("invalid arguments for tool {tool_name}: {reason}"),
  )
  .with_severity(Severity::Error)
  .with_context("tool_name", tool_name)
  .with_context("reason", reason)
}

/// 构造「重复注册」错误（Severity: Warning——已有同名工具在位，本次是幂等冲突）。
pub fn already_registered(tool_name: impl Into<String>) -> RiboError {
  let tool_name = tool_name.into();
  RiboError::new(
    DOMAIN,
    ALREADY_REGISTERED,
    format!("tool already registered: {tool_name}"),
  )
  .with_severity(Severity::Warning)
  .with_context("tool_name", tool_name)
}

/// 构造「执行失败」错误（Severity: Error）。
pub fn exec_failed(
  tool_name: impl Into<String>,
  message: impl Into<String>,
) -> RiboError {
  let tool_name = tool_name.into();
  let message = message.into();
  RiboError::new(
    DOMAIN,
    EXEC_FAILED,
    format!("tool execution failed for {tool_name}: {message}"),
  )
  .with_severity(Severity::Error)
  .with_context("tool_name", tool_name)
  .with_context("reason", message)
}

/// 构造「熔断拒绝」错误（Severity: Warning——保护性拒绝，非工具故障）。
pub fn rate_limited(tool_name: impl Into<String>) -> RiboError {
  let tool_name = tool_name.into();
  RiboError::new(
    DOMAIN,
    RATE_LIMITED,
    format!("tool circuit open, rejected: {tool_name}"),
  )
  .with_severity(Severity::Warning)
  .with_context("tool_name", tool_name)
}

/// 构造「执行被取消」错误（Severity: Warning——调用方主动放弃）。
pub fn cancelled(tool_name: impl Into<String>) -> RiboError {
  let tool_name = tool_name.into();
  RiboError::new(
    DOMAIN,
    CANCELLED,
    format!("tool execution cancelled: {tool_name}"),
  )
  .with_severity(Severity::Warning)
  .with_context("tool_name", tool_name)
}
