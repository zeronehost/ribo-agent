//! 单次流式请求的入参封装。
//!
//! 用结构体而非多参数，避免 [`Provider::stream`](crate::traits::Provider::stream)
//! 的签名随选项增长而反复变更。

use std::{collections::BTreeMap, time::Duration};

use tokio_util::sync::CancellationToken;

use crate::{
  context::Context,
  model::{Model, ThinkingBudgets},
};

/// 一次流式请求的全部入参。
///
/// 仅借用、不持有所有权，因此必须绑定调用方栈帧的生命周期 `'a`；
/// 适配器不得把该结构体跨任务长期保存（确需保留时先克隆内部数据）。
#[derive(Debug, Clone, Copy)]
pub struct ProviderOptions<'a> {
  /// 本次调用的模型描述（含 base_url、能力位、计费与思考档位）。
  pub model: &'a Model,
  /// 本次调用的上下文（系统提示、历史消息、工具声明）。
  pub context: &'a Context,
  /// 流式运行选项（超时、重试、采样、取消等）。
  pub options: &'a StreamOptions,
}

/// 流式请求的运行参数集合。
///
/// 所有字段均有合理默认值（见 [`StreamOptions::default`]）；`cancel`
/// 默认携带一个未触发的取消令牌，便于上层随时中止。
#[derive(Debug, Clone)]
pub struct StreamOptions {
  /// 整个流式调用的超时上限；`None` 表示不限制。
  pub timeout: Option<Duration>,
  /// 重试策略选项（次数、退避上限、首包超时）。
  pub retry: RetryOptions,
  /// 采样参数（温度、top_p、停止词、随机种子）。
  pub sampling: SamplingOptions,
  /// 单次生成的最大 token 数；`None` 表示沿用提供方默认。
  pub max_tokens: Option<u64>,
  /// 思考 Token 预算（仅对按预算计费的提供方生效）。
  pub thinking_budgets: Option<ThinkingBudgets>,

  /// 取消令牌；`None` 表示本次调用不可中途取消。
  pub cancel: Option<CancelToken>,
  /// 附加 HTTP 请求头（有序键值对，允许重复键）。
  pub extra_headers: Vec<(String, String)>,
  /// 业务标签（供上层打点、路由使用，不参与线格式）。
  pub labels: BTreeMap<String, String>,
}

impl Default for StreamOptions {
  fn default() -> Self {
    Self {
      timeout: None,
      retry: RetryOptions::default(),
      sampling: SamplingOptions::default(),
      max_tokens: None,
      thinking_budgets: None,
      cancel: Some(CancelToken::new()),
      extra_headers: Vec::new(),
      labels: BTreeMap::new(),
    }
  }
}

/// 重试策略选项。
///
/// 字段为 `None` 时表示沿用重试层的内置默认值。
#[derive(Debug, Clone, Default)]
pub struct RetryOptions {
  /// 最大重试次数（不含首次请求）；`None` 表示沿用默认值。
  pub max_retries: Option<u32>,
  /// 单次退避等待的上限；`None` 表示沿用默认值。
  pub max_backoff: Option<Duration>,
  /// 首包（首个内容事件）超时时长。
  ///
  /// 目前为预留字段，重试层尚未消费；`None` 表示不做显式设置。
  pub first_token_timeout: Option<Duration>,
}

/// 采样参数集合。
///
/// 所有字段可选；`None` / 空列表表示沿用提供方默认采样行为。
#[derive(Debug, Clone, Default)]
pub struct SamplingOptions {
  /// 采样温度；`None` 表示沿用提供方默认。
  pub temperature: Option<f64>,
  /// 核采样（nucleus sampling）概率阈值；`None` 表示沿用提供方默认。
  pub top_p: Option<f32>,
  /// 停止词列表（命中即终止生成）。
  pub stop: Vec<String>,
  /// 随机种子（用于可复现采样）；`None` 表示不固定。
  pub seed: Option<u64>,
}

/// 流式请求的取消令牌。
///
/// 包装 [`tokio_util::sync::CancellationToken`]，避免把具体实现类型
/// 泄漏到公共 API；[`Default`] 实现产生一个未触发的令牌。
#[derive(Debug, Clone, Default)]
pub struct CancelToken(CancellationToken);

impl CancelToken {
  /// 创建一个尚未触发的取消令牌。
  pub fn new() -> Self {
    Self(CancellationToken::new())
  }

  /// 触发取消：等待 [`CancelToken::cancelled`] 的任务与订阅了令牌的流都会被唤醒。
  pub fn cancel(&self) {
    self.0.cancel();
  }
  /// 判断令牌是否已被触发。
  pub fn is_cancelled(&self) -> bool {
    self.0.is_cancelled()
  }
  /// 等待令牌被触发；已触发时立即返回。
  pub async fn cancelled(&self) {
    self.0.cancelled().await;
  }

  /// 由既有的 [`CancellationToken`] 构造取消令牌（桥接转换）。
  ///
  /// 用于上层已持有 `tokio_util` 令牌树（如 abort 树派生的子令牌）时，
  /// 将其包装进 [`StreamOptions::cancel`]，使外部取消信号传播到流式调用。
  /// 返回值与源令牌共享同一取消状态：源触发则本令牌立即观测到取消。
  ///
  /// # Examples
  ///
  /// ```
  /// use ra_provider::options::CancelToken;
  /// use tokio_util::sync::CancellationToken;
  ///
  /// let root = CancellationToken::new();
  /// let token = CancelToken::from_token(root.child_token());
  /// assert!(!token.is_cancelled());
  /// root.cancel();
  /// assert!(token.is_cancelled());
  /// ```
  pub fn from_token(token: CancellationToken) -> Self {
    Self(token)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  // 场景：from_token 桥接的令牌与源令牌共享取消状态。
  // 预期：根令牌触发后，包装令牌立即观测到取消，且 cancelled() 可等待完成。
  #[tokio::test]
  async fn from_token_shares_cancellation_with_source() {
    let root = CancellationToken::new();
    let token = CancelToken::from_token(root.child_token());
    assert!(!token.is_cancelled());
    root.cancel();
    assert!(token.is_cancelled());
    tokio::time::timeout(
      std::time::Duration::from_millis(50),
      token.cancelled(),
    )
    .await
    .expect("bridged token must observe cancellation");
  }

  // 场景：两个令牌各自独立（from_token 不产生隐式关联）。
  // 预期：触发 a 不影响默认构造的 b，保持取消语义隔离。
  #[tokio::test]
  async fn from_token_keeps_tokens_independent() {
    let a = CancelToken::from_token(CancellationToken::new());
    let b = CancelToken::default();
    a.cancel();
    assert!(a.is_cancelled());
    assert!(!b.is_cancelled());
  }
}
