//! 插件工具适配器：`Plugin` 工具 → `Arc<dyn AgentTool>` 的关键桥接。
//!
//! [`PluginToolAdapter`] 遍历一个 [`Plugin`](crate::traits::Plugin) 的
//! [`ToolManifest`](crate::traits::ToolManifest)，为每个工具生成实现
//! [`AgentTool`](ra_tools::traits::AgentTool) 的 [`PluginAgentTool`]，
//! 注入 `ToolManager` 后自动被 `ToolManagerAdapter` 暴露给主循环——
//! 这是保持 `AgentTool` / `ToolManager` / agent 主循环**零改动**的核心。
//!
//! 工具名策略（方案 A，设计文档 18_ §2.4）：适配器自动为每个工具加
//! `{plugin_id}__{tool_name}` 双下划线前缀。LLM 看到的是完整名
//! （如 `plugin_a__echo`），两个插件提供同名工具不再冲突；
//! `PluginAgentTool::execute` 派发时使用剥离前缀后的**短名**。

use std::sync::Arc;

use async_trait::async_trait;
use ra_core::{
  error::RiboResult,
  message::{Schema, TextContent, ToolSchema},
};
use ra_tools::{
  context::ToolContext,
  traits::AgentTool,
  types::{ToolExecutionMode, ToolExecutionResult, ToolOutput},
};

use crate::error::plugin_invalid_name;
use crate::traits::Plugin;

/// `plugin_id` 长度上界（1..=32 字节）。
///
/// 32 与 `tool_name` 的 64 **不是笔误，是刻意的命名空间预算**（18_ §2.4）：
/// 方案 A 下 `plugin_id` 出现在每个工具名前缀里，限制 32 可把完整名
/// 压在 32 + 2 分隔符 + 64 = 98 字节上界内，避免 LLM 看到的工具名过长。
pub const PLUGIN_ID_MAX_LEN: usize = 32;

/// 工具短名长度上界（1..=64 字节）。
pub const TOOL_NAME_MAX_LEN: usize = 64;

/// 校验插件 ID：`^[a-z][a-z0-9_]{0,31}$`（1..=32 字节，ASCII）。
///
/// # Errors
///
/// 空串 / 超长 / 首字符非小写字母 / 含非法字符 →
/// `plugin.plugin_invalid_name`。
pub fn validate_plugin_id(s: &str) -> RiboResult<()> {
  validate_ident(s, PLUGIN_ID_MAX_LEN, "plugin_id")
}

/// 校验工具短名：`^[a-z][a-z0-9_]{0,63}$`（1..=64 字节，ASCII）。
///
/// # Errors
///
/// 空串 / 超长 / 首字符非小写字母 / 含非法字符 →
/// `plugin.plugin_invalid_name`。
pub fn validate_tool_name(s: &str) -> RiboResult<()> {
  validate_ident(s, TOOL_NAME_MAX_LEN, "tool_name")
}

/// 逐字节标识符校验（不引入 regex 依赖；字节即字符——限定 ASCII）。
///
/// 先以**显式守卫**拒绝空串 / 超长，再索引 `b[0]`，不依赖 `&&`
/// 短路求值顺序（重构打乱顺序也不会 panic，18_ §三.5）。
fn validate_ident(s: &str, max: usize, field: &str) -> RiboResult<()> {
  let b = s.as_bytes();
  let err = || {
    plugin_invalid_name(
      field,
      s,
      format!("expected ^[a-z][a-z0-9_]*$ with length 1..={max}"),
    )
  };
  // 显式空串 / 超长守卫：与短路求值顺序无关，绝不越界访问 b[0]
  if b.is_empty() || b.len() > max {
    return Err(err());
  }
  if b[0].is_ascii_lowercase()
    && b
      .iter()
      .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
  {
    Ok(())
  } else {
    Err(err())
  }
}

/// 一个插件工具的 `AgentTool` 实现：schema 构造期固化，execute 委派
/// 给所属 [`Plugin`]。
struct PluginAgentTool {
  /// 构造期固化的工具声明（`name` 已带 `{plugin_id}__` 前缀）。
  schema: ToolSchema,
  /// 展示名（人类可读标签，取工具短名）。
  label: String,
  /// 所属插件（执行入口）。
  plugin: Arc<dyn Plugin>,
  /// 工具短名：派发 `Plugin::execute` 时使用（不含前缀）。
  tool_name: String,
  /// 是否强制顺序执行：由 `ToolManifest.idempotent` 取反得到。
  ///
  /// 非幂等工具（如 `send_email`）在并行批中交错执行的副作用顺序
  /// 不可控、重试会放大风险，故强制 `Sequential`（18_ §2.4）。
  sequential: bool,
}

