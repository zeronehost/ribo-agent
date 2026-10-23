//! 模型元信息模块：`Model` 及其能力/计费/思考档位子结构。
//!
//! [`Model`] 是**只读快照**：provider 在注册时提供，之后不得就地修改；
//! 变更必须以新实例替换（保证并发读取无锁）。
//!
//! 计费使用"微美元"（`u64`）而非浮点——`1 USD = 1,000,000 micros`，
//! 彻底避免精度丢失。阶梯费率通过 `tiers: Vec<ModelCostTiers>` 表达：
//! 当输入 token 超过 `input_token_above` 阈值时切换到新费率。

use crate::ids::{ModelId, ProviderId};
use std::collections::BTreeMap;

// ============================================================================
// 1. 输入模态
// ============================================================================

/// 模型支持的输入模态。
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
  /// 图像输入（图片理解/VLM）。
  Image,
  /// 音频输入（语音转写）。
  Audio,
  /// 视频输入（视频理解）。
  Video,
  /// 文档输入（PDF/Word 等）。
  Document,
}

// ============================================================================
// 2. 执行能力
// ============================================================================

/// 模型的核心执行能力位。
#[derive(
  Debug,
  Clone,
  Copy,
  PartialEq,
  Eq,
  Default,
  serde::Serialize,
  serde::Deserialize,
)]
#[serde(rename_all = "camelCase")]
pub struct ModelCapabilities {
  /// 是否支持流式输出。
  pub streaming: bool,
  /// 是否支持工具调用。
  pub tool_calling: bool,
  /// 是否支持并行工具调用（一次回复中同时触发多个工具）。
  pub parallel_tool_calls: bool,
  /// 是否支持结构化 JSON 输出（JSON Schema 约束）。
  pub structured_output: bool,
}

// ============================================================================
// 3. 思考（推理链）支持
// ============================================================================

/// 统一的思考强度档位。
///
/// 从 `Off` 到 `Max` 共 8 档，具体映射由各 provider 在
/// [`ThinkingLevelMap`] 中定义。并非所有 provider 都支持全部档位。
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
  /// 不进行思考。
  #[default]
  Off,
  /// 最小思考（极低延迟）。
  Minimal,
  /// 低强度思考。
  Low,
  /// 中等强度思考（默认档）。
  Medium,
  /// 高强度思考。
  High,
  /// 极高强度思考（高开销）。
  Extreme,
  /// 最大思考档位（最高开销，部分 provider 可能不支持）。
  Max,
}

/// 思考档位到提供方私有字符串的映射。
///
/// 键为统一档位，值为 API 实际接受的字符串（如 OpenAI 的 `"low"` / `"high"`）。
/// `None` 表示该档位在此 provider 上不支持。
pub type ThinkingLevelMap = BTreeMap<ThinkingLevel, Option<String>>;

/// 各思考档位的 Token 预算（适用于 Anthropic 等按预算计算的提供方）。
///
/// 非该模式的 provider 可忽略此字段。
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
  /// 极低强度思考的 Token 上限。
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub minimal: Option<u32>,
  /// 低强度思考的 Token 上限。
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub low: Option<u32>,
  /// 中等强度思考的 Token 上限。
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub medium: Option<u32>,
  /// 高强度思考的 Token 上限。
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub high: Option<u32>,
}

// ============================================================================
// 4. 计费模型（高精度 u64 与阶梯定价）
// ============================================================================
//
// 所有金额都用"微美元"（`u64`）表达，避免浮点精度丢失。
// `1 USD = 1_000_000 micros`。整数精度足以覆盖现有所有模型的定价范围。

/// 各类 token 的单价（微美元/百万 token）。
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
pub struct ModelCostRates {
  /// 输入 token 单价。
  pub input_per_million_micros: u64,
  /// 输出 token 单价。
  pub output_per_million_micros: u64,
  /// 缓存读取 token 单价（部分 provider 支持 prompt 缓存）。
  pub cache_read_per_million_micros: u64,
  /// 缓存写入 token 单价。
  pub cache_write_per_million_micros: u64,
}

/// 阶梯费率：当输入 token 达到指定阈值后适用新费率。
///
/// 例如 `"input_token_above": 128000` 表示前 128k token 用基础费率，
/// 之后切换到本档费率。
#[derive(
  Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "camelCase")]
pub struct ModelCostTiers {
  /// 本档费率，序列化时展平到同级。
  #[serde(flatten)]
  pub rates: ModelCostRates,
  /// 本档生效的输入 token 用量下限。
  pub input_token_above: u64,
}

/// 模型的完整计费规则。
///
/// 阶梯费率列表按 `input_token_above` 升序排列；为空表示单一费率无阶梯。
#[derive(
  Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "camelCase")]
pub struct ModelCost {
  /// 基础费率（无阶梯时唯一生效）。
  #[serde(flatten)]
  pub rates: ModelCostRates,
  /// 阶梯费率列表（按 `input_token_above` 升序生效）。
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub tiers: Vec<ModelCostTiers>,
}

// ============================================================================
// 5. 核心 Model 结构体
// ============================================================================

