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
    // 预留 provider / model 元信息槽位，供 `from_result` 折叠错误时还原；
    // 适配器可用同键覆盖为真实值，缺失时保持空串。
    .with_context("provider", "")
    .with_context("model", "")
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
    // 同上：为 `from_result` 预留元信息槽位。
    .with_context("provider", "")
    .with_context("model", "")
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
  let code = reqwest_code(
    err.is_timeout(),
    err.is_connect(),
    err.is_decode(),
    err.is_body(),
  );
  pre_stream(code, err.to_string()).with_source(err)
}

/// 按 `reqwest::Error` 的四类判定结果选择错误码。
///
/// 抽成纯函数便于对每个分支做表驱动测试，避免依赖真实网络故障。
fn reqwest_code(
  is_timeout: bool,
  is_connect: bool,
  is_decode: bool,
  is_body: bool,
) -> &'static str {
  // 判定顺序与 `reqwest::Error` 的语义优先级一致：超时 > 连接 > 解码/读体 > 其他。
  if is_timeout {
    TIMEOUT
  } else if is_connect {
    TRANSPORT
  } else if is_decode || is_body {
    INVALID_RESPONSE
  } else {
    // 无法归类的请求错误保守视为传输层问题。
    TRANSPORT
  }
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

#[cfg(test)]
mod tests {
  use std::time::Duration;

  use super::*;
  use crate::traits::ProviderErrorExt;

  /// 场景：HTTP 状态码映射表覆盖常见请求错误类别。
  ///
  /// 预期：每个状态码落到文档约定的错误码，并写入 `http_status` 上下文。
  #[test]
  fn http_status_maps_to_documented_codes() {
    let cases = [
      (400, INVALID_REQUEST),
      (409, INVALID_REQUEST),
      (418, INVALID_REQUEST),
      (422, INVALID_REQUEST),
      (401, AUTH_FAILED),
      (403, AUTH_FAILED),
      (402, QUOTA_EXCEEDED),
      (408, TIMEOUT),
      (504, TIMEOUT),
      (429, RATE_LIMITED),
      (500, SERVER_ERROR),
      (599, SERVER_ERROR),
    ];

    for (status, expected_code) in cases {
      let err = from_http(status, "body");
      assert_eq!(err.code(), expected_code, "HTTP {status}");
      let context = err.context().expect("错误应携带 context");
      assert_eq!(context["http_status"], status.to_string());
    }
  }

  /// 场景：严重程度按错误码映射。
  #[test]
  fn severity_follows_documented_mapping() {
    assert_eq!(from_http(400, "").severity(), &Severity::Warning);
    assert_eq!(from_http(429, "").severity(), &Severity::Error);
    assert_eq!(pre_stream(CANCELLED, "").severity(), &Severity::Debug);
    assert_eq!(
      pre_stream(PROVIDER_NOT_FOUND, "").severity(),
      &Severity::Critical
    );
  }

  /// 场景：响应体超过 512 字节时截断，并保证不切开多字节字符。
  #[test]
  fn long_body_is_truncated_safely() {
    let ascii = "a".repeat(600);
    let err = from_http(400, &ascii);
    assert!(err.message().ends_with("...(truncated)"));

    // 每个汉字 3 字节，512 字节处不是字符边界，截断逻辑需向前回退。
    let chinese = "中".repeat(300);
    let err = from_http(400, &chinese);
    assert!(err.message().ends_with("...(truncated)"));
  }

  /// 场景：`pre_stream` 把流式专属 code 纠正为建流阶段语义。
  #[test]
  fn pre_stream_coerces_stream_only_codes() {
    assert_eq!(pre_stream(STREAM_INTERRUPTED, "x").code(), TRANSPORT);
    assert_eq!(pre_stream(CONTENT_FILTERED, "x").code(), INVALID_REQUEST);
  }

