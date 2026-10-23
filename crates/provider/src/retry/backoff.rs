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

#[cfg(test)]
mod tests {
  use super::*;
  use ra_core::error::Severity;

  /// 场景：零延迟直接返回 `Ok`，不进入休眠分支。
  ///
  /// 预期：立即返回 `Ok(())`，且不改变取消令牌状态。
  #[tokio::test(start_paused = true)]
  async fn zero_delay_returns_ok_immediately() {
    let token = CancelToken::new();

    let res = sleep_backoff(Duration::ZERO, Some(&token)).await;

    assert!(res.is_ok());
    assert!(!token.is_cancelled());
  }

  /// 场景：无取消令牌时按给定时正常休眠后返回。
  ///
  /// 预期：虚拟时间精确推进 100ms 后返回 `Ok(())`。
  #[tokio::test(start_paused = true)]
  async fn sleeps_fully_without_cancel_token() {
    let start = tokio::time::Instant::now();

    let res = sleep_backoff(Duration::from_millis(100), None).await;

    assert!(res.is_ok());
    assert_eq!(start.elapsed(), Duration::from_millis(100));
  }

  /// 场景：退避期间触发取消 → 返回 `CANCELLED` 错误，
  /// 且不计入上游故障（严重程度为 Debug，属用户行为而非故障）。
  ///
  /// 预期：休眠 1h 的退避在 10ms 虚拟时间被取消打断；
  /// 错误码为 CANCELLED、严重程度为 Debug。
  #[tokio::test(start_paused = true)]
  async fn cancellation_during_backoff_returns_cancelled() {
    let token = CancelToken::new();
    // 模拟上层在退避进行到 10ms 时触发取消
    let canceller = {
      let token = token.clone();
      tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        token.cancel();
      })
    };

    let err = sleep_backoff(Duration::from_secs(3600), Some(&token))
      .await
      .expect_err("退避期被取消应返回 Err");

    canceller.await.expect("取消任务不应 panic");
    assert_eq!(err.code(), CANCELLED);
    assert!(
      matches!(err.severity(), Severity::Debug),
      "CANCELLED 不应计入上游故障: {:?}",
      err.severity()
    );
  }
}
