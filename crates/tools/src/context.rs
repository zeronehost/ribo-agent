//! 工具执行上下文：取消信号与流式输出回调。

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::types::ToolOutput;

/// 单次（或一轮）工具执行共享的上下文。
///
/// - `signal`：取消令牌，由调用方持有另一端；工具内部应通过
///   `ctx.is_cancelled()` / `ctx.signal()` 感知取消；
/// - `on_update`：流式输出回调，工具产出部分结果（长任务进度）
///   时调用 [`ToolContext::emit`] 推送给上层。
pub struct ToolContext {
  /// 取消信号。
  signal: CancellationToken,
  /// 流式输出回调（空实现表示调用方不关心中间输出）。
  on_update: Arc<dyn Fn(ToolOutput) + Send + Sync>,
}

impl ToolContext {
  /// 创建上下文：显式指定取消信号与输出回调。
  ///
  /// # 参数
  ///
  /// - `signal`：取消令牌（克隆自调用方持有的令牌）；
  /// - `on_update`：中间输出回调。
  pub fn new(
    signal: CancellationToken,
    on_update: Arc<dyn Fn(ToolOutput) + Send + Sync>,
  ) -> Self {
    Self { signal, on_update }
  }

  /// 创建"无人关心"的独立上下文：不可取消、输出丢弃。
  ///
  /// 适用于测试或纯同步的一次性调用。
  pub fn detached() -> Self {
    Self {
      signal: CancellationToken::new(),
      on_update: Arc::new(|_| {}),
    }
  }

  /// 取消信号（供 `tokio::select!` 等异步原语等待）。
  pub fn signal(&self) -> &CancellationToken {
    &self.signal
  }

  /// 推送一次中间输出给上层回调。
  pub fn emit(&self, output: ToolOutput) {
    (self.on_update)(output)
  }

  /// 是否已被取消（同步快照；异步等待请用 [`ToolContext::signal`]）。
  pub fn is_cancelled(&self) -> bool {
    self.signal.is_cancelled()
  }
}

impl core::fmt::Debug for ToolContext {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    // 回调不可打印，只输出取消状态
    f.debug_struct("ToolContext")
      .field("signal_cancelled", &self.is_cancelled())
      .finish_non_exhaustive()
  }
}
