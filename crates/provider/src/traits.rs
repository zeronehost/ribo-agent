//! Provider trait 与错误谓词扩展 trait。
//!
//! [`Provider`] 是所有模型提供方的统一抽象，适配器通过实现该 trait
//! 把各家 LLM 提供方的 API 线格式屏蔽在 crate 内部。
//!
//! [`ProviderErrorExt`] 是对 [`ra_core::error::RiboError`] 的本地扩展，
//! 提供重试、降级、熔断所需的分类谓词。具体实现见 [`crate::error`]。

use std::sync::Arc;

use ra_core::error::RiboResult;

use crate::{
  error::ErrorStage,
  ids::{ModelId, ProviderId},
  model::Model,
  options::ProviderOptions,
  stream::ProviderEventStream,
};

// ============================================================================
// 错误分类谓词扩展 trait
// ============================================================================
//
// 统一按 `(domain, code)` 二元组做 match，domain 固定为 `"provider"`
// （由 error/constants.rs 的 DOMAIN 常量定义）。阶段门短路规则：
// InStream 阶段一律不可重试/降级（除 circuit_trigger 外），
// 因为重试会与已产出的内容重复。

/// 错误分类谓词扩展 trait。
///
/// 实现于 [`RiboError`](ra_core::error::RiboError)，内部用 `(code, stage)`
/// 二元组做 match + 阶段门短路，保证跨阶段误用被运行期纠正。
pub trait ProviderErrorExt {
  /// 读取 `RiboError.context["stage"]` 反解出错误生命周期阶段。
  ///
  /// 无 stage context 时保守按 PreStream 处理（旧数据兼容）。
  fn stage(&self) -> ErrorStage;

  /// 是否可重试。
  ///
  /// InStream 阶段一律不可重试（重试会与已产出的内容重复）。
  /// 可重试的 code：`RATE_LIMITED` / `TIMEOUT` / `SERVER_ERROR` /
  /// `TRANSPORT` / `OVERLOADED`。
  fn is_retryable(&self) -> bool;

  /// 是否可降级（换同一 provider 的其他候选）。
  ///
  /// InStream 阶段一律降级为 false。
  fn is_degradable(&self) -> bool;

  /// 是否可跨 provider 降级（换另一家提供方）。
  ///
  /// 跨 provider 能解决：限流（配额耗尽）、鉴权、配额。
  /// ServerError / Timeout / Transport 等跨 provider 意义不大
  /// （如果是网络问题，哪家都挂）。
  fn is_cross_provider_degradable(&self) -> bool;

  /// 是否应触发熔断器。
  ///
  /// 不受阶段门影响——流内错误同样可能触发熔断
  /// （例如 SSE 连接反复中断说明上游不稳定）。
  fn is_circuit_trigger(&self) -> bool;
}

// ============================================================================
// Provider trait
// ============================================================================

/// 模型提供方统一抽象。
///
/// # 契约（实现方必须遵守）
///
/// 1. 建流阶段失败（HTTP 连接、认证、限流）必须返回 `Err(RiboError)`，
///    且错误的 `stage` 应为 PreStream；
/// 2. 流内阶段失败必须通过 [`crate::stream::ProviderEventStreamProducer::error`]
///    终结，不得让 `stream` 返回 `Err`；
/// 3. 实现内部不得自行重试，重试归属 `ra-retry` 层或网关；
/// 4. 必须实现 `Send + Sync`，适配到并发任务中。
#[async_trait::async_trait]
pub trait Provider: Send + Sync {
  /// 提供方标识，在同一注册表内唯一。
  fn id(&self) -> &ProviderId;

  /// 该提供方当前可用的全部模型描述。
  ///
  /// 返回借用切片而非克隆，调用方不得跨越 `await` 长期持有。
  fn models(&self) -> &[Model];

  /// 判断该提供方是否支持指定模型。
  ///
  /// 默认实现按 [`Self::models`] 的 `id` 做精确匹配；
  /// 若模型列表是远端动态拉取的，可在实现中覆写。
  fn supports(&self, model: &ModelId) -> bool {
    self.models().iter().any(|m| &m.id == model)
  }

  /// 该实现内部是否已自带重试能力。
  ///
  /// 默认 `false`。返回 `true` 时网关不得再叠加自身重试，
  /// 否则会形成"重试的重试"放大上游压力。
  /// 由于 `Arc<dyn Provider>` 无法通过 `type_name` 判别具体类型，
  /// 该标记是唯一的叠加防护手段。
  fn is_retrying_provider(&self) -> bool {
    false
  }

  /// 发起一次流式请求，返回事件流消费端。
  ///
  /// # 参数
  ///
  /// - `options`：模型描述与请求上下文，字段均为借用，生命周期绑定本次调用。
  ///
  /// # 返回
  ///
  /// `Ok(stream)` 表示建流成功，后续内容与终结事件通过事件流交付；
  /// `Err(err)` 仅表示建流阶段失败（见上方契约第 1 条）。
  ///
  /// # Errors
  ///
  /// provider/模型不存在、鉴权失败、限流、连接失败等建流阶段错误，
  /// 均以 `Err(RiboError)` 返回，且 `stage` 应为 PreStream。
  async fn stream(
    &self,
    options: ProviderOptions<'_>,
  ) -> RiboResult<ProviderEventStream>;
}

/// 注册表的只读查询视图。
///
/// 网关与工作流引擎通过该 trait 解析路由目标，不关心注册表的具体实现
/// （静态注册、动态加载或测试替身）。所有方法只读，实现必须支持并发调用。
pub trait ProviderCatalog: Send + Sync {
  /// 按 provider 标识取实现实例，不存在时返回 `None`。
  fn provider(&self, id: &ProviderId) -> Option<Arc<dyn Provider>>;

  /// 按 `(provider, model)` 二元组查询完整模型描述。
  fn model(&self, id: &ProviderId, model: &ModelId) -> Option<Model>;

  /// 在全部 provider 中查找声明了该模型的提供方。
  ///
  /// 返回 `(provider_id, model)`；多个 provider 声明同名模型时，
  /// 由实现方决定优先级（建议取注册顺序首个）。
  fn find_model(&self, model: &ModelId) -> Option<(ProviderId, Model)>;

  /// 列出全部已注册的 provider 标识。
  fn providers(&self) -> Vec<ProviderId>;
}
