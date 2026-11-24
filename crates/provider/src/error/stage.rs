//! 错误生命周期阶段枚举。
//!
//! 错误分为两个阶段：
//!
//! - **PreStream**（建流前）：请求刚构造、HTTP 刚发出还没收到任何字节。
//!   这个阶段的失败可以被重试层或网关重新发起整个请求。
//! - **InStream**（流内）：已经收到至少一个事件（或进入流式解析）。
//!   这个阶段的失败**不可重试**——重试会重新发起请求、与已产出的部分内容重复。
//!
//! 阶段信息写入 [`RiboError::context`] 的 `"stage"` 键，由
//! [`crate::traits::ProviderErrorExt::stage`] 读回。

/// 错误生命周期阶段。
///
/// 序列化形式为小写字符串 `"pre_stream"` / `"in_stream"`，写入
/// `RiboError::context` 键时同样使用该形式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorStage {
  /// 建流前失败：HTTP 连接、认证、限流等。可重试。
  PreStream,
  /// 流内失败：已收到事件后发生的解析错误、传输中断等。不可重试。
  InStream,
}

impl ErrorStage {
  /// 序列化为 context 中使用的小写字符串。
  pub fn as_str(&self) -> &'static str {
    match self {
      Self::InStream => "in_stream",
      Self::PreStream => "pre_stream",
    }
  }

  /// 从 context 字符串反序列化。
  ///
  /// 未知值返回 `None`，调用方应决定是否用默认值（通常默认 PreStream）。
  pub fn from(s: impl Into<String>) -> Option<Self> {
    match s.into().as_str() {
      "pre_stream" => Some(Self::PreStream),
      "in_stream" => Some(Self::InStream),
      _ => None,
    }
  }
}

impl core::fmt::Display for ErrorStage {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    f.write_str(self.as_str())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// 场景：字符串表示与反序列化互为逆运算。
  ///
  /// 预期：`as_str()` / `Display` / `from` 三者对同一阶段保持一致。
  #[test]
  fn as_str_and_from_roundtrip() {
    for stage in [ErrorStage::PreStream, ErrorStage::InStream] {
      assert_eq!(ErrorStage::from(stage.as_str()), Some(stage));
      assert_eq!(stage.to_string(), stage.as_str());
    }
  }

  /// 场景：未知字符串无法反序列化为阶段。
  #[test]
  fn from_unknown_returns_none() {
    assert_eq!(ErrorStage::from("mid_stream"), None);
  }
}