#[async_trait]
impl AgentTool for PluginAgentTool {
  fn schema(&self) -> &ToolSchema {
    &self.schema
  }

  fn label(&self) -> &str {
    &self.label
  }

  fn execution_mode(&self) -> ToolExecutionMode {
    if self.sequential {
      ToolExecutionMode::Sequential
    } else {
      ToolExecutionMode::Parallel
    }
  }

  /// 委派所属插件执行，并把 JSON 结果包装为
  /// [`ToolExecutionResult`]：`content` 为文本（进入模型上下文），
  /// `details` 为原始 JSON（仅供上层消费，不进入模型上下文）。
  ///
  /// # Errors
  ///
  /// 插件返回的 `plugin.plugin_*` 错误原样透传（`ToolManager` 按
  /// 错误码记账与回灌）。
  async fn execute(
    &self,
    _tool_call_id: &str,
    args: serde_json::Value,
    ctx: &ToolContext,
  ) -> RiboResult<ToolExecutionResult> {
    let value = self.plugin.execute(&self.tool_name, args, ctx).await?;
    Ok(ToolExecutionResult {
      content: vec![ToolOutput::Text(TextContent {
        // 4KB 截断仅约束错误路径（§1.2）；成功结果的裁剪策略由
        // 插件侧与上层共同负责，此处不做二次截断
        content: value.to_string(),
        signature: None,
      })],
      details: Some(value),
      ..Default::default()
    })
  }
}

/// 插件工具适配器：为单个 [`Plugin`] 生成带前缀的 `AgentTool` 集合。
pub struct PluginToolAdapter {
  plugin: Arc<dyn Plugin>,
}

impl PluginToolAdapter {
  /// 以插件实例构造适配器。
  pub fn new(plugin: Arc<dyn Plugin>) -> Self {
    Self { plugin }
  }

  /// 方案 A 工具名：`{plugin_id}__{tool_name}` 双下划线前缀。
  ///
  /// 双下划线比单下划线更不易与工具名内部命名冲突。
  fn prefixed(plugin_id: &str, tool_name: &str) -> String {
    format!("{plugin_id}__{tool_name}")
  }

  /// 把一个工具声明桥接为 `Arc<dyn AgentTool>`。
  ///
  /// **关键不变量**：`ToolSchema.name` 为带前缀名（LLM 视角），
  /// `tool_name` 为短名（内部派发视角）。
  pub fn make_tool(
    &self,
    m: &crate::traits::ToolManifest,
  ) -> Arc<dyn AgentTool> {
    Arc::new(PluginAgentTool {
      schema: ToolSchema {
        name: Self::prefixed(self.plugin.id(), &m.name),
        description: m.description.clone(),
        input_schema: Schema(m.parameters.clone()),
        constrained_sampling: None,
      },
      label: m.name.clone(),
      plugin: self.plugin.clone(),
      tool_name: m.name.clone(),
      sequential: !m.idempotent,
    })
  }
}

#[cfg(test)]
mod tests {
  use std::sync::atomic::{AtomicUsize, Ordering};

  use serde_json::json;

  use super::*;
  use crate::traits::ToolManifest;

  /// 计数 Mock 插件：记录 `tools()` 调用次数，提供 `echo` 工具。
  struct MockPlugin {
    tools_calls: AtomicUsize,
  }

  #[async_trait::async_trait]
  impl Plugin for MockPlugin {
    fn id(&self) -> &str {
      "a"
    }

    fn tools(&self) -> Vec<ToolManifest> {
      self.tools_calls.fetch_add(1, Ordering::SeqCst);
      vec![ToolManifest {
        name: "echo".to_string(),
        description: "echo args back".to_string(),
        parameters: json!({ "type": "object", "properties": {} }),
        timeout_hint: None,
        idempotent: true,
        memory_limit_bytes: None,
      }]
    }

    async fn execute(
      &self,
      tool: &str,
      args: serde_json::Value,
      _ctx: &ToolContext,
    ) -> RiboResult<serde_json::Value> {
      assert_eq!(tool, "echo");
      Ok(args)
    }
  }

