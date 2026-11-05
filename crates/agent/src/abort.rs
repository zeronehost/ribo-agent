//! 取消句柄：包装 `CancellationToken` 并提供树形派生（D5）。
//!
//! 父令牌触发时全部子令牌级联触发（tokio 原生 `child_token` 语义）。
//! 传播路径：会话取消根 → 循环配置的 `signal` → 每次 LLM 流与
//! 工具执行各派生一个子令牌，粒度化取消。

use tokio_util::sync::CancellationToken;

/// 取消句柄（会话取消根）。
#[derive(Debug, Clone)]
pub struct AbortHandle {
  token: CancellationToken,
}

impl AbortHandle {
  /// 新建根句柄。
  pub fn new() -> Self {
    Self {
      token: CancellationToken::new(),
    }
  }

  /// 派生子句柄：工具执行 / 单次 LLM 流各持一个，粒度化取消。
  pub fn child(&self) -> Self {
    Self {
      token: self.token.child_token(),
    }
  }

  /// 触发取消（级联到全部后代）。
  pub fn abort(&self) {
    self.token.cancel();
  }

  /// 是否已触发。
  pub fn is_aborted(&self) -> bool {
    self.token.is_cancelled()
  }

  /// 异步等待触发（已触发时立即返回）。
  pub async fn cancelled(&self) {
    self.token.cancelled().await;
  }

  /// 取内部令牌引用（供 `LoopConfig.signal` 等下游克隆共享）。
  pub fn token(&self) -> &CancellationToken {
    &self.token
  }
}

// 无参 new 必须配 Default（clippy new_without_default）
impl Default for AbortHandle {
  fn default() -> Self {
    Self::new()
  }
}

#[cfg(test)]
mod tests {
  use std::time::Duration;

  use super::*;

  // 场景：AbortHandle::new 与 Default 的初始状态及独立性。
  // 预期：两者均未触发；触发其一不影响另一实例（各自持有独立令牌）。
  #[test]
  fn new_and_default_start_unaborted() {
    let handle = AbortHandle::new();
    let default = AbortHandle::default();

    assert!(!handle.is_aborted());
    assert!(!default.is_aborted());

    handle.abort();
    assert!(handle.is_aborted());
    assert!(!default.is_aborted(), "实例之间不应共享取消状态");
  }

  // 场景：父句柄触发后子句柄级联取消（D5 树形语义）。
  // 预期：子句柄与孙句柄全部同步进入已触发状态。
  #[test]
  fn parent_abort_cascades_to_descendants() {
    let parent = AbortHandle::new();
    let child = parent.child();
    let grandchild = child.child();

    assert!(!child.is_aborted());
    assert!(!grandchild.is_aborted());

    parent.abort();
    assert!(child.is_aborted(), "子句柄应级联触发");
    assert!(grandchild.is_aborted(), "孙句柄应级联触发");
  }

  // 场景：cancelled() 异步等待触发，含「先 abort 再等待」的立即返回分支。
  // 预期：等待任务在 abort 后完成；已触发句柄的 cancelled() 不挂起。
  #[tokio::test]
  async fn cancelled_await_resolves_on_abort() {
    let handle = AbortHandle::new();
    let waiter = handle.child();
    let task = tokio::spawn(async move {
      waiter.cancelled().await;
    });
    tokio::task::yield_now().await;

    handle.abort();
    task.await.expect("等待任务应正常结束");

    // 已触发句柄的 cancelled() 立即返回（超时保护防挂起）
    tokio::time::timeout(Duration::from_millis(100), handle.cancelled())
      .await
      .expect("已触发句柄的 cancelled 应立即返回");
  }

  // 场景：token() 返回内部令牌引用，与句柄共享取消状态。
  // 预期：句柄 abort 后经 token() 克隆出的令牌同步触发（LoopConfig 联动）。
  #[test]
  fn token_reference_shares_cancellation() {
    let handle = AbortHandle::new();
    let token = handle.token().clone();

    assert!(!token.is_cancelled());
    handle.abort();
    assert!(token.is_cancelled(), "克隆令牌应随句柄触发");
  }
}
