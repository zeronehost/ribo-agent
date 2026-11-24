//! Agent 工具 trait：声明、参数预处理与执行入口。

use ra_core::{error::RiboResult, message::ToolSchema};

use crate::{
  context::ToolContext,
  types::{ToolExecutionMode, ToolExecutionResult},
};

/// Agent 可调用的工具抽象。
///
/// 实现方持有自身 immutable 状态，`execute` 会被并发调用
/// （`Send + Sync` 约束保证跨线程共享安全）。
///
/// 契约说明：
///
/// - `execute` 返回 `Err` 表示工具级失败，由上层（`ToolManager`）
///   记入熔断统计并转为 `is_error` 结果回灌模型；
/// - 参数校验分两层：`prepare_arguments` 做实现方自定义预处理
///   （补默认值、形态归一），JSON Schema 校验由 `ToolManager`
///   依据 `schema().input_schema` 统一执行。
#[async_trait::async_trait]
pub trait AgentTool: Send + Sync {
  /// 工具声明（名称、描述与 JSON Schema）；注册后不得变化。
  fn schema(&self) -> &ToolSchema;

  /// 展示名（人类可读，用于 UI 标签，如 "Calculator"）。
  fn label(&self) -> &str;

  /// 执行模式：默认 [`ToolExecutionMode::Parallel`]（可与其它
  /// 并行安全工具并发执行）；有副作用或对顺序敏感的实现应覆盖。
  fn execution_mode(&self) -> ToolExecutionMode {
    ToolExecutionMode::Parallel
  }

  /// 执行前参数预处理钩子（默认原样透传）。
  ///
  /// 典型用途：填充默认值、把宽松形态归一为 schema 要求的形态。
  /// 返回 `Err` 等价于参数校验失败（不进入执行、不计熔断失败——
  /// 入参问题由调用方模型修正）。
  fn prepare_arguments(
    &self,
    args: serde_json::Value,
  ) -> RiboResult<serde_json::Value> {
    Ok(args)
  }

  /// 执行工具。
  ///
  /// # 参数
  ///
  /// - `tool_call_id`：本次调用的 id（透传给结果回灌链路）；
  /// - `args`：已经 `prepare_arguments` 与 schema 校验的入参；
  /// - `ctx`：执行上下文（取消信号 + 流式输出回调）。
  async fn execute(
    &self,
    tool_call_id: &str,
    args: serde_json::Value,
    ctx: &ToolContext,
  ) -> RiboResult<ToolExecutionResult>;
}