  /// 场景：`validate_ident` 边界——非法命名（空串 / 大写 / 数字开头 /
  /// 特殊字符 / 超长）全部拒绝，合法边界（1 字符、恰 32 / 64 字节、
  /// 下划线数字混合）通过。
  #[test]
  fn validate_ident_boundaries() {
    // 非法：空串 / 首字符下划线 / 数字开头 / 大写 / 连字符 / 点 / 空格 / 超长
    for bad in [
      "",
      "_abc",
      "1abc",
      "Abc",
      "a-b",
      "a.b",
      "a b",
      &"a".repeat(33),
    ] {
      assert!(
        validate_plugin_id(bad).is_err(),
        "plugin_id `{bad}` should be rejected"
      );
    }
    // tool_name 上界 64：63 通过、65 拒绝
    assert!(validate_tool_name(&"a".repeat(64)).is_ok());
    assert!(validate_tool_name(&"a".repeat(65)).is_err());

    // 合法边界
    assert!(validate_plugin_id("a").is_ok());
    assert!(validate_plugin_id(&"a".repeat(32)).is_ok());
    assert!(validate_tool_name("echo_2").is_ok());
  }

  /// 场景：适配器产出的 `AgentTool` 契约——schema 名带前缀、label
  /// 非空、幂等工具为 Parallel、execute 派发短名并返回 content + details。
  #[tokio::test]
  async fn adapter_tool_contract_with_prefix() {
    let plugin: Arc<dyn Plugin> = Arc::new(MockPlugin {
      tools_calls: AtomicUsize::new(0),
    });
    let adapter = PluginToolAdapter::new(plugin.clone());
    let manifest = plugin.tools().remove(0);
    let tool = adapter.make_tool(&manifest);

    // schema 名带前缀、描述透传
    let schema = tool.schema();
    assert_eq!(schema.name, "a__echo");
    assert_eq!(schema.description, "echo args back");

    // label 非空且为短名
    assert_eq!(tool.label(), "echo");

    // 幂等工具保持 Parallel 默认
    assert_eq!(tool.execution_mode(), ToolExecutionMode::Parallel);

    // execute 正确派发短名，content 为 JSON 文本、details 为原始 JSON
    let result = tool
      .execute("call-1", json!({ "x": 1 }), &ToolContext::detached())
      .await
      .expect("plugin tool should succeed");
    match &result.content[0] {
      ToolOutput::Text(t) => assert_eq!(t.content, r#"{"x":1}"#),
      other => panic!("expected text output, got {other:?}"),
    }
    assert_eq!(result.details, Some(json!({ "x": 1 })));
  }

  /// 场景：非幂等工具强制 Sequential 执行模式。
  #[tokio::test]
  async fn non_idempotent_tool_is_sequential() {
    struct SideEffectPlugin;

    #[async_trait::async_trait]
    impl Plugin for SideEffectPlugin {
      fn id(&self) -> &str {
        "fx"
      }

      fn tools(&self) -> Vec<ToolManifest> {
        vec![ToolManifest {
          name: "send".to_string(),
          description: "has side effects".to_string(),
          parameters: json!({ "type": "object", "properties": {} }),
          timeout_hint: None,
          idempotent: false,
          memory_limit_bytes: None,
        }]
      }

      async fn execute(
        &self,
        _tool: &str,
        _args: serde_json::Value,
        _ctx: &ToolContext,
      ) -> RiboResult<serde_json::Value> {
        Ok(json!(null))
      }
    }

    let adapter = PluginToolAdapter::new(Arc::new(SideEffectPlugin));
    let tool = adapter.make_tool(&SideEffectPlugin.tools()[0]);
    assert_eq!(tool.execution_mode(), ToolExecutionMode::Sequential);
  }

  /// 场景：`Arc<dyn Plugin>` 可跨 `tokio::spawn` 调用（Future: Send）。
  ///
  /// 任一 `impl Plugin` 漏标 `#[async_trait]`（Future 非 Send）时
  /// 本测试编译失败——这是 §2.5 的编译期契约测试。
  #[tokio::test]
  async fn plugin_trait_object_is_send() {
    let plugin: Arc<dyn Plugin> = Arc::new(MockPlugin {
      tools_calls: AtomicUsize::new(0),
    });
    let handle = tokio::spawn(async move {
      plugin
        .execute("echo", json!({}), &ToolContext::detached())
        .await
    });
    handle.await.expect("spawn ok").expect("execute ok");
  }
}
