//! 脚本化步骤：一次 `stream` 调用的行为脚本。
//!
//! 每个步骤描述 mock 在下一次调用中的完整行为：从建流成败，到事件
//! 回放节奏，再到终结方式。`Start` 事件由适配器自动补发，脚本步骤
//! 一律不包含它。

use std::{sync::Arc, time::Duration};

use ra_core::{
  error::RiboError,
  message::{AssistantMessage, StopReason},
};

use crate::{context::Context, stream::ProviderEvent};

/// [`MockStep::Complete`] 使用的动态构建闭包类型。
///
/// 入参是本次调用的上下文，返回 `(停止原因, 最终消息)`。
pub type CompleteBuilder =
  dyn Fn(&Context) -> (StopReason, AssistantMessage) + Send + Sync;

/// 脚本化步骤。
///
/// `Start` 事件由适配器自动补发（归属与时间戳在补发时统一注入），
/// 脚本步骤不包含它。
///
/// # 契约
///
/// - [`MockStep::Events`]：脚本中不得携带终结事件——终结由适配器
///   以 `Done(Stop)` 收敛（携带终结事件属于脚本编写错误，行为未定义）；
/// - [`MockStep::Fail`]：错误必须经 `pre_stream` 等阶段门构造器构造，
///   保证错误 `stage` 为 PreStream（契约层阶段门约束）。
#[derive(Clone)]
pub enum MockStep {
  /// 回放一组预置事件（`Start` 自动补发；未携带终结事件时以
  /// `Done(Stop)` 收尾，最终消息取最后一个内容事件的 partial）。
  Events(Vec<ProviderEvent>),
  /// 建流阶段失败（通过 `pre_stream` 等阶段门构造器构造的 `RiboError`）。
  Fail(RiboError),
  /// 建流成功，产出部分事件后以流式错误终结。
  StreamFail {
    /// 先产出的事件（不含 `Start` 与终结事件）。
    events: Vec<ProviderEvent>,
    /// 终结停止原因（`Error` 或 `Aborted`）。
    reason: StopReason,
    /// 结构化错误码（取自 `crate::error` 常量）。
    code: Option<&'static str>,
  },
  /// 建流成功但长时间不产出内容（用于 TTFT 守卫与看门狗测试）。
  Silent {
    /// 建流后保持静默的时长。
    duration: Duration,
  },
  /// 动态生成完成结果（同步路径：建流前推完 `Start` + `Done`）。
  ///
  /// 闭包入参是本次调用的上下文，返回 `(停止原因, 最终消息)`；
  /// 消息的 `provider` / `model` / `timestamp` 由适配器改写为 mock
  /// 的真实归属（mock 对调用方完全透明）。
  Complete(Arc<CompleteBuilder>),
}
