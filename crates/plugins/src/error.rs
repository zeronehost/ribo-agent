//! 插件域错误：错误码常量与构造辅助。
//!
//! 插件域独立声明 `DOMAIN = "plugin"`（不复用 `ra-tools` 的 `tool` 域，
//! 避免上层 [`ToolManager`](ra_tools::manager::ToolManager) / agent 主循环
//! 在 `domain` 维度上误判）。所有插件失败统一收敛为
//! [`RiboError`](ra_core::error::RiboError)——不新增错误变体、不引入
//! 第二套对外错误类型，语义分级由稳定错误码（`code()`）区分。
//!
//! 错误码总览（对应设计文档 18_ §1.2 / §5.3）：
//!
//! | 错误码 | 语义 | 熔断记账 |
//! | :-- | :-- | :-- |
//! | [`PLUGIN_EXEC_FAILED`] | 业务级：guest 返回错误 / ABI 违约 | 计入（可回灌 LLM 自我修正） |
//! | [`PLUGIN_TIMEOUT`] | 系统级：epoch 到期 / 内存超限 | 不计（白名单） |
//! | [`PLUGIN_FORBIDDEN`] | 越权：未声明的能力调用 | 不计（白名单，配置类错误） |
//! | [`PLUGIN_INTERNAL`] | 宿主内部：Engine / Linker / 线程失败 | 计入 |
//! | `plugin.plugin_*`（注册期） | ID 重复 / 命名非法 / Schema 非法 / 批量失败 | 注册期拒绝，不进 ToolManager |

use ra_core::error::{RiboError, Severity};

/// 错误域标识：所有插件域错误共用的 domain 字段。
pub const DOMAIN: &str = "plugin";

/// 错误码：业务级失败——guest 返回错误 / 参数问题（LLM 可自我修正）。
pub const PLUGIN_EXEC_FAILED: &str = "plugin.plugin_exec_failed";
/// 错误码：系统级失败——epoch 到期 / 内存超限 / trap（不可重试）。
pub const PLUGIN_TIMEOUT: &str = "plugin.plugin_timeout";
/// 错误码：越权——调用了未声明的 host function 能力。
pub const PLUGIN_FORBIDDEN: &str = "plugin.plugin_forbidden";
/// 错误码：注册期——插件 ID 重复。
pub const PLUGIN_DUPLICATE_ID: &str = "plugin.plugin_duplicate_id";
/// 错误码：注册期——插件 ID / 工具名命名非法。
pub const PLUGIN_INVALID_NAME: &str = "plugin.plugin_invalid_name";
/// 错误码：注册期——工具参数 JSON Schema 非法。
pub const PLUGIN_SCHEMA_INVALID: &str = "plugin.plugin_schema_invalid";
/// 错误码：注册期——批量注入 `ToolManager` 失败。
pub const PLUGIN_BATCH_FAILED: &str = "plugin.plugin_batch_failed";
/// 错误码：宿主内部错误——Engine / Linker / 线程 / JoinHandle 失败。
pub const PLUGIN_INTERNAL: &str = "plugin.plugin_internal";
/// 错误码：guest 传入非法 ptr/len/method/key 等 ABI 参数违约。
///
/// 注意与 `ra_tools::error::invalid_arguments()`（`tool.invalid_arguments`）
/// 区分：后者是 LLM 传入的工具参数错，本码是 **guest 传给 host 的
/// ptr/len/key** 违约（经 `guest_error:` 前缀收敛为 [`PLUGIN_EXEC_FAILED`]）。
pub const PLUGIN_INVALID_ARGUMENTS: &str = "plugin.plugin_invalid_arguments";
/// 错误码：来源校验失败——签名无效 / 摘要不符 / 签名者不在信任列表。
///
/// 加载期致命错误（编译之前拦截），不进入执行期熔断记账。
pub const PLUGIN_SIGNATURE_INVALID: &str = "plugin.plugin_signature_invalid";

/// 构造插件业务错误（Severity: Error）。
///
/// 截断后的 `reason` 同时写入 message 与 context，供回灌 LLM 自我修正；
/// 完整错误文本只落日志（见 [`truncate_error`]）。
pub fn plugin_exec_failed(
  plugin_id: &str,
  tool_name: &str,
  reason: &str,
) -> RiboError {
  RiboError::new(
    DOMAIN,
    PLUGIN_EXEC_FAILED,
    format!("plugin tool {tool_name} failed: {reason}"),
  )
  .with_severity(Severity::Error)
  .with_context("plugin_id", plugin_id)
  .with_context("tool_name", tool_name)
  .with_context("reason", reason)
}

/// 构造插件系统级超时 / 中断错误（Severity: Error）。
///
/// `trap_reason` 记录中断来源（如 `epoch_exhausted` / `memory_limit`），
/// 该类错误在 `ToolManager` 白名单下**不计熔断**（18_ §1.3）。
pub fn plugin_timeout(
  plugin_id: &str,
  tool_name: &str,
  deadline_ms: u64,
  trap_reason: &str,
) -> RiboError {
  RiboError::new(
    DOMAIN,
    PLUGIN_TIMEOUT,
    format!("plugin tool {tool_name} timed out after {deadline_ms}ms"),
  )
  .with_severity(Severity::Error)
  .with_context("plugin_id", plugin_id)
  .with_context("tool_name", tool_name)
  .with_context("deadline_ms", deadline_ms.to_string())
  .with_context("trap_reason", trap_reason)
}

