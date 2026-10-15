//! 统一错误模型。
//!
//! 本模块为整个工作空间提供一套结构化错误设施，包含三个公开项：
//!
//! - [`RiboError`]：错误载体，按「领域 + 错误码 + 消息」组织，并附带严重程度、
//!   结构化上下文与可选的上游错误链。
//! - [`Severity`]：五级严重程度枚举，可比较、可排序、可序列化。
//! - [`RiboResult`]：错误侧固定为 [`RiboError`] 的 `Result` 别名。
//!
//! # 关键设计
//!
//! ## 上游错误用 `Arc` 持有
//!
//! `source` 字段类型为 `Arc<dyn Error + Send + Sync>`。`dyn Error` 本身不可 `Clone`，
//! 若用 `Box` 持有，[`RiboError`] 就无法派生 `Clone`；改用 `Arc` 后克隆只增加引用计数，
//! 同一份错误可以同时用于日志、指标上报与向上传播。`Send + Sync` 约束则保证错误可以
//! 自由跨线程移动与共享。
//!
//! ## `Display` 只呈现三要素
//!
//! [`RiboError`] 的 `Display` 输出固定为 `[domain] code: message`。严重程度与上下文
//! 只能通过 [`RiboError::severity`]、[`RiboError::context`] 显式读取，避免上下文中的
//! 敏感信息被无意拼进用户可见的文本。
//!
//! ## `source` 不参与序列化
//!
//! trait object 没有通用的序列化形式，因此 `source` 标注 `#[serde(skip)]`；
//! 反序列化时该字段取 `Option` 的默认值 `None`。错误链不会跨越进程边界。
//!
//! ## `source()` 的两条解析路径
//!
//! 结构体派生了 `thiserror::Error` 且包含名为 `source` 的字段，thiserror 会据此生成
//! `Error::source()`。同时本模块又定义了同名固有方法 [`RiboError::source`]。
//! 直接调用 `err.source()` 走固有方法；以 `Error::source(&err)` 形式调用走 trait 方法，
//! 两者返回同一个上游错误。
//!
//! # 序列化契约
//!
//! JSON 形状如下，跨进程通信（HTTP、RPC 等）的双方都应以该形状为准：
//!
//! ```text
//! {
//!   "domain": "provider",
//!   "code": "request_failed",
//!   "severity": "critical",
//!   "message": "server is down",
//!   "context": { "attempt": "3" }
//! }
//! ```
//!
//! 注意三点：
//!
//! 1. `severity` 序列化为变体名的**小写**形式（[`Severity`] 标注了
//!    `#[serde(rename_all = "lowercase")]`），例如 `"critical"` 而非 `"Critical"`。
//! 2. `context` 未设置时整字段缺省，读取方需要处理字段不存在的情况。
//! 3. `source` 被跳过，对端反序列化后错误链为空；需要传递原因时应显式写入 `context`。
//!
//! # 使用建议
//!
//! - 跨越 crate 或模块边界、需要被上层或用户感知的错误，统一使用 [`RiboError`]，
//!   调用方可以依据 `domain` 与 `code` 做稳定分支判断。
//! - 仅在函数内部临时使用、不会外泄的错误，可直接保留原始错误类型，
//!   避免包装过程中丢失信息。
//!
//! # Examples
//!
//! ```
//! use ra_core::error::{RiboError, Severity};
//!
//! let err = RiboError::new("provider", "upstream_unavailable", "server is down")
//!   .with_severity(Severity::Critical)
//!   .with_context("attempt", "3");
//!
//! assert_eq!(
//!   err.to_string(),
//!   "[provider] upstream_unavailable: server is down"
//! );
//! assert_eq!(err.severity(), &Severity::Critical);
//! ```

use std::{collections::HashMap, fmt, sync::Arc};

