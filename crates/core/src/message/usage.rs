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

#[cfg(test)]
mod tests {
  use super::*;

  /// 场景：必填字段与可选字段均存在时逐项累加。
  ///
  /// 预期：计数、费用与可选字段全部相加。
  #[test]
  fn add_accumulates_all_fields() {
    let mut total = Usage {
      input: 1,
      output: 2,
      cache_read: 3,
      cache_write: 4,
      total_tokens: 10,
      cost: UsageCost {
        input: 0.1,
        output: 0.2,
        cache_read: 0.3,
        cache_write: 0.4,
        total: 1.0,
      },
      cache_write_1h: Some(5),
      reasoning: Some(6),
    };
    let other = Usage {
      input: 10,
      output: 20,
      cache_read: 30,
      cache_write: 40,
      total_tokens: 100,
      cost: UsageCost {
        input: 1.0,
        output: 2.0,
        cache_read: 3.0,
        cache_write: 4.0,
        total: 10.0,
      },
      cache_write_1h: Some(50),
      reasoning: Some(60),
    };

    total.add(&other);

    assert_eq!(total.input, 11);
    assert_eq!(total.output, 22);
    assert_eq!(total.cache_read, 33);
    assert_eq!(total.cache_write, 44);
    // `total_tokens` 是提供方上报值，当前累加实现只汇总各分量、不改写它。
    assert_eq!(total.total_tokens, 10);
    assert_eq!(total.cache_write_1h, Some(55));
    assert_eq!(total.reasoning, Some(66));
    assert_eq!(
      total.cost,
      UsageCost {
        input: 1.1,
        output: 2.2,
        cache_read: 3.3,
        cache_write: 4.4,
        total: 11.0,
      }
    );
  }

  /// 场景：对方可选字段缺省时，本方的可选字段保持原值。
  ///
  /// 预期：`cache_write_1h` 保留、`reasoning` 仍为 `None`。
  #[test]
  fn add_keeps_optional_none_when_other_absent() {
    let mut total = Usage::zero();
    total.cache_write_1h = Some(7);
    let other = Usage::zero();

    total.add(&other);

    assert_eq!(total.cache_write_1h, Some(7));
    assert_eq!(total.reasoning, None);
  }

  /// 场景：`total_tokens` 为 0 时退化为分量求和，大于 0 时直接采用上报值。
  #[test]
  fn context_tokens_prefers_reported_total_or_component_sum() {
    let mut usage = Usage {
      input: 3,
      output: 2,
      cache_read: 4,
      cache_write: 1,
      ..Usage::zero()
    };
    assert_eq!(usage.context_tokens(), 10, "未上报总量时应按分量求和");

    usage.total_tokens = 42;
    assert_eq!(usage.context_tokens(), 42, "已上报总量时应直接采用");
  }

  /// 场景：serde 往返保留可选字段，并使用 camelCase 键。
  #[test]
  fn serde_roundtrip_preserves_optional_fields() {
    let usage = Usage {
      input: 1,
      output: 2,
      cache_read: 3,
      cache_write: 4,
      total_tokens: 10,
      cost: UsageCost {
        input: 0.1,
        output: 0.2,
        cache_read: 0.3,
        cache_write: 0.4,
        total: 1.0,
      },
      cache_write_1h: Some(5),
      reasoning: Some(6),
    };

    let value = serde_json::to_value(&usage).expect("序列化应成功");
    assert_eq!(value["cacheWrite1h"], 5);
    assert_eq!(value["reasoning"], 6);
    assert_eq!(value["cost"]["cacheRead"], 0.3);

    let back: Usage = serde_json::from_value(value).expect("反序列化应成功");
    assert_eq!(back, usage);
  }

  /// 场景：可选字段为 `None` 时序列化省略对应键。
  #[test]
  fn serde_skips_absent_optional_fields() {
    let value = serde_json::to_value(Usage::zero()).expect("序列化应成功");

    assert!(value.get("cacheWrite1h").is_none());
    assert!(value.get("reasoning").is_none());
  }
}
