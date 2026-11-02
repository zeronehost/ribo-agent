//! 工具执行的结果与输出类型。

use ra_core::message::{Content, ImageContent, TextContent, Usage};

/// 工具执行模式：决定调度器能否把多个工具调用并发执行。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ToolExecutionMode {
  /// 顺序执行：有副作用或顺序敏感的工具应选择此模式。
  Sequential,
  /// 并行执行（默认）：工具无副作用或并发安全，可与其它并行工具并发。
  #[default]
  Parallel,
}

/// 工具输出的内容块（文本或图片），回灌模型时映射为对应 `Content`。
#[derive(Debug, Clone)]
pub enum ToolOutput {
  /// 文本输出块。
  Text(TextContent),
  /// 图片输出块（当前内置工具未产出，为多模态工具预留）。
  Image(ImageContent),
}

impl From<ToolOutput> for Content {
  fn from(value: ToolOutput) -> Self {
    match value {
      ToolOutput::Image(v) => Content::Image(v),
      ToolOutput::Text(v) => Content::Text(v),
    }
  }
}

/// 单次工具执行的结果。
#[derive(Debug, Clone, Default)]
pub struct ToolExecutionResult {
  /// 回灌模型的内容块列表（可多个）。
  pub content: Vec<ToolOutput>,
  /// 结构化附加数据（仅供上层消费，不进入模型上下文）。
  pub details: Option<serde_json::Value>,
  /// 工具自身上报的用量（如计费型 API 的 token 统计）；缺省为无。
  pub usage: Option<Usage>,
  /// 执行期间动态注册的新工具名（供上层追加 schema 到下一轮请求）。
  pub added_tool_names: Vec<String>,
  /// 终止标记：`true` 表示本工具终结整个代理循环（如 exit 工具）。
  pub terminate: bool,
}