  /// 场景：`in_stream` 把网关专属 code 纠正为流中断。
  #[test]
  fn in_stream_coerces_gateway_codes() {
    assert_eq!(
      in_stream(PROVIDER_NOT_FOUND, "x").code(),
      STREAM_INTERRUPTED
    );
    assert_eq!(in_stream(TIMEOUT, "x").code(), TIMEOUT);
  }

  /// 场景：流终结事件未携带 code 时回落为 STREAM_INTERRUPTED。
  #[test]
  fn from_stream_terminal_defaults_to_interrupted() {
    let err = from_stream_terminal(None, "closed");

    assert_eq!(err.code(), STREAM_INTERRUPTED);
    assert_eq!(err.stage(), ErrorStage::InStream);
  }

  /// 场景：InStream 阶段短路降级判定。
  ///
  /// 预期：即使是可降级的 code，流内也不可重试/降级，但仍可触发熔断。
  #[test]
  fn in_stream_predicates_are_short_circuited() {
    let err = in_stream(SERVER_ERROR, "mid-stream");

    assert!(!err.is_retryable());
    assert!(!err.is_degradable());
    assert!(!err.is_cross_provider_degradable());
    assert!(err.is_circuit_trigger());
  }

  /// 场景：`from_reqwest` 将连接失败映射为 TRANSPORT。
  #[tokio::test]
  async fn from_reqwest_maps_connect_error_to_transport() {
    let err = reqwest::Client::new()
      .get("http://127.0.0.1:1/")
      .send()
      .await
      .expect_err("连接应失败");
    assert!(err.is_connect(), "前置条件：应为连接错误");

    assert_eq!(from_reqwest(err).code(), TRANSPORT);
  }

  /// 场景：`from_reqwest` 对无法归类的错误回退为 TRANSPORT。
  #[tokio::test]
  async fn from_reqwest_falls_back_to_transport() {
    let err = reqwest::Client::new()
      .get("http://")
      .send()
      .await
      .expect_err("非法 URL 应失败");
    assert!(
      !err.is_timeout()
        && !err.is_connect()
        && !err.is_decode()
        && !err.is_body(),
      "前置条件：不应命中其他分支"
    );

    assert_eq!(from_reqwest(err).code(), TRANSPORT);
  }

  /// 场景：`reqwest` 四类判定结果到错误码的映射。
  ///
  /// 预期：timeout / connect / decode / body / 其余分支各自落到约定错误码。
  #[test]
  fn reqwest_code_maps_each_condition() {
    assert_eq!(reqwest_code(true, true, true, true), TIMEOUT);
    assert_eq!(reqwest_code(false, true, true, true), TRANSPORT);
    assert_eq!(reqwest_code(false, false, true, false), INVALID_RESPONSE);
    assert_eq!(reqwest_code(false, false, false, true), INVALID_RESPONSE);
    assert_eq!(reqwest_code(false, false, false, false), TRANSPORT);
  }

  /// 场景：服务端接受连接但迟迟不返回响应。
  ///
  /// 预期：客户端超时被映射为 TIMEOUT。
  #[tokio::test]
  async fn from_reqwest_maps_timeout_to_timeout() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
      .await
      .expect("绑定本地端口");
    let addr = listener.local_addr().expect("读取本地地址");
    let server = tokio::spawn(async move {
      let (socket, _) = listener.accept().await.expect("应接受测试连接");
      // 保持连接但不发送响应，等待客户端超时
      tokio::time::sleep(Duration::from_millis(100)).await;
      drop(socket);
    });

    let client = reqwest::Client::builder()
      .timeout(Duration::from_millis(50))
      .build()
      .expect("构建客户端");
    let err = client
      .get(format!("http://{addr}/"))
      .send()
      .await
      .expect_err("应超时");
    assert!(err.is_timeout(), "前置条件：应为超时错误");

    assert_eq!(from_reqwest(err).code(), TIMEOUT);
    // 等待服务端任务收尾，保证其清理路径被完整执行。
    tokio::time::sleep(Duration::from_millis(150)).await;
    server.await.expect("服务端任务不应 panic");
  }
}