/// 跨模块传递的统一错误类型。
///
/// 通过链式构造器组装：[`RiboError::new`] 提供必填的领域、错误码与消息，
/// 随后可用 [`RiboError::with_severity`]、[`RiboError::with_context`]、
/// [`RiboError::with_source`] 追加可选信息。所有字段私有，外部只能经 getter 读取，
/// 因此实例构造完成后始终处于合法状态。
///
/// # 克隆语义
///
/// 克隆为部分浅拷贝：`message` 与 `context` 会被深拷贝，副本之间互不影响；
/// `source` 由 [`Arc`] 共享，克隆只增加引用计数而不复制底层错误。
///
/// # 字符串字段约束
///
/// `domain` 与 `code` 均为 `&'static str`，要求取值来自编译期常量，
/// 不接受运行时拼接的动态字符串；动态信息应放入 `message` 或 `context`。
///
/// # 序列化
///
/// 序列化形状与注意事项见模块文档的「序列化契约」一节，下面的示例对关键字段做可执行校验：
///
/// ```
/// use ra_core::error::{RiboError, Severity};
///
/// let err = RiboError::new("provider", "request_failed", "server is down")
///   .with_severity(Severity::Critical)
///   .with_context("attempt", "3");
///
/// let json = serde_json::to_value(&err).expect("should serialize");
/// assert_eq!(json["domain"], "provider");
/// assert_eq!(json["code"], "request_failed");
/// // severity 经 rename_all = "lowercase" 序列化为小写
/// assert_eq!(json["severity"], "critical");
/// assert_eq!(json["message"], "server is down");
/// assert_eq!(json["context"]["attempt"], "3");
/// // source 不参与序列化
/// assert!(json.get("source").is_none());
/// ```
///
/// # Examples
///
/// ```
/// use ra_core::error::RiboError;
///
/// let err = RiboError::new("config", "parse_failed", "invalid port");
/// assert_eq!(err.domain(), "config");
/// assert_eq!(err.code(), "parse_failed");
/// assert_eq!(err.message(), "invalid port");
/// // 未显式设置时严重程度取默认值 Error
/// assert_eq!(err.severity(), &ra_core::error::Severity::Error);
/// ```
// 各派生宏的职责：
// - Debug：面向开发者的调试输出。
// - Clone：依赖 source 字段的 Arc 共享，见上方「克隆语义」。
// - thiserror::Error：生成 Display（格式为 `[domain] code: message`）与 Error::source()。
// - Serialize/Deserialize：支撑跨进程传输，形状见模块文档。
#[derive(
  Debug, Clone, thiserror::Error, serde::Serialize, serde::Deserialize,
)]
#[error("[{domain}] {code}: {message}")]
pub struct RiboError {
  /// 错误归属的功能领域，如 `"provider"`、`"config"`，用于归类统计与告警聚合。
  domain: &'static str,
  /// 领域内稳定唯一的错误码，如 `"request_failed"`，调用方据此做分支处理。
  code: &'static str,
  /// 严重程度，影响重试、告警等处置策略，默认值为 [`Severity::Error`]。
  severity: Severity,
  /// 面向人类的错误描述，允许是运行时构造的动态文本。
  message: String,
  /// 补充排查信息的键值对集合；缺省为 `None`，且缺省时不出现在序列化结果中。
  #[serde(default, skip_serializing_if = "Option::is_none")]
  context: Option<HashMap<String, String>>,
  /// 触发本错误的上游错误；trait object 无法序列化，因此恒被 serde 跳过。
  #[serde(skip)]
  source: Option<Arc<dyn core::error::Error + Send + Sync>>,
}

impl RiboError {
  /// 以必填三要素创建错误。
  ///
  /// 新实例的严重程度为 [`Severity::Error`]，上下文与上游错误均为空。
  ///
  /// # 参数
  ///
  /// - `domain`：功能领域，建议以编译期常量集中管理。
  /// - `code`：领域内错误码，必须是 `'static str`。
  /// - `message`：错误描述，任何可转换为 `String` 的类型均可。
  ///
  /// # Examples
  ///
  /// ```
  /// use ra_core::error::RiboError;
  ///
  /// let err = RiboError::new("provider", "timeout", "request timed out");
  /// assert_eq!(err.to_string(), "[provider] timeout: request timed out");
  /// assert_eq!(err.severity(), &ra_core::error::Severity::Error);
  /// ```
  pub fn new(
    domain: &'static str,
    code: &'static str,
    message: impl Into<String>,
  ) -> Self {
    Self {
      domain,
      code,
      severity: Severity::default(),
      message: message.into(),
      context: None,
      source: None,
    }
  }

