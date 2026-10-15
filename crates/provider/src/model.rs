//! 模型元信息：能力、思考档位、输入模态与计费结构。

use std::collections::BTreeMap;

/// 一个可调用模型的静态描述信息。
///
/// 通常由提供方在初始化时注册，描述模型的接入地址、能力边界与计费规则；
/// 请求时以本结构为参数之一传给 `Provider::stream`。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
  /// 模型在提供方处的实际标识（API 请求中使用的 ID）。
  pub id: String,
  /// 面向用户展示的模型名称。
  pub name: String,
  /// 所属提供方标识，与 `Provider::provider()` 对应。
  pub provider: String,
  /// 提供方 API 的基础地址。
  pub base_url: String,
  /// 是否支持思考（推理链）输出。
  pub reasoning: bool,
  /// 各思考档位到提供方私有档位字符串的映射；
  /// 值为 `None` 表示该档位存在但无需传参（采用提供方默认）。
  pub thinking_level_map: Option<ThinkingLevelMap>,
  /// 模型支持的输入模态列表（文本、图片、音频等）。
  pub input: Vec<InputModality>,
  /// 模型计费规则。
  pub cost: ModelCost,
  /// 上下文窗口大小（token 数）。
  pub context_window: u64,
  /// 单次输出允许的最大 token 数。
  pub max_tokens: u64,
  /// 调用该模型时附加的自定义 HTTP 请求头；缺省为 `None`。
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub header: Option<BTreeMap<String, String>>,
  /// 提供方私有的采样参数透传表，未覆盖到的采样参数以此为准；
  /// 缺省为 `None`。
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub sampling_params: Option<serde_json::Map<String, serde_json::Value>>,
  /// 兼容性配置：不同提供方私有协议的适配参数，由具体实现解释；
  /// 缺省为 `None`。
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub compat: Option<serde_json::Value>,
}

/// 思考（推理链）强度档位。
///
/// 按序递增：`Off` < `Minimal` < `Low` < `Medium` < `High` < `Extreme` < `Max`，
/// 派生了 `Ord`，可直接比较档位高低。serde 外部表示为小写
/// （`rename_all = "lowercase"`），默认值为 [`ThinkingLevel::Off`]。
#[derive(
  Debug,
  Clone,
  Copy,
  Default,
  PartialEq,
  Eq,
  PartialOrd,
  Ord,
  Hash,
  serde::Serialize,
  serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
  /// 关闭思考。
  #[default]
  Off,
  /// 极简思考。
  Minimal,
  /// 低档思考。
  Low,
  /// 中档思考。
  Medium,
  /// 高档思考。
  High,
  /// 极高档思考。
  Extreme,
  /// 提供方允许的最高档思考。
  Max,
}

/// 各思考档位的 token 预算。
///
/// 只列出需要显式预算的档位，缺省字段表示该档位不设置预算；
/// 序列化为 camelCase，缺省字段整体省略。
#[derive(
  Debug,
  Clone,
  Copy,
  Default,
  PartialEq,
  Eq,
  serde::Serialize,
  serde::Deserialize,
)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingBudgets {
  /// Minimal 档位的 token 预算。
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub minimal: Option<u64>,
  /// Low 档位的 token 预算。
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub low: Option<u64>,
  /// Medium 档位的 token 预算。
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub medium: Option<u64>,
  /// High 档位的 token 预算。
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub high: Option<u64>,
}

/// 思考档位到提供方私有档位字符串的映射。
///
/// 键为统一档位，值为提供方 API 实际接受的字符串；
/// 值为 `None` 表示该档位直接省略参数、使用提供方默认配置。
pub type ThinkingLevelMap = BTreeMap<ThinkingLevel, Option<String>>;

/// 模型支持的输入模态。
///
/// serde 外部表示为小写（`rename_all = "lowercase"`），
/// 例如 [`InputModality::Image`] 序列化为 `"image"`。
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
#[serde(rename_all = "lowercase")]
pub enum InputModality {
  /// 纯文本输入。
  Text,
  /// 图片输入。
  Image,
  /// 音频输入。
  Audio,
  /// 视频输入。
  Video,
  /// 文档（文件）输入。
  Document,
}

/// 模型的完整计费规则：基础单价加上按用量分段的阶梯单价。
#[derive(
  Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "camelCase")]
pub struct ModelCost {
  /// 基础费率，序列化时字段展平到本结构同级。
  #[serde(flatten)]
  pub rates: ModelCostRates,
  /// 阶梯费率列表，按输入 token 用量达到阈值后切换；为空时不参与序列化。
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub tiers: Vec<ModelCostTiers>,
}

/// 各类 token 的单价。
///
/// 字段当前为反序列化专用（由配置注入后供计费逻辑读取），
/// 单位由配置约定（通常为每百万 token 的美元价格）。
#[derive(
  Debug, Clone, Copy, Default, PartialEq, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "camelCase")]
pub struct ModelCostRates {
  // 每个输入 token 的单价。
  input_token: f64,
  // 每个输出 token 的单价。
  output_token: f64,
  // 每个缓存读取 token 的单价。
  cache_read: f64,
  // 每个缓存写入 token 的单价。
  cache_write: f64,
}

/// 一档阶梯费率：当输入 token 用量达到 [`input_token_above`](Self::input_token_above)
/// 阈值后适用该费率。
#[derive(
  Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "camelCase")]
pub struct ModelCostTiers {
  /// 本档费率，序列化时字段展平到本结构同级。
  #[serde(flatten)]
  pub rates: ModelCostRates,
  /// 本档生效的输入 token 用量下限（达到该值后适用 `rates`）。
  pub input_token_above: i64,
}
