//! 提供方抽象：各模型服务需要实现的 trait 与请求钩子。

use ra_core::message::AssistantMessage;

use crate::{
  context::Context,
  model::Model,
  options::{StreamOptions, StreamOptionsPatch},
  stream::ProviderEventStream,
};

/// 一个模型提供方（服务）的统一接入抽象。
///
/// 实现方负责描述自身标识与支持的模型列表，并把一次请求转换为
/// [`ProviderEventStream`] 事件流返回。请求过程中的错误不以
/// `Result::Err` 体现，而是通过事件流中的
/// [`ProviderEvent::Error`](crate::stream::ProviderEvent::Error)
/// 终结事件携带，保证错误与部分输出走同一条消费路径。
#[async_trait::async_trait]
pub trait Provider: Sync + Send {
  /// 返回提供方标识（与 [`Model::provider`] 中的取值一致）。
  fn provider(&self) -> &str;

  /// 返回该提供方当前支持的全部模型元信息。
  fn models(&self) -> &[Model];

  /// 以流式方式发起一次模型请求。
  ///
  /// 调用方通过返回的 [`ProviderEventStream`] 逐条拉取事件并在结束后
  /// 取收敛的最终消息。
  ///
  /// # 参数
  ///
  /// - `model`：目标模型（取自 [`Provider::models`]）。
  /// - `ctx`：对话上下文（系统提示词、消息历史、工具集合）。
  /// - `options`：运行选项（超时、重试、采样参数、取消信号等）。
  async fn stream(
    &self,
    model: &Model,
    ctx: &Context,
    options: &StreamOptions,
  ) -> ProviderEventStream;
}

/// 请求生命周期的可选钩子。
///
/// 与 [`Provider`] 分离：钩子只做观测与选项改写，不负责实际请求，
/// 全部方法都有空默认实现，使用方按需覆盖。
pub trait ProviderHooks: Send + Sync {
  /// 请求发出前的钩子：可基于模型、上下文与基准选项返回一份局部覆盖，
  /// 该覆盖会合并到实际使用的 [`StreamOptions`] 上。
  ///
  /// 返回 `None`（默认）表示不修改任何选项。
  fn before_request(
    &self,
    _model: &Model,
    _ctx: &Context,
    _options: &StreamOptions,
  ) -> Option<StreamOptionsPatch> {
    None
  }

  /// 收到最终响应后的钩子：用于日志、指标上报等观测动作，默认无操作。
  fn after_response(&self, _model: &Model, _message: &AssistantMessage) {}
}
