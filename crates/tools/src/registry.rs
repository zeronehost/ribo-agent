//! 并发安全的工具注册表。

use std::sync::Arc;

use dashmap::{DashMap, Entry};
use ra_core::{error::RiboResult, message::ToolSchema};

use crate::{
  error::{already_registered, not_found},
  traits::AgentTool,
};

/// 工具注册表：以工具名为键的并发安全哈希表。
///
/// 读多写少的场景用 [`DashMap`] 实现，注册/注销与查找可并发进行；
/// `Debug` 输出只含排序后的工具名，不打印工具内部状态。
#[derive(Default)]
pub struct ToolRegistry {
  /// 工具名 → 工具实例。
  tools: DashMap<String, Arc<dyn AgentTool>>,
}

impl core::fmt::Debug for ToolRegistry {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    // 只输出工具名快照：工具实例无 Debug 约束，且名字足以定位注册状态
    let mut names: Vec<String> = self
      .tools
      .iter()
      .map(|entry| entry.key().to_string())
      .collect();
    names.sort_unstable();
    f.debug_struct("ToolRegistry")
      .field("tools", &names)
      .finish()
  }
}

impl ToolRegistry {
  /// 创建空注册表（等价于 [`Default`]）。
  pub fn new() -> Self {
    Self::default()
  }

  /// 注册一个工具；同名工具已存在时拒绝。
  ///
  /// # Errors
  ///
  /// 同名工具已注册 → `tool.already_registered`（Severity: Warning）。
  pub fn registry(&self, tool: Arc<dyn AgentTool>) -> RiboResult<()> {
    let name = tool.schema().name.clone();
    // entry API 保证"检查 + 插入"原子完成，无 TOCTOU 竞态
    match self.tools.entry(name.clone()) {
      Entry::Occupied(_) => {
        // 拒绝分支与成功注册对称记录，便于排查配置覆盖问题
        tracing::warn!(tool=%name, "duplicate tool registration refused");
        Err(already_registered(name))
      }
      Entry::Vacant(slot) => {
        slot.insert(tool);
        tracing::info!(tool=%name, "tool registered");
        Ok(())
      }
    }
  }

  /// 按名称注销工具。
  ///
  /// # Errors
  ///
  /// 工具不存在 → `tool.not_found`。
  pub fn unregister(&self, name: &str) -> RiboResult<()> {
    // DashMap::remove 返回 Option<(K, V)>，None 即未命中
    match self.tools.remove(name) {
      Some(_) => {
        tracing::info!(tool = %name, "tool unregistered");
        Ok(())
      }
      None => Err(not_found(name)),
    }
  }

  /// 按工具名前缀批量注销，返回移除数量。
  ///
  /// 先收集命中前缀的键快照再逐个移除：避免在 DashMap 迭代过程中
  /// 直接删除引发分片死锁。任何顺序都不保证，移除数量以返回值为准。
  pub fn unregister_by_prefix(&self, prefix: &str) -> usize {
    let keys: Vec<String> = self
      .tools
      .iter()
      .filter(|entry| entry.key().starts_with(prefix))
      .map(|entry| entry.key().clone())
      .collect();
    let mut removed = 0;
    for key in keys {
      if self.tools.remove(&key).is_some() {
        removed += 1;
        tracing::info!(tool = %key, prefix = %prefix, "tool unregistered by prefix");
      }
    }
    removed
  }

  /// 按名称取工具实例快照（`Arc` 克隆，注册表生命周期独立）。
  pub fn get(&self, name: &str) -> Option<Arc<dyn AgentTool>> {
    self.tools.get(name).map(|entry| Arc::clone(entry.value()))
  }

  /// 取全部工具实例（顺序不定）。
  pub fn list(&self) -> Vec<Arc<dyn AgentTool>> {
    self
      .tools
      .iter()
      .map(|entry| Arc::clone(entry.value()))
      .collect()
  }

  /// 取全部工具声明，按名称字典序排序（保证发给模型的 schema 顺序稳定）。
  pub fn schemas(&self) -> Vec<ToolSchema> {
    let mut schemas: Vec<ToolSchema> = self
      .tools
      .iter()
      .map(|entry| entry.value().schema().clone())
      .collect();
    schemas.sort_by(|a, b| a.name.cmp(&b.name));
    schemas
  }

