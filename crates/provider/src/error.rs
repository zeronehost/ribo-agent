//! provider crate 的错误构造工具。
//!
//! 本 crate 不定义独立错误类型，所有错误统一收敛为
//! [`ra_core::error::RiboError`]；这里的 [`ProviderError`] 仅作为按错误码
//! 组织的构造函数命名空间。

use ra_core::error::RiboError;

/// provider 域错误的构造命名空间，本身不携带数据。
///
/// 通过关联函数（如 [`ProviderError::recv_error`]）产出带有固定
/// `domain = "provider"` 的 [`RiboError`]；实例本身无状态，
/// 调用方只需使用其关联函数，无需持有该类型的值。
pub struct ProviderError;

/// provider 域标识，写入 [`RiboError`] 的 domain 字段。
const DOMAIN: &str = "provider";

/// 事件流接收失败的错误码：流未以 Done/Error 事件正常终结即中断。
const CODE_RECV_ERROR: &str = "recv_error";

impl ProviderError {
  /// 构造「事件流接收失败」错误。
  ///
  /// 当最终消息的 oneshot 通道无法收到结果（典型原因：生产端在推送
  /// Done/Error 前被丢弃）时使用，原始通道错误作为上游错误挂接。
  ///
  /// # 参数
  ///
  /// - `source`：底层接收错误，需满足 `Error + Send + Sync + 'static`。
  pub fn recv_error<S>(source: S) -> RiboError
  where
    S: core::error::Error + Send + Sync + 'static,
  {
    RiboError::new(
      DOMAIN,
      CODE_RECV_ERROR,
      "provider event stream ended without a terminal Done/Error event",
    )
    .with_source(source)
  }
}