  /// 写入一条上下文键值并返回自身，支持链式调用。
  ///
  /// 所有值统一转换为字符串，以保证上下文可直接序列化。
  ///
  /// # 参数
  ///
  /// - `k`：上下文键；键已存在时新值**覆盖**旧值。
  /// - `v`：上下文值。
  ///
  /// # 实现说明
  ///
  /// 内部 map 采用惰性分配：首次调用本方法时才创建 [`HashMap`]，
  /// 从未添加上下文的错误不会产生分配，序列化时也不会出现空的 `context` 字段。
  ///
  /// # Examples
  ///
  /// ```
  /// use ra_core::error::RiboError;
  ///
  /// let err = RiboError::new("io", "read_failed", "cannot read file")
  ///   .with_context("path", "/tmp/a.txt");
  ///
  /// let context = err.context().expect("context should be attached");
  /// assert_eq!(context.get("path").map(String::as_str), Some("/tmp/a.txt"));
  /// ```
  pub fn with_context(
    mut self,
    k: impl Into<String>,
    v: impl Into<String>,
  ) -> Self {
    // ===== 惰性初始化 =====
    // 仅在首次写入时分配 HashMap，避免无上下文的错误承担空 map 的开销。
    let context = self.context.get_or_insert_with(HashMap::new);
    // ===== 覆盖写入 =====
    // 同键以新值覆盖，语义与 HashMap::insert 一致。
    context.insert(k.into(), v.into());
    self
  }

  /// 设置严重程度并返回自身，支持链式调用。
  ///
  /// # 参数
  ///
  /// - `severity`：目标严重程度。
  ///
  /// # Examples
  ///
  /// ```
  /// use ra_core::error::{RiboError, Severity};
  ///
  /// let err = RiboError::new("tool", "exec_failed", "command exited")
  ///   .with_severity(Severity::Fatal);
  /// assert_eq!(err.severity(), &Severity::Fatal);
  /// ```
  pub fn with_severity(mut self, severity: Severity) -> Self {
    self.severity = severity;
    self
  }

  /// 挂接上游错误并返回自身，支持链式调用。
  ///
  /// 上游错误被装入 `Arc<dyn Error + Send + Sync>`，因此错误整体仍可克隆、
  /// 可跨线程传递，设计原因见模块文档的「上游错误用 `Arc` 持有」一节。
  ///
  /// # 参数
  ///
  /// - `source`：满足 `Error + Send + Sync + 'static` 的任意错误类型。
  ///
  /// # Examples
  ///
  /// ```
  /// use ra_core::error::RiboError;
  ///
  /// let cause = std::io::Error::new(std::io::ErrorKind::NotFound, "missing file");
  /// let err = RiboError::new("config", "load_failed", "cannot load config")
  ///   .with_source(cause);
  ///
  /// assert_eq!(
  ///   err.source().map(ToString::to_string).as_deref(),
  ///   Some("missing file")
  /// );
  /// ```
  pub fn with_source<E>(mut self, source: E) -> Self
  where
    E: core::error::Error + Send + Sync + 'static,
  {
    self.source = Some(Arc::new(source));
    self
  }