/// 构造越权错误（Severity: Warning——配置/权限问题，非工具故障）。
///
/// `capability` 为**纯能力名**（如 `storage` / `url_policy`），该类错误
/// 在 `ToolManager` 白名单下**不计熔断**（18_ §1.3）。
pub fn plugin_forbidden(plugin_id: &str, capability: &str) -> RiboError {
  RiboError::new(
    DOMAIN,
    PLUGIN_FORBIDDEN,
    format!("plugin {plugin_id} denied capability {capability}"),
  )
  .with_severity(Severity::Warning)
  .with_context("plugin_id", plugin_id)
  .with_context("capability", capability)
}

/// 构造宿主内部错误（Severity: Critical——按宿主故障计入熔断）。
///
/// 不暴露上游错误对象，仅记录摘要到 context（`stage` + `reason`）。
pub fn plugin_internal(
  stage: &str,
  cause: impl core::fmt::Display,
) -> RiboError {
  RiboError::new(
    DOMAIN,
    PLUGIN_INTERNAL,
    format!("plugin host internal error at {stage}"),
  )
  .with_severity(Severity::Critical)
  .with_context("stage", stage)
  .with_context("reason", cause.to_string())
}

/// 构造 guest ABI 参数违约错误（Severity: Error）。
///
/// `reason` 应指向具体违约项（负 ptr / 超限长度 / 非法 UTF-8 等），
/// 供 LLM 收到回灌后自行缩小入参。
pub fn plugin_invalid_arguments(reason: &str) -> RiboError {
  RiboError::new(
    DOMAIN,
    PLUGIN_INVALID_ARGUMENTS,
    format!("invalid argument: {reason}"),
  )
  .with_severity(Severity::Error)
  .with_context("reason", reason)
}

/// 构造来源校验失败错误（Severity: Error，加载期致命）。
///
/// `reason` 指明具体失败项（签名无效 / wasm 摘要不符 / 签名者未授信），
/// 完整差异明细由调用方写入日志；错误文本本身不含密钥或签名载荷。
pub fn plugin_signature_invalid(reason: &str) -> RiboError {
  RiboError::new(
    DOMAIN,
    PLUGIN_SIGNATURE_INVALID,
    format!("plugin provenance verification failed: {reason}"),
  )
  .with_severity(Severity::Error)
  .with_context("reason", reason)
}

/// 构造「插件 ID 重复」错误（Severity: Warning——幂等冲突，同已注册插件）。
pub fn plugin_duplicate_id(plugin_id: &str) -> RiboError {
  RiboError::new(
    DOMAIN,
    PLUGIN_DUPLICATE_ID,
    format!("plugin id already registered: {plugin_id}"),
  )
  .with_severity(Severity::Warning)
  .with_context("plugin_id", plugin_id)
}

/// 构造「插件命名非法」错误（Severity: Error）。
///
/// `field` 为 `plugin_id` 或 `tool_name`，`reason` 指向具体约束
/// （前缀字符集、长度上界等）。
pub fn plugin_invalid_name(
  field: &str,
  value: &str,
  reason: impl Into<String>,
) -> RiboError {
  let reason = reason.into();
  RiboError::new(
    DOMAIN,
    PLUGIN_INVALID_NAME,
    format!("invalid plugin {field} `{value}`: {reason}"),
  )
  .with_severity(Severity::Error)
  .with_context("field", field)
  .with_context("value", value)
  .with_context("reason", reason)
}

/// 构造「工具参数 Schema 非法」错误（Severity: Error，注册期拒绝）。
pub fn plugin_schema_invalid(
  plugin_id: &str,
  tool_name: &str,
  reason: &str,
) -> RiboError {
  RiboError::new(
    DOMAIN,
    PLUGIN_SCHEMA_INVALID,
    format!("plugin tool {tool_name} has invalid parameter schema: {reason}"),
  )
  .with_severity(Severity::Error)
  .with_context("plugin_id", plugin_id)
  .with_context("tool_name", tool_name)
  .with_context("reason", reason)
}

/// 构造「批量注入 ToolManager 失败」错误（Severity: Error，注册期拒绝）。
pub fn plugin_batch_failed(plugin_id: &str, reason: &str) -> RiboError {
  RiboError::new(
    DOMAIN,
    PLUGIN_BATCH_FAILED,
    format!("plugin {plugin_id} batch registration failed: {reason}"),
  )
  .with_severity(Severity::Error)
  .with_context("plugin_id", plugin_id)
  .with_context("reason", reason)
}

