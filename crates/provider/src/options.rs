//! 流式请求的运行选项与局部覆盖（patch）。

use std::{collections::BTreeMap, time::Duration};

use tokio_util::sync::CancellationToken;

use crate::model::{ThinkingBudgets, ThinkingLevel};

/// 一次流式请求的运行选项。
///
/// 与 [`crate::context::Context`]（对话内容）分离：本结构控制「如何发起请求」，
/// 包括鉴权、超时重试、取消信号与采样参数等。字段大多可选，
/// 未设置时由提供方采用自身默认值。
#[derive(Debug, Clone)]
pub struct StreamOptions {
  /// 提供方 API 密钥；通过其他渠道（环境变量、默认头）鉴权时可为 `None`。
  pub api_key: Option<String>,
  /// 单次请求超时时间。
  pub timeout: Option<Duration>,
  /// 失败后的最大重试次数。
  pub max_retries: Option<u32>,
  /// 重试退避的最大间隔（实际间隔按退避策略计算，以此值封顶）。
  pub max_retry_delay: Option<Duration>,
  /// 附加到请求上的自定义 HTTP 头。
  pub headers: Option<BTreeMap<String, String>>,
  /// 透传给提供方或本地中间件的自定义元数据（不影响请求语义）。
  pub metadata: Option<BTreeMap<String, String>>,
  /// 提示词缓存保留策略。
  pub cache_retention: CacheRetention,
  /// 会话 ID，用于多轮请求之间的缓存命中与会话关联。
  pub session_id: Option<String>,
  /// 取消令牌：触发后进行中的请求应尽快终止。
  pub signal: CancellationToken,
  /// 请求使用的思考档位；`None` 表示不启用或沿用模型默认。
  pub reasoning: Option<ThinkingLevel>,
  /// 本次输出的最大 token 数。
  pub max_tokens: Option<u64>,
  /// 采样温度。
  pub temperature: Option<f64>,
  /// 各思考档位的 token 预算覆盖。
  pub thinking_budgets: Option<ThinkingBudgets>,
}

/// 提示词缓存的保留时长策略，默认值为 [`CacheRetention::Short`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheRetention {
  /// 不使用缓存。
  None,
  /// 短期保留（默认，平衡成本与命中率）。
  #[default]
  Short,
  /// 长期保留（适合重复率高的固定上下文）。
  Long,
}

/// [`StreamOptions`] 的局部覆盖集合。
///
/// 每个字段是双层 `Option`：外层表示「本次 patch 是否涉及该字段」，
/// 内层表示「覆盖成什么值」，因此内层为 `None` 时语义是**显式清空**。
/// Map 类型字段以「值为 `Some` 插入、值为 `None` 删除」的方式按键合并。
///
/// 典型来源是 `ProviderHooks::before_request`：钩子无需构造完整选项，
/// 只需返回要改动的部分，再由 [`apply`](Self::apply) 合并到基准选项上。
#[derive(Debug, Clone, Default)]
pub struct StreamOptionsPatch {
  /// 覆盖超时；内层 `None` 表示清空为不限制/默认。
  pub timeout: Option<Option<Duration>>,
  /// 覆盖最大重试次数；内层 `None` 表示清空。
  pub max_retries: Option<Option<u32>>,
  /// 覆盖最大重试间隔；内层 `None` 表示清空。
  pub max_retry_delay: Option<Option<Duration>>,
  /// 按键合并 HTTP 头：`Some(v)` 插入/覆盖，`None` 删除该头。
  pub headers: Option<BTreeMap<String, Option<String>>>,
  /// 按键合并元数据：`Some(v)` 插入/覆盖，`None` 删除该键。
  pub metadata: Option<BTreeMap<String, Option<String>>>,
  /// 覆盖缓存保留策略；内层 `None` 回落到 [`CacheRetention`] 的默认值。
  pub cache_retention: Option<Option<CacheRetention>>,
  /// 覆盖最大输出 token；内层 `None` 表示清空。
  pub max_tokens: Option<Option<u64>>,
  /// 覆盖采样温度；内层 `None` 表示清空。
  pub temperature: Option<Option<f64>>,
  /// 覆盖会话 ID；内层 `None` 表示清空。
  pub session_id: Option<Option<String>>,
}

impl StreamOptionsPatch {
  /// 把本 patch 原地合并到基准选项上。
  ///
  /// 只处理外层为 `Some` 的字段（patch 未涉及的字段保持不变）；
  /// 标量字段整体替换，两个 map 字段按键做插入/删除合并，
  /// 合并后为空的 map 会收敛回 `None`，避免发送无意义的空对象。
  pub fn apply(&self, options: &mut StreamOptions) {
    // ===== 标量字段：整体替换 =====
    if let Some(v) = self.timeout {
      options.timeout = v;
    }
    if let Some(v) = self.max_retries {
      options.max_retries = v;
    }
    if let Some(v) = self.max_retry_delay {
      options.max_retry_delay = v;
    }
    // 清空时回落到枚举默认（Short），与 derive(Default) 的基准行为一致
    if let Some(v) = self.cache_retention {
      options.cache_retention = v.unwrap_or_default();
    }
    if let Some(v) = self.max_tokens {
      options.max_tokens = v;
    }
    if let Some(v) = self.temperature {
      options.temperature = v;
    }
    if let Some(v) = &self.session_id {
      options.session_id = v.clone();
    }
    // ===== Map 字段：按键插入/删除，空 map 收敛为 None =====
    if let Some(patch) = &self.headers {
      let mut map = options.headers.take().unwrap_or_default();
      for (k, v) in patch {
        match v {
          Some(v) => map.insert(k.clone(), v.clone()),
          None => map.remove(k),
        };
      }
      options.headers = if map.is_empty() { None } else { Some(map) };
    }
    if let Some(patch) = &self.metadata {
      let mut map = options.metadata.take().unwrap_or_default();
      for (k, v) in patch {
        match v {
          Some(v) => map.insert(k.clone(), v.clone()),
          None => map.remove(k),
        };
      }
      options.metadata = if map.is_empty() { None } else { Some(map) };
    }
  }
}