  /// 返回错误所属领域。
  ///
  /// # Examples
  ///
  /// ```
  /// use ra_core::error::RiboError;
  ///
  /// let err = RiboError::new("provider", "timeout", "request timed out");
  /// assert_eq!(err.domain(), "provider");
  /// ```
  pub fn domain(&self) -> &'static str {
    self.domain
  }

  /// 返回领域内错误码。
  ///
  /// # Examples
  ///
  /// ```
  /// use ra_core::error::RiboError;
  ///
  /// let err = RiboError::new("provider", "timeout", "request timed out");
  /// assert_eq!(err.code(), "timeout");
  /// ```
  pub fn code(&self) -> &'static str {
    self.code
  }

  /// 返回原始错误描述（不含领域与错误码前缀）。
  ///
  /// 完整的 `[domain] code: message` 文本请使用 `to_string()`。
  ///
  /// # Examples
  ///
  /// ```
  /// use ra_core::error::RiboError;
  ///
  /// let err = RiboError::new("provider", "timeout", "request timed out");
  /// assert_eq!(err.message(), "request timed out");
  /// assert_ne!(err.message(), err.to_string());
  /// ```
  pub fn message(&self) -> &str {
    &self.message
  }

  /// 返回严重程度。
  ///
  /// # Examples
  ///
  /// ```
  /// use ra_core::error::RiboError;
  ///
  /// let err = RiboError::new("provider", "timeout", "request timed out");
  /// assert_eq!(err.severity(), &ra_core::error::Severity::Error);
  /// ```
  pub fn severity(&self) -> &Severity {
    &self.severity
  }

  /// 返回结构化上下文；从未调用 [`RiboError::with_context`] 时为 `None`。
  ///
  /// # Examples
  ///
  /// ```
  /// use std::collections::HashMap;
  ///
  /// use ra_core::error::RiboError;
  ///
  /// let bare = RiboError::new("io", "read_failed", "cannot read file");
  /// assert!(bare.context().is_none());
  ///
  /// let rich = bare.with_context("path", "/tmp/a.txt");
  /// assert_eq!(rich.context().map(HashMap::len), Some(1));
  /// ```
  pub fn context(&self) -> Option<&HashMap<String, String>> {
    self.context.as_ref()
  }

  /// 返回上游错误；未挂接任何上游错误时为 `None`。
  ///
  /// 这是固有方法版本，直接调用 `err.source()` 即命中本方法，返回带
  /// `Send + Sync` 约束的 trait object；thiserror 生成的 `Error::source()`
  /// 需通过 trait 显式调用，返回值相同。
  ///
  /// # Examples
  ///
  /// ```
  /// use std::error::Error;
  ///
  /// use ra_core::error::RiboError;
  ///
  /// let bare = RiboError::new("config", "load_failed", "cannot load config");
  /// assert!(bare.source().is_none());
  ///
  /// // 经由 Error trait 调用 thiserror 生成的版本，结果一致
  /// assert!(Error::source(&bare).is_none());
  /// ```
  pub fn source(
    &self,
  ) -> Option<&(dyn core::error::Error + Send + Sync + 'static)> {
    self.source.as_deref()
  }
}

/// 错误严重程度。
///
/// 严重程度随变体声明顺序递增：
/// `Debug` < `Warning` < `Error` < `Critical` < `Fatal`。
/// 枚举派生了 [`Ord`]，可直接使用比较运算符；默认值为 [`Severity::Error`]。
///
/// # 处置建议
///
/// | 级别 | 建议处置 |
/// | :-- | :-- |
/// | `Debug` | 仅写入调试日志，不打扰用户 |
/// | `Warning` | 降级继续执行，提醒用户关注 |
/// | `Error` | 操作失败，可按策略重试 |
/// | `Critical` | 核心能力受损，需要立即告警 |
/// | `Fatal` | 进程无法继续安全运行，应终止并保留现场 |
///
/// # 序列化
///
/// 枚举标注了 `#[serde(rename_all = "lowercase")]`，外部表示固定为小写变体名：
/// `"debug"`、`"warning"`、`"error"`、`"critical"`、`"fatal"`；
/// 反序列化也只接受这五种小写字符串。
///
/// # Examples
///
/// ```
/// use ra_core::error::Severity;
///
/// assert!(Severity::Warning < Severity::Fatal);
/// assert_eq!(Severity::default(), Severity::Error);
/// assert_eq!(Severity::Critical.to_string(), "critical");
/// ```
#[derive(
  Debug,
  Default,
  Clone,
  PartialEq,
  Eq,
  PartialOrd,
  Ord,
  Hash,
  serde::Serialize,
  serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
  /// 调试信息，仅在问题排查时关注。
  Debug,
  /// 警告：流程得以延续，但结果存在瑕疵或隐患。
  Warning,
  /// 常规错误：本次操作未能完成。
  #[default]
  Error,
  /// 严重错误：关键能力不可用，需要人工介入。
  Critical,
  /// 致命错误：程序已无法安全继续运行。
  Fatal,
}

