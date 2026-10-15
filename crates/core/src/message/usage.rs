//! Token 用量与费用统计模型。

/// 一次模型调用（或多条消息聚合后）的 token 用量与费用。
///
/// 所有计数字段均为累计量，可通过 [`Usage::add`] 逐段累加；
/// 费用部分由 [`UsageCost`] 承载。可选字段缺省（`None`）时不参与序列化。
///
/// # Examples
///
/// ```
/// use ra_core::message::Usage;
///
/// let usage = Usage::zero();
/// assert_eq!(usage.total_tokens, 0);
/// assert_eq!(usage.context_tokens(), 0);
/// ```
#[derive(
  Debug, Default, Clone, PartialEq, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
  /// 输入（提示词）token 数。
  pub input: i64,
  /// 输出（生成内容）token 数。
  pub output: i64,
  /// 命中缓存读取的 token 数。
  pub cache_read: i64,
  /// 写入缓存的 token 数。
  pub cache_write: i64,
  /// 提供方上报的总 token 数；为 `0` 时 [`Usage::context_tokens`] 会退化为各分量求和。
  pub total_tokens: i64,
  /// 本次调用的费用明细。
  pub cost: UsageCost,
  /// 1 小时有效期缓存写入量（部分提供方单独计费）；未上报时为 `None`。
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub cache_write_1h: Option<i64>,
  /// 推理（思考）token 数；未单独上报时为 `None`。
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub reasoning: Option<i64>,
}

impl Usage {
  /// 构造全部计数与费用均为零的空用量，用于尚无任何消耗的占位消息
  ///（见 `AssistantMessage::empty`）。
  ///
  /// # Examples
  ///
  /// ```
  /// use ra_core::message::Usage;
  ///
  /// let usage = Usage::zero();
  /// assert_eq!(usage.input + usage.output, 0);
  /// ```
  pub fn zero() -> Self {
    Self {
      input: 0,
      output: 0,
      cache_read: 0,
      cache_write: 0,
      total_tokens: 0,
      cost: UsageCost::default(),
      cache_write_1h: None,
      reasoning: None,
    }
  }

  /// 将另一份用量累加到自身上，用于把流式分片响应或多轮对话的消耗汇总。
  ///
  /// 必填字段逐项相加；可选字段（[`cache_write_1h`](Self::cache_write_1h)、
  /// [`reasoning`](Self::reasoning)）在任一方存在时按 `0` 补齐后相加；
  /// 费用委托内部的 `UsageCost::add` 累加。
  ///
  /// # Examples
  ///
  /// ```
  /// use ra_core::message::Usage;
  ///
  /// let mut total = Usage::zero();
  /// let mut part = Usage::zero();
  /// part.output = 12;
  /// total.add(&part);
  /// assert_eq!(total.output, 12);
  /// ```
  pub fn add(&mut self, other: &Usage) {
    self.input += other.input;
    self.output += other.output;
    self.cache_read += other.cache_read;
    self.cache_write += other.cache_write;
    // 可选字段：对方有值时按零补齐后累加，保持累加后为具体数值而非缺省。
    if let Some(v) = other.cache_write_1h {
      self.cache_write_1h = Some(self.cache_write_1h.unwrap_or(0) + v);
    }
    if let Some(v) = other.reasoning {
      self.reasoning = Some(self.reasoning.unwrap_or(0) + v);
    }
    self.cost.add(&other.cost);
  }

  /// 返回用于上下文窗口统计的 token 总量。
  ///
  /// 优先采用提供方上报的 [`total_tokens`](Self::total_tokens)；
  /// 当其为 `0`（提供方未上报）时，退化为输入、输出、缓存读写各项之和。
  ///
  /// # Examples
  ///
  /// ```
  /// use ra_core::message::Usage;
  ///
  /// let mut usage = Usage::zero();
  /// usage.input = 3;
  /// usage.output = 2;
  /// assert_eq!(usage.context_tokens(), 5);
  ///
  /// usage.total_tokens = 10;
  /// assert_eq!(usage.context_tokens(), 10);
  /// ```
  pub fn context_tokens(&self) -> i64 {
    let sum = self.total_tokens;
    if sum > 0 {
      sum
    } else {
      self.input + self.output + self.cache_read + self.cache_write
    }
  }
}

/// 用量对应的费用明细，单位与精度由提供方约定（通常为美元、`f64`）。
///
/// 字段全部为可选上报项的兜底值：未上报时默认为 `0.0`，
/// 随 [`Usage`] 聚合时在内部逐字段累加。
#[derive(
  Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "camelCase")]
pub struct UsageCost {
  /// 输入 token 费用。
  pub input: f64,
  /// 输出 token 费用。
  pub output: f64,
  /// 缓存读取 token 费用（通常费率极低）。
  pub cache_read: f64,
  /// 缓存写入 token 费用。
  pub cache_write: f64,
  /// 总费用。
  pub total: f64,
}

impl UsageCost {
  // 将另一份费用明细逐字段累加到自身上。
  fn add(&mut self, other: &UsageCost) {
    self.input += other.input;
    self.output += other.output;
    self.cache_read += other.cache_read;
    self.cache_write += other.cache_write;
    self.total += other.total;
  }
}
