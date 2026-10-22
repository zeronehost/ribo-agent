//! 单次流式请求的入参封装。
//!
//! 用结构体而非多参数，避免 [`Provider::stream`](crate::traits::Provider::stream)
//! 的签名随选项增长而反复变更。

use crate::{context::Context, model::Model};

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
}
