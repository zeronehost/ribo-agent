//! 退避休眠与 `retry_after` 解析辅助。

use std::time::Duration;

use ra_core::error::{RiboError, RiboResult};

use crate::{
  error::{CANCELLED, pre_stream},
  options::CancelToken,
};

/// 从错误的 context 中解析服务端建议的重试等待时长。
///
/// 读取 `context["retry_after_ms"]` 并按毫秒解析；键不存在或解析失败
/// 时返回 `None`。
///
/// # 参数
///
/// - `err`：待解析的错误。
///
/// # 返回
///
/// 服务端建议的等待时长；无建议时为 `None`。
pub(crate) fn retry_after_ms(err: &RiboError) -> Option<Duration> {
  err
    .context()
    .and_then(|c| c.get("retry_after_ms"))
    .and_then(|s| s.parse::<u64>().ok())
    .map(Duration::from_millis)
}

/// 执行一次退避休眠，可被取消令牌中断。
///
/// 处理流程：
///
/// 1. `delay` 为零时直接返回，不进入休眠分支；
/// 2. 未提供取消令牌时执行普通休眠；
/// 3. 提供令牌时用 `select!` 让休眠与取消竞争，取消先到即返回
///    `CANCELLED` 错误（PreStream 阶段），避免白等一个无效退避。
///
/// # Errors
///
/// 退避期间被取消时返回 `CANCELLED` 错误。
pub(crate) async fn sleep_backoff(
  delay: Duration,
  cancel: Option<&CancelToken>,
) -> RiboResult<()> {
  if delay.is_zero() {
    return Ok(());
  }

  match cancel {
    None => {
      tokio::time::sleep(delay).await;
      Ok(())
    }
    Some(cancel) => {
      tokio::select! {
        _ = tokio::time::sleep(delay) => Ok(()),
        () = cancel.cancelled() => {
          tracing::info!("retry backoff aborted by cancellation");
          Err(pre_stream(CANCELLED, "retry backoff aborted"))
        }
      }
    }
  }
}
