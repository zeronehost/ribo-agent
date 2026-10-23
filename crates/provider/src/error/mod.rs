//! ra-provider 统一错误模块。
//!
//! 本模块为整个 crate 提供一套本地化的错误设施：
//!
//! 1. `constants` 子模块：17 个 `&'static str` 错误码常量，取值形如
//!    `"provider.auth_failed"`，与 `RiboError::domain = "provider"` 配对。
//! 2. `stage` 子模块：`ErrorStage` 枚举，区分 PreStream / InStream 生命周期。
//! 3. 本文件：构造器（`pre_stream` / `in_stream` / `from_http` / `from_reqwest` /
//!    `from_stream_terminal`）+ [`ProviderErrorExt`] trait 扩展，实现阶段门自动纠正
//!    与分类谓词。
//!
//! # 阶段门
//!
//! 构造器在运行期自动拦截非法的 code/阶段配对：
//!
//! - `pre_stream` 会把 `STREAM_INTERRUPTED → TRANSPORT`、
//!   `CONTENT_FILTERED → INVALID_REQUEST`；
//! - `in_stream` 会把 PreStream 专属 code（`PROVIDER_NOT_FOUND` 等网关错误）
//!   统一改写为 `STREAM_INTERRUPTED`。
//!
//! 这保证了即使调用方误把"建流专属 code"放进流内，最终也会落成合法 code。

mod constants;
mod stage;

use ra_core::error::{RiboError, Severity};

use crate::traits::ProviderErrorExt;

pub use self::{constants::*, stage::*};

// ============================================================================
// 分类谓词：ProviderErrorExt for RiboError
// ============================================================================
//
// 统一用 `self.code()` 与常量做二元组匹配。阶段门短路规则：
// - InStream 阶段一律不可重试/降级/跨 provider 降级（除 circuit_trigger 外），
//   因为重试会与已产出的内容重复，降级对同一 provider 无意义。
// - circuit_trigger 不受阶段门影响：流内错误同样可能触发熔断（如 SSE 中断）。

impl ProviderErrorExt for RiboError {
  fn stage(&self) -> ErrorStage {
    self
      .context()
      .and_then(|c| c.get("stage").and_then(ErrorStage::from))
      // 旧错误可能没有 stage context，保守按 PreStream 处理
      .unwrap_or(ErrorStage::PreStream)
  }

  fn is_retryable(&self) -> bool {
    // ===== 阶段门短路 =====
    // 流内一律不可重试——重试会重新发起整个请求，与已产出的内容重复。
    if self.stage() == ErrorStage::InStream {
      return false;
    }
    matches!(
      self.code(),
      RATE_LIMITED | TIMEOUT | SERVER_ERROR | TRANSPORT | OVERLOADED
    )
  }

  fn is_degradable(&self) -> bool {
    // ===== 阶段门短路 =====
    if self.stage() == ErrorStage::InStream {
      return false;
    }
    matches!(
      self.code(),
      RATE_LIMITED | TIMEOUT | SERVER_ERROR | AUTH_FAILED | QUOTA_EXCEEDED
    )
  }

  fn is_cross_provider_degradable(&self) -> bool {
    // ===== 阶段门短路 =====
    if self.stage() == ErrorStage::InStream {
      return false;
    }
    // 换 provider 能解决：限流（配额耗尽）、鉴权（可能是该 provider 的 key 有问题）、
    // 配额（该 provider 额度用完，换一家继续）。ServerError / Timeout / Transport
    // 等跨 provider 意义不大（如果是网络问题，哪家都挂）。
    matches!(self.code(), RATE_LIMITED | AUTH_FAILED | QUOTA_EXCEEDED)
  }

  fn is_circuit_trigger(&self) -> bool {
    // ===== 不受阶段门影响 =====
    // 流内错误同样可能触发熔断——例如 SSE 连接反复中断说明上游不稳定。
    matches!(
      self.code(),
      RATE_LIMITED
        | TIMEOUT
        | SERVER_ERROR
        | TRANSPORT
        | INVALID_RESPONSE
        | STREAM_INTERRUPTED
        | CIRCUIT_OPEN
        | OVERLOADED
    )
  }
}

