//! 工具边界适配：ra-tools `ToolManager` 到 `ToolProvider` 的桥接。
//!
//! ra-tools 不反向依赖 ra-agent，故适配器放在本 crate（07 §7.3 决策 4）：
//! 循环只认 [`ToolProvider`](crate::traits::ToolProvider) trait，
//! 生产实现由此处包装 `ToolManager` 提供。

use std::sync::Arc;

use ra_core::{
  error::RiboResult,
  message::{ToolCall, ToolSchema},
};
use ra_tools::{
  context::ToolContext,
  manager::ToolManager,
  types::{ToolExecutionMode, ToolExecutionResult},
};

use crate::traits::ToolProvider;

/// `ToolManager` 的 `ToolProvider` 适配器。
///
/// 克隆廉价（内部仅一个 `Arc`），供 `LoopDeps.tools` 跨任务共享。
#[derive(Clone)]
pub struct ToolManagerAdapter {
  manager: Arc<ToolManager>,
}

impl ToolManagerAdapter {
  /// 包装工具管理器。
  pub fn new(manager: Arc<ToolManager>) -> Self {
    Self { manager }
  }
}

#[async_trait::async_trait]
impl ToolProvider for ToolManagerAdapter {
  async fn get_tool_schemas(&self) -> Vec<ToolSchema> {
    self.manager.get_tool_schemas()
  }

  // 所有权入参是为并行批 move 进 spawn 任务而设计；
  // ToolManager 收借用，此处零成本转引。
  async fn execute(
    &self,
    call: ToolCall,
    ctx: ToolContext,
  ) -> RiboResult<ToolExecutionResult> {
    self.manager.execute(&call, &ctx).await
  }

  // 未知工具按顺序执行（保守默认：宁可串行也不冒险并行）。
  fn execution_mode(&self, name: &str) -> ToolExecutionMode {
    self
      .manager
      .get(name)
      .map_or(ToolExecutionMode::Sequential, |tool| tool.execution_mode())
  }
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use ra_core::message::ToolCall;
  use ra_tools::{
    builtin::create_echo_tool, context::ToolContext, manager::ToolManager,
    types::ToolOutput,
  };
  use serde_json::json;

  use super::*;

  /// 构造注册了 echo 工具的适配器。
  fn echo_adapter() -> ToolManagerAdapter {
    let manager = ToolManager::new();
    manager
      .register(create_echo_tool())
      .expect("echo 工具注册成功");
    ToolManagerAdapter::new(Arc::new(manager))
  }

  /// 构造一次 echo 调用。
  fn echo_call(id: &str, text: &str) -> ToolCall {
    ToolCall {
      id: id.to_string(),
      name: "echo".to_string(),
      arguments: json!({ "text": text }),
      signature: None,
      namespace: None,
    }
  }

  // 场景：适配器透传工具声明。
  // 预期：schema 列表非空且按名升序（ra-tools 契约，供请求 tools 段）。
  #[tokio::test]
  async fn schemas_are_non_empty_and_sorted() {
    let adapter = echo_adapter();

    let schemas = adapter.get_tool_schemas().await;

    assert!(!schemas.is_empty());
    let names: Vec<String> = schemas.into_iter().map(|s| s.name).collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted);
  }

  // 场景：适配器执行 echo 工具（零成本转引 ToolManager）。
  // 预期：回显文本与入参一致（ToolOutput::Text 载荷）。
  #[tokio::test]
  async fn execute_echo_returns_text() {
    let adapter = echo_adapter();

    let result = adapter
      .execute(echo_call("call-1", "hi 世界"), ToolContext::detached())
      .await
      .expect("echo 应执行成功");

    match &result.content[0] {
      ToolOutput::Text(text) => assert_eq!(text.content, "hi 世界"),
      other => panic!("应为文本输出，实际 {other:?}"),
    }
  }

  // 场景：execution_mode 透传注册工具的模式。
  // 预期：echo 未覆写 AgentTool 默认，返回 Parallel。
  #[test]
  fn execution_mode_matches_registered_tool() {
    let adapter = echo_adapter();

    assert_eq!(adapter.execution_mode("echo"), ToolExecutionMode::Parallel);
  }

  // 场景：execution_mode 查询未注册工具。
  // 预期：保守默认返回 Sequential（宁可串行也不冒险并行）。
  #[test]
  fn execution_mode_unknown_defaults_to_sequential() {
    let adapter = echo_adapter();

    assert_eq!(
      adapter.execution_mode("ghost"),
      ToolExecutionMode::Sequential
    );
  }

  // 场景：适配器克隆廉价（内部仅一个 Arc）。
  // 预期：克隆体执行结果与原体一致（共享同一管理器）。
  #[tokio::test]
  async fn cloned_adapter_shares_manager() {
    let adapter = echo_adapter();
    let clone = adapter.clone();

    let result = clone
      .execute(echo_call("call-2", "shared"), ToolContext::detached())
      .await
      .expect("克隆体应共享管理器并可执行");

    match &result.content[0] {
      ToolOutput::Text(text) => assert_eq!(text.content, "shared"),
      other => panic!("应为文本输出，实际 {other:?}"),
    }
  }
}