  /// 已注册工具数。
  pub fn len(&self) -> usize {
    self.tools.len()
  }

  /// 注册表是否为空。
  pub fn is_empty(&self) -> bool {
    self.tools.is_empty()
  }
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use ra_core::message::{Schema, ToolSchema};
  use serde_json::json;

  use super::*;
  use crate::{
    context::ToolContext,
    error::{ALREADY_REGISTERED, NOT_FOUND},
    types::ToolExecutionResult,
  };

  /// 最小桩工具：只提供声明，execute 恒返回默认空结果。
  struct StubTool {
    /// 构造期固化的工具声明。
    schema: ToolSchema,
  }

  impl StubTool {
    /// 构造指定名称的桩工具并装箱为 trait 对象。
    fn boxed(name: &str) -> Arc<dyn AgentTool> {
      Arc::new(Self {
        schema: ToolSchema {
          name: name.to_string(),
          description: "stub tool for registry tests".to_string(),
          input_schema: Schema(json!({ "type": "object", "properties": {} })),
          constrained_sampling: None,
        },
      })
    }
  }

  #[async_trait::async_trait]
  impl AgentTool for StubTool {
    fn schema(&self) -> &ToolSchema {
      &self.schema
    }

    fn label(&self) -> &str {
      "Stub"
    }

    async fn execute(
      &self,
      _tool_call_id: &str,
      _args: serde_json::Value,
      _ctx: &ToolContext,
    ) -> RiboResult<ToolExecutionResult> {
      Ok(ToolExecutionResult::default())
    }
  }

  /// 场景：注册后按名命中，schemas() 按名称字典序稳定排序。
  ///
  /// 预期：get 命中已注册工具、未注册名称返回 None；schemas 名称
  /// 序列与插入顺序无关，恒为字典序。
  #[test]
  fn register_then_get_hit_and_schemas_sorted() {
    let registry = ToolRegistry::new();
    registry
      .registry(StubTool::boxed("beta"))
      .expect("register beta");
    registry
      .registry(StubTool::boxed("alpha"))
      .expect("register alpha");
    registry
      .registry(StubTool::boxed("gamma"))
      .expect("register gamma");

    assert!(
      registry.get("beta").is_some(),
      "registered tool should be found"
    );
    assert!(registry.get("missing").is_none());
    assert_eq!(registry.len(), 3);
    assert!(!registry.is_empty());

    let names: Vec<String> =
      registry.schemas().into_iter().map(|s| s.name).collect();
    // 顺序稳定：与插入顺序无关，按名称字典序输出
    assert_eq!(names, vec!["alpha", "beta", "gamma"]);
  }

  /// 场景：重复注册同名工具被拒绝且原实例未被覆盖。
  ///
  /// 预期：第二次注册返回 `tool.already_registered`；get 仍返回
  /// 第一个实例（Arc 指针相等）。
  #[test]
  fn duplicate_registration_keeps_first_instance() {
    let registry = ToolRegistry::new();
    let first = StubTool::boxed("dup");
    registry
      .registry(first.clone())
      .expect("first registration");

    let err = registry.registry(StubTool::boxed("dup")).unwrap_err();
    assert_eq!(err.code(), ALREADY_REGISTERED);

    let got = registry
      .get("dup")
      .expect("first instance stays registered");
    assert!(
      Arc::ptr_eq(&first, &got),
      "duplicate registration must not overwrite the original tool"
    );
  }

  /// 场景：注销已注册工具与注销不存在的工具。
  ///
  /// 预期：注销后 get 为 None 且注册表为空；再次注销返回
  /// `tool.not_found`。
  #[test]
  fn unregister_removes_tool_and_reports_missing() {
    let registry = ToolRegistry::new();
    registry
      .registry(StubTool::boxed("gone"))
      .expect("register gone");

    registry
      .unregister("gone")
      .expect("unregister existing tool");
    assert!(registry.get("gone").is_none());
    assert!(registry.is_empty());

    let err = registry.unregister("gone").unwrap_err();
    assert_eq!(err.code(), NOT_FOUND);
  }
}