/// 以小写名称渲染严重程度，用于日志与用户可读文本。
///
/// [`Severity`] 本身不是错误类型，只作为 [`RiboError`] 的字段存在，
/// 因此不派生 `thiserror::Error`，而是在此手写 `Display`。
/// 渲染结果与 serde 的小写外部表示保持一致，例如 [`Severity::Critical`]
/// 渲染为 `"critical"`。
///
/// # Examples
///
/// ```
/// use ra_core::error::Severity;
///
/// assert_eq!(Severity::Fatal.to_string(), "fatal");
/// ```
impl fmt::Display for Severity {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let level = match self {
      Self::Debug => "debug",
      Self::Warning => "warning",
      Self::Error => "error",
      Self::Critical => "critical",
      Self::Fatal => "fatal",
    };
    f.write_str(level)
  }
}

/// 工作空间通用的 `Result` 别名，错误类型统一为 [`RiboError`]。
///
/// # Examples
///
/// ```
/// use ra_core::error::{RiboError, RiboResult};
///
/// fn parse_port(input: &str) -> RiboResult<u16> {
///   input.parse().map_err(|_| {
///     RiboError::new("config", "invalid_port", "port must be a number")
///       .with_context("input", input)
///   })
/// }
///
/// assert_eq!(parse_port("8080").unwrap(), 8080);
/// assert!(parse_port("abc").is_err());
/// ```
pub type RiboResult<T> = Result<T, RiboError>;

#[cfg(test)]
mod tests {
  use super::*;

  // 场景：仅通过 new 构造、不追加任何可选信息。
  // 预期：上下文为空，严重程度为默认值 Error，领域与错误码原样保留。
  #[test]
  fn error_test() {
    let err = RiboError::new("test", "test_error", "test error");
    assert!(err.context().is_none());
    assert_eq!(err.severity(), &Severity::default());
    assert_eq!(err.domain(), "test");
    assert_eq!(err.code(), "test_error");
  }

  // 场景：通过 with_severity 覆盖默认严重程度。
  // 预期：getter 返回新设置的级别。
  #[test]
  fn error_with_severity_test() {
    let err = RiboError::new("test", "test_error", "test error")
      .with_severity(Severity::Debug);
    assert_eq!(err.severity(), &Severity::Debug);
  }

  // 场景：通过 with_context 写入一条上下文。
  // 预期：上下文从 None 变为包含该键值对的 map。
  #[test]
  fn error_with_context_test() {
    let err = RiboError::new("test", "test_error", "test error")
      .with_context("test", "test");
    assert!(err.context().is_some());
    assert_eq!(
      err.context().and_then(|c| c.get("test")),
      Some(&"test".to_string())
    );
  }

  // 场景：校验 Severity 的小写序列化契约。
  // 预期：序列化为小写字符串；小写字符串可往返反序列化；驼峰变体名被拒绝。
  #[test]
  fn severity_serde_lowercase_test() {
    let serialized = serde_json::to_string(&Severity::Critical)
      .expect("severity should serialize");
    assert_eq!(serialized, "\"critical\"");

    let deserialized: Severity = serde_json::from_str("\"critical\"")
      .expect("lowercase should deserialize");
    assert_eq!(deserialized, Severity::Critical);

    assert!(serde_json::from_str::<Severity>("\"Critical\"").is_err());
  }
}