/// 一个可调用模型的静态描述信息。
///
/// 注册时创建，之后只读变更；并发读取无锁。
/// 基础字段（id、provider、base_url）必填，其余可选默认。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
  /// 强类型模型标识。
  pub id: ModelId,
  /// 归属提供方。
  pub provider: ProviderId,
  /// 面向用户展示的模型名称（与 id 可能不同）。
  pub name: String,
  /// 提供方 API 的基础地址（支持代理）。
  pub base_url: String,

  /// 上下文窗口大小（单请求最多 token 数）。
  pub context_window: u32,
  /// 单次输出允许的最大 token 数。
  pub max_tokens: u32,

  /// 支持的输入模态列表。
  pub input: Vec<InputModality>,
  /// 模型执行能力（工具调用、结构化输出等）。
  pub capabilities: ModelCapabilities,

  /// 思考档位映射（若支持）。
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub thinking_level_map: Option<ThinkingLevelMap>,
  /// 思考 Token 预算（若支持）。
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub thinking_budgets: Option<ThinkingBudgets>,

  /// 计费规则。
  pub cost: ModelCost,

  /// 调用该模型时附加的自定义 HTTP 请求头（鉴权外的附加信息）。
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub header: Option<BTreeMap<String, String>>,
  /// 提供方私有的采样参数透传表（如 OpenAI 的 `frequency_penalty`）。
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub sampling_params: Option<serde_json::Map<String, serde_json::Value>>,
  /// 兼容性配置（适配特定 API 版本的临时覆盖项）。
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub compat: Option<serde_json::Value>,
}

impl Model {
  /// 创建仅包含基础信息的模型描述，其余字段取默认值。
  ///
  /// `provider` / `id` 参数是强类型（[`ProviderId`] / [`ModelId`]），
  /// 调用方应先用 [`ProviderId::parse`] / [`ModelId::parse`] 校验原始输入，
  /// 或从已有注册表直接取现成实例。
  pub fn new(
    provider: ProviderId,
    id: ModelId,
    name: impl Into<String>,
    base_url: impl Into<String>,
  ) -> Self {
    Self {
      id,
      provider,
      name: name.into(),
      base_url: base_url.into(),
      context_window: 0,
      max_tokens: 0,
      input: vec![InputModality::Text],
      capabilities: ModelCapabilities::default(),
      thinking_level_map: None,
      thinking_budgets: None,
      cost: ModelCost::default(),
      header: None,
      sampling_params: None,
      compat: None,
    }
  }

  // === 链式构造器（Builder Pattern） ===
  //
  // 所有 with_* 方法都返回 `Self` 以支持链式调用，
  // 与 Model::new 组合使用。

  /// 设置上下文窗口大小。
  pub fn with_context_window(mut self, size: u32) -> Self {
    self.context_window = size;
    self
  }

  /// 设置单次输出最大 token 数。
  pub fn with_max_tokens(mut self, max: u32) -> Self {
    self.max_tokens = max;
    self
  }

  /// 设置支持的输入模态列表。
  pub fn with_input(mut self, input: Vec<InputModality>) -> Self {
    self.input = input;
    self
  }

  /// 设置执行能力位。
  pub fn with_capabilities(mut self, caps: ModelCapabilities) -> Self {
    self.capabilities = caps;
    self
  }

  /// 设置思考档位映射表。
  pub fn with_thinking_level_map(mut self, map: ThinkingLevelMap) -> Self {
    self.thinking_level_map = Some(map);
    self
  }

  /// 设置思考 Token 预算。
  pub fn with_thinking_budgets(mut self, budgets: ThinkingBudgets) -> Self {
    self.thinking_budgets = Some(budgets);
    self
  }

  /// 设置计费规则。
  pub fn with_cost(mut self, cost: ModelCost) -> Self {
    self.cost = cost;
    self
  }

  /// 设置自定义 HTTP 请求头。
  pub fn with_header(mut self, header: BTreeMap<String, String>) -> Self {
    self.header = Some(header);
    self
  }

  /// 设置采样参数透传表。
  pub fn with_sampling_params(
    mut self,
    params: serde_json::Map<String, serde_json::Value>,
  ) -> Self {
    self.sampling_params = Some(params);
    self
  }

  /// 设置兼容性配置。
  pub fn with_compat(mut self, compat: serde_json::Value) -> Self {
    self.compat = Some(compat);
    self
  }

  // === 校验逻辑 ===

  /// 校验模型配置的合法性。应在 Provider 注册时调用。
  ///
  /// # Errors
  ///
  /// - `context_window` 或 `max_tokens` 为 0
  /// - `max_tokens` 大于 `context_window`
  /// - `base_url` 为空
  pub fn validate(&self) -> Result<(), String> {
    if self.base_url.is_empty() {
      return Err(format!("Model '{}' has empty base_url", self.id));
    }
    if self.context_window == 0 {
      return Err(format!("Model '{}' context_window cannot be zero", self.id));
    }
    if self.max_tokens == 0 {
      return Err(format!("Model '{}' max_tokens cannot be zero", self.id));
    }
    if self.max_tokens > self.context_window {
      return Err(format!(
        "Model '{}' max_tokens ({}) cannot exceed context_window ({})",
        self.id, self.max_tokens, self.context_window
      ));
    }
    Ok(())
  }
}