/// 按 UTF-8 字符边界截断错误文本（设计文档 18_ §1.2）。
///
/// guest 返回的 `error` 字段必须截断到上限（默认 4KB）再回灌 / 入日志，
/// 避免巨型错误文本爆 token 预算。截断点按 [`str::is_char_boundary`]
/// 向前回退，保证不切断多字节序列（中文 / emoji 安全）。
///
/// # Examples
///
/// ```
/// use ra_plugins::error::truncate_error;
///
/// // 未超上限：原样返回
/// assert_eq!(truncate_error("boom", 4096), "boom");
///
/// // 超上限：截断并追加标记（多字节字符不被切断）
/// let long = "中".repeat(3000);
/// let cut = truncate_error(&long, 4096);
/// assert!(cut.ends_with("... [truncated]"));
/// assert!(cut.is_char_boundary(cut.len() - "... [truncated]".len()));
/// ```
pub fn truncate_error(s: &str, max_bytes: usize) -> String {
  if s.len() <= max_bytes {
    return s.to_owned();
  }
  // ===== 向前回退到合法 UTF-8 字符边界 =====
  let mut end = max_bytes.min(s.len());
  while end > 0 && !s.is_char_boundary(end) {
    end -= 1;
  }
  let mut out = String::with_capacity(end + 16);
  out.push_str(&s[..end]);
  out.push_str("... [truncated]");
  out
}

#[cfg(test)]
mod tests {
  use ra_core::error::Severity;

  use super::*;

  /// 场景：各构造函数产生的错误码、domain、Severity 与 context 正确。
  #[test]
  fn constructors_set_domain_code_severity_and_context() {
    let err = plugin_exec_failed("a", "echo", "boom");
    assert_eq!(err.domain(), DOMAIN);
    assert_eq!(err.code(), PLUGIN_EXEC_FAILED);
    assert_eq!(err.severity(), &Severity::Error);
    let ctx = err.context().expect("context attached");
    assert_eq!(ctx.get("plugin_id").map(String::as_str), Some("a"));
    assert_eq!(ctx.get("tool_name").map(String::as_str), Some("echo"));

    let err = plugin_timeout("a", "echo", 2000, "epoch_exhausted");
    assert_eq!(err.code(), PLUGIN_TIMEOUT);
    assert_eq!(err.severity(), &Severity::Error);
    let ctx = err.context().expect("context attached");
    assert_eq!(
      ctx.get("trap_reason").map(String::as_str),
      Some("epoch_exhausted")
    );

    let err = plugin_forbidden("a", "storage");
    assert_eq!(err.code(), PLUGIN_FORBIDDEN);
    assert_eq!(err.severity(), &Severity::Warning);
    assert_eq!(
      err
        .context()
        .and_then(|c| c.get("capability").map(String::as_str)),
      Some("storage")
    );

    let err = plugin_internal("engine init", "boom");
    assert_eq!(err.code(), PLUGIN_INTERNAL);
    assert_eq!(err.severity(), &Severity::Critical);

    let err = plugin_invalid_arguments("ptr<0");
    assert_eq!(err.code(), PLUGIN_INVALID_ARGUMENTS);
    assert_eq!(err.severity(), &Severity::Error);

    let err = plugin_signature_invalid("wasm digest mismatch");
    assert_eq!(err.code(), PLUGIN_SIGNATURE_INVALID);
    assert_eq!(err.severity(), &Severity::Error);
    assert_eq!(
      err
        .context()
        .and_then(|c| c.get("reason").map(String::as_str)),
      Some("wasm digest mismatch")
    );

    let err = plugin_duplicate_id("a");
    assert_eq!(err.code(), PLUGIN_DUPLICATE_ID);
    assert_eq!(err.severity(), &Severity::Warning);

    let err = plugin_invalid_name("plugin_id", "Bad", "bad ident");
    assert_eq!(err.code(), PLUGIN_INVALID_NAME);
    assert_eq!(err.severity(), &Severity::Error);

    let err = plugin_schema_invalid("a", "echo", "bad schema");
    assert_eq!(err.code(), PLUGIN_SCHEMA_INVALID);

    let err = plugin_batch_failed("a", "conflict");
    assert_eq!(err.code(), PLUGIN_BATCH_FAILED);
  }

  /// 场景：`truncate_error` 的 UTF-8 边界安全截断。
  ///
  /// 预期：含中文 / emoji 的超长文本截断后仍是合法 UTF-8、
  /// 以 `... [truncated]` 结尾，且不 panic。
  #[test]
  fn truncate_error_respects_char_boundaries() {
    // 4 字节 emoji 重复：4096 处落在字符中间，必须回退
    let text = "🦀".repeat(2000); // 2000 * 4 = 8000 bytes
    let cut = truncate_error(&text, 4096);
    assert!(cut.ends_with("... [truncated]"));
    assert!(cut.len() < 4096 + 16);

    // 未超上限：原样返回，不加标记
    assert_eq!(truncate_error("short", 4096), "short");
    // 恰好等于上限：原样返回
    let exact = "x".repeat(4096);
    assert_eq!(truncate_error(&exact, 4096), exact);
  }
}
