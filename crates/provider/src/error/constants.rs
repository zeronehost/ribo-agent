//! 本地化错误码常量模块。
//!
//! 所有常量取值都以 `"provider."` 为前缀，与上层 `RiboError::domain = "provider"`
//! 配对，形成 `(domain, code)` 二元组供错误分类谓词匹配。
//!
//! 常量按生命周期划分：
//!
//! - **请求参数 / 服务端**：任何阶段都可能出现。
//! - **流式专属**：只能在流内出现，建流阶段构造器会强制改写。
//! - **网关专属**：只由网关路由层产出，适配器绝不返回。

/// 本 crate 所有错误的 domain 值，传给 [`ra_core::error::RiboError::new`]。
pub(super) const DOMAIN: &str = "provider";

// ===== 请求参数 / 服务端 =====
// 这类错误在 PreStream 与 InStream 都可能出现，构造器阶段门不会改写。

/// 请求格式非法（HTTP 400/422）。
pub const INVALID_REQUEST: &str = "provider.invalid_request";
/// 认证失败（HTTP 401/403）。
pub const AUTH_FAILED: &str = "provider.auth_failed";
/// 配额用尽（HTTP 402）。
pub const QUOTA_EXCEEDED: &str = "provider.quota_exceeded";
/// 被限流（HTTP 429）。
pub const RATE_LIMITED: &str = "provider.rate_limited";
/// 请求超时（HTTP 408/504 或连接超时）。
pub const TIMEOUT: &str = "provider.timeout";
/// 上游服务端错误（HTTP 5xx）。
pub const SERVER_ERROR: &str = "provider.server_error";
/// 网络传输层错误（连接重置、TLS 失败等）。
pub const TRANSPORT: &str = "provider.transport_error";
/// 提供方响应无法解析（坏 JSON、SSE 帧损坏等）。
pub const INVALID_RESPONSE: &str = "provider.invalid_response";

// ===== 流式专属 =====
// 构造器阶段门会阻止这些 code 出现在 PreStream 阶段。

/// 内容被提供方过滤（安全策略），不视为重试/熔断故障。
pub const CONTENT_FILTERED: &str = "provider.content_filtered";
/// 流在未终结前被异常关闭（生产端 drop、TCP 断开等）。
pub const STREAM_INTERRUPTED: &str = "provider.stream_interrupted";
/// 用户主动取消（不计入上游故障）。
pub const CANCELLED: &str = "provider.cancelled";

// ===== 网关专属 =====
// 这些 code 只由网关路由层产出，适配器和重试层都不会返回。

/// 请求的 provider 不在注册表中。
pub const PROVIDER_NOT_FOUND: &str = "provider.provider_not_found";
/// 请求的模型在目标 provider 上不存在。
pub const MODEL_NOT_FOUND: &str = "provider.model_not_found";
/// 熔断已打开，当前请求被快速拒绝。
pub const CIRCUIT_OPEN: &str = "provider.circuit_open";
/// 并发许可耗尽，请求被拒绝。
pub const OVERLOADED: &str = "provider.overloaded";
/// 注册重复的 provider id。
pub const DUPLICATE_PROVIDER: &str = "provider.duplicate_provider";
/// 试图注册自带重试能力的 provider（网关已接管重试）。
pub const RETRY_PROVIDER_REFUSED: &str = "provider.retry_provider_refused";
