//! 工具相关模型：工具调用请求、工具执行结果，以及向模型声明工具能力的 schema。

use std::collections::BTreeMap;

use super::{content::Content, usage::Usage};

/// 模型发起的一次工具调用请求。
///
/// 该结构同时作为助手消息中的一种内容块（[`Content::ToolCall`]）存在；
/// 工具执行完成后，需用 [`ToolResultMessage`] 并带上相同的 [`id`](Self::id) 回灌。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
  /// 工具调用 ID，用于在工具结果消息中关联本次调用。
  pub id: String,
  /// 被调用的工具名。
  pub name: String,
  /// 模型生成的调用参数，通常是 JSON 对象；具体结构由工具自身约定。
  pub arguments: serde_json::Value,
  /// 部分提供方对工具调用签发的校验签名；无签名机制时为 `None`，
  /// 为 `None` 时不参与序列化。
  #[serde(skip_serializing_if = "Option::is_none")]
  pub signature: Option<String>,
  /// 工具所属命名空间（部分提供方用于区分内置工具与第三方工具）；
  /// 为 `None` 时不参与序列化。
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub namespace: Option<String>,
}

/// 工具执行完成后回灌给模型的结果消息。
///
/// 通过 [`tool_call_id`](Self::tool_call_id) 与助手消息中的某次
/// [`ToolCall`] 关联；执行是否失败由 [`is_error`](Self::is_error) 标记。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultMessage {
  /// 对应的 [`ToolCall::id`]。
  pub tool_call_id: String,
  /// 对应的 [`ToolCall::name`]。
  pub tool_name: String,
  /// 执行结果是否为错误：`true` 表示工具执行失败，`content` 中通常为错误描述。
  pub is_error: bool,
  /// 结果产生时间，Unix 毫秒时间戳。
  pub timestamp: i64,
  /// 结果内容块列表（文本、图片等）；为空时不参与序列化，
  /// 反序列化缺省该字段时按空列表处理。
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub content: Vec<Content>,
  /// 供调试或提供方专用的附加结构化细节；为 `None` 时不参与序列化。
  #[serde(skip_serializing_if = "Option::is_none")]
  pub details: Option<serde_json::Value>,
  /// 本次工具执行额外消耗的 token 用量（工具自身又触发了模型调用时才有）；
  /// 为 `None` 时不参与序列化。
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub usage: Option<Usage>,
  /// 执行过程中新发现/注册的工具名列表（如 MCP 动态工具发现场景）；
  /// 为空时不参与序列化。
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub added_tool_names: Vec<String>,
}

/// 向模型声明的一个可调用工具的描述（schema）。
///
/// 随对话上下文的 `tools` 列表一起提交，模型据此决定是否发起 [`ToolCall`]。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolSchema {
  /// 工具名，模型发起调用时（[`ToolCall::name`]）必须与该值一致。
  pub name: String,
  /// 工具用途的自然语言描述，模型据此判断何时选择该工具。
  pub description: String,
  /// 工具入参的 JSON Schema 定义。
  pub input_schema: Schema,
  /// 对工具输出的约束采样配置（要求输出符合 JSON Schema 或指定语法）；
  /// 为 `None` 时不约束输出格式，且该字段不参与序列化。
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub constrained_sampling: Option<ConstrainedSamplingConfig>,
}

/// 以 JSON 对象形式承载的 JSON Schema。
///
/// 对 [`serde_json::Value`] 的 newtype 包装，序列化时透明展开为内部 JSON，
/// 本 crate 不限制 schema 的具体字段，交由工具提供方按标准约定填写。
#[derive(
  Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct Schema(pub serde_json::Value);

/// 约束采样配置：要求模型输出必须符合指定结构。
///
/// 序列化为内部标签枚举（`type` 字段区分）：`"json_schema"` 或 `"grammar"`。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConstrainedSamplingConfig {
  /// 要求工具输出符合一份 JSON Schema。
  JsonSchema {
    // 对结构符合度的要求强度（偏好或强制）。
    #[serde(rename = "strict")]
    strict: ConstrainedStrictness,
  },
  /// 要求工具输出符合某种形式语法（正则、Lark 文法等）。
  Grammar {
    // 按提供方支持的语法格式组织的语法定义集合。
    variants: GrammarVariants,
  },
}

/// 约束采样的严格程度，serde 外部表示为小写。
#[derive(
  Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum ConstrainedStrictness {
  /// 尽量满足约束，模型可在必要时回退到自由输出。
  Prefer,
  /// 强制满足约束，不符合结构的输出应被拒绝。
  Require,
}

/// 支持的语法格式。
///
/// 序列化键名为提供方约定的私有字符串（`openai_lark` / `openai_regex`），
/// 不随枚举命名风格自动派生，故逐变体显式指定。
#[derive(
  Debug,
  Clone,
  Copy,
  PartialEq,
  Eq,
  PartialOrd,
  Ord,
  Hash,
  serde::Serialize,
  serde::Deserialize,
)]
pub enum GrammarFormat {
  /// Lark 文法（BNF 风格的结构化语法描述）。
  #[serde(rename = "openai_lark")]
  OpenaiLark,
  /// 正则表达式语法。
  #[serde(rename = "openai_regex")]
  OpenaiRegex,
}

/// 按语法格式组织的语法映射。
pub type GrammarVariants = BTreeMap<GrammarFormat, String>;