// ============================================================================
// Severity 映射与阶段门内部函数
// ============================================================================

/// 按错误码取默认严重程度。
///
/// 映射规则：
/// - `INVALID_REQUEST` / `CONTENT_FILTERED` → `Severity::Warning`（可恢复）
/// - `CANCELLED` → `Severity::Debug`（用户行为，非故障）
/// - 网关专属 code（`PROVIDER_NOT_FOUND` 等）→ `Severity::Critical`（配置/注册异常）
/// - 其余默认 `Severity::Error`
fn default_severity(code: &str) -> Severity {
  match code {
    INVALID_REQUEST | CONTENT_FILTERED => Severity::Warning,
    CANCELLED => Severity::Debug,
    PROVIDER_NOT_FOUND
    | MODEL_NOT_FOUND
    | DUPLICATE_PROVIDER
    | RETRY_PROVIDER_REFUSED
    | CIRCUIT_OPEN
    | OVERLOADED => Severity::Critical,
    _ => Severity::Error,
  }
}

/// 把非法的 InStream code 纠正为合法值。
///
/// PreStream 专属 code（`PROVIDER_NOT_FOUND` / `MODEL_NOT_FOUND` /
/// `CIRCUIT_OPEN` / `OVERLOADED` / `DUPLICATE_PROVIDER` / `RETRY_PROVIDER_REFUSED`）
/// 在流内没有对应语义，统一改写为 `STREAM_INTERRUPTED`——
/// 流在任何阶段被异常中断都可归为此类。
fn coerce_for_in_stream(code: &'static str) -> &'static str {
  match code {
    INVALID_REQUEST | TIMEOUT | SERVER_ERROR | TRANSPORT | INVALID_RESPONSE
    | CONTENT_FILTERED | STREAM_INTERRUPTED | CANCELLED => code,
    _ => STREAM_INTERRUPTED,
  }
}

/// 把非法的 PreStream code 纠正为合法值。
///
/// 流专属 code 出现在 PreStream 的唯一可能是调用方手误，
/// 这里做保守映射：`CONTENT_FILTERED` → `INVALID_REQUEST`
/// （内容还没开始就被过滤 ≈ 请求本身非法），
/// `STREAM_INTERRUPTED` → `TRANSPORT`（流没建立就断了就是传输问题）。
fn coerce_for_pre_stream(code: &'static str) -> &'static str {
  match code {
    CONTENT_FILTERED => INVALID_REQUEST,
    STREAM_INTERRUPTED => TRANSPORT,
    _ => code,
  }
}

// ============================================================================
// 公开构造器
// ============================================================================
//
// 所有构造器都通过 `with_context("stage", ..)` 注入阶段信息，
// 供 `ProviderErrorExt::stage()` 读回以驱动后续分类。

/// 构造 PreStream 阶段错误。
///
/// 传入 InStream 专属 code（`CONTENT_FILTERED` / `STREAM_INTERRUPTED`）时，
/// 运行期改写为 `INVALID_REQUEST` / `TRANSPORT`——PreStream 还没开始流，
/// 这些 code 没有对应的语义。
pub fn pre_stream(code: &'static str, msg: impl Into<String>) -> RiboError {
  let code = coerce_for_pre_stream(code);
  RiboError::new(DOMAIN, code, msg)
    .with_severity(default_severity(code))
    .with_context("stage", "pre_stream")
}

/// 构造 InStream 阶段错误。
///
/// 传入 PreStream 专属 code（网关错误等）时，运行期改写为 `STREAM_INTERRUPTED`——
/// 流内阶段这些 code 没有对应语义，归为"流被中断"。
pub fn in_stream(code: &'static str, msg: impl Into<String>) -> RiboError {
  let code = coerce_for_in_stream(code);
  RiboError::new(DOMAIN, code, msg)
    .with_severity(default_severity(code))
    .with_context("stage", "in_stream")
}

/// 由 HTTP 状态码映射为 PreStream 错误。
///
/// 映射规则与 `from_reqwest` 一致：
/// - 400/422/409 → `INVALID_REQUEST`（Warning）
/// - 401/403 → `AUTH_FAILED`（Error）
/// - 402 → `QUOTA_EXCEEDED`（Error）
/// - 408/504 → `TIMEOUT`（Error）
/// - 429 → `RATE_LIMITED`（Error）
/// - 5xx → `SERVER_ERROR`（Error）
/// - 其余 → `INVALID_REQUEST`（Warning，保守）
///
/// `body` 截断到 512 字节（UTF-8 安全边界）后写入 message，
/// 同时注入 `context["http_status"]` 供排查。
pub fn from_http(status: u16, body: &str) -> RiboError {
  let (code, severity) = match status {
    400 | 422 => (INVALID_REQUEST, Severity::Warning),
    401 | 403 => (AUTH_FAILED, Severity::Error),
    402 => (QUOTA_EXCEEDED, Severity::Error),
    408 | 504 => (TIMEOUT, Severity::Error),
    409 => (INVALID_REQUEST, Severity::Warning),
    429 => (RATE_LIMITED, Severity::Error),
    500..=599 => (SERVER_ERROR, Severity::Error),
    _ => (INVALID_REQUEST, Severity::Warning),
  };
  let truncated = if body.len() <= 512 {
    body.to_string()
  } else {
    // ===== UTF-8 安全截断 =====
    // 从 512 字节位置向前回退到最近的字符边界，避免截断多字节字符。
    let mut end = 512;
    while end > 0 && !body.is_char_boundary(end) {
      end -= 1;
    }
    format!("{}...(truncated)", &body[..end])
  };
  let message = format!("HTTP {status}: {truncated}");

  pre_stream(code, message)
    .with_severity(severity)
    .with_context("http_status", status.to_string())
}

/// 由 `reqwest::Error` 映射为 PreStream 错误。
///
/// # ⚠️ 仅限建流阶段
///
/// **流式阶段禁止调用本函数**。流内的 reqwest 错误（例如 SSE 帧解码失败、
/// 响应体传输中断）应直接通过 `producer.error(StopReason::Error, .., Some(code))`
/// 终结。如果对流内错误调用 `from_reqwest`，会得到 PreStream 错误，
/// 重试层会尝试重新发起整个请求——与已产出的部分内容重复。
///
/// 本函数错误地传入流内错误虽然会被阶段门纠正为 `STREAM_INTERRUPTED`，
/// 但 message 来自 reqwest 自身描述，可能丢失业务上下文。
///
/// # 映射规则
///
/// - `is_timeout()` → `TIMEOUT`
/// - `is_connect()` → `TRANSPORT`
/// - `is_decode()` / `is_body()` → `INVALID_RESPONSE`
/// - 其余 → `TRANSPORT`（保守回退）
pub fn from_reqwest(err: reqwest::Error) -> RiboError {
  let code = if err.is_timeout() {
    TIMEOUT
  } else if err.is_connect() {
    TRANSPORT
  } else if err.is_decode() || err.is_body() {
    INVALID_RESPONSE
  } else {
    TRANSPORT
  };
  pre_stream(code, err.to_string()).with_source(err)
}

/// 由流式终结事件还原为完整错误。
///
/// 供网关/上层把 `ProviderEvent::Error` 的 `(code, message)` 还原后
/// 复用分类谓词 `is_retryable` / `is_degradable` / `is_circuit_trigger`。
///
/// 阶段固定为 InStream——流内终结事件不可能回到 PreStream。
/// `code` 为 `None` 时内部回落为 `STREAM_INTERRUPTED`。
pub fn from_stream_terminal(
  code: Option<&'static str>,
  message: impl Into<String>,
) -> RiboError {
  in_stream(code.unwrap_or(STREAM_INTERRUPTED), message)
}
