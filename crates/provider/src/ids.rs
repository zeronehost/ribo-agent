//! 强类型标识模块：避免裸字符串在"provider id / model id"之间被混用。
//!
//! 两个 newtype 都禁止 `From<String>`，强制走 [`ProviderId::parse`] /
//! [`ModelId::parse`] 显式校验——把"标识合法性"从运行期假设变成编译前检查。

use std::sync::Arc;

use ra_core::error::RiboResult;

use crate::error::{INVALID_REQUEST, pre_stream};

/// 提供方标识（小写 ASCII，例如 `openai`、`anthropic`）。
///
/// 不变量：非空、仅含 `[a-z0-9_-]`。构造失败返回
/// [`crate::error::pre_stream`] 构造的 [`INVALID_REQUEST`] 错误。
///
/// 收窄字符集的原因：该标识会直接进入配置键、指标标签与日志字段，
/// 限制字符集可避免转义与大小写归一化的歧义（例如 `OpenAI` 与 `openai`
/// 是否算同一个 provider）。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProviderId(Arc<str>);

impl ProviderId {
  /// 校验并构造。
  ///
  /// # Errors
  ///
  /// 空串或含非法字符时返回
  /// [`pre_stream(INVALID_REQUEST, ..)`](crate::error::pre_stream)。
  ///
  /// # 实现说明
  ///
  /// 大写字母同样视为非法——大小写不归一化，强制调用方显式提供小写形式，
  /// 避免"同一 provider 注册出两个键"。
  pub fn parse(raw: &str) -> RiboResult<Self> {
    // 非空检查
    // 空标识无法作为注册表键，也无法出现在日志字段中定位问题。
    if raw.is_empty() {
      return Err(pre_stream(INVALID_REQUEST, "provider id must not be empty"));
    }

    // 字符集检查
    // 首个非法字符即短路返回，消息中带出该字符与原始输入以利排查。
    if let Some(ch) = raw
      .chars()
      .find(|c| !matches!(c, 'a'..='z' | '0'..='9' | '_' | '-'))
    {
      return Err(pre_stream(
        INVALID_REQUEST,
        format!("provider id contains invalid character {ch:?}: {raw:?}"),
      ));
    }

    // 构造
    // 校验通过后一次性转入 `Arc<str>`，后续克隆只增引用计数。
    Ok(Self(Arc::from(raw)))
  }
}

/// 模型标识（由提供方定义，例如 `gpt-4o`、`claude-sonnet-4`）。
///
/// 不变量：非空且不得仅由空白组成。不同 provider 的同一 `ModelId`
/// 不保证是同一模型——模型是否存在由 provider 的 `supports()` 判定，
/// 不在此处校验。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModelId(Arc<str>);

impl ModelId {
  /// 校验并构造。
  ///
  /// # Errors
  ///
  /// 空串或仅含空白时返回
  /// [`pre_stream(INVALID_REQUEST, ..)`](crate::error::pre_stream)。
  ///
  /// # 实现说明
  ///
  /// 只校验非空：模型名由各提供方自行定义（含点、斜杠、冒号等），
  /// 强行收窄字符集会误杀合法模型。
  pub fn parse(raw: &str) -> RiboResult<Self> {
    if raw.trim().is_empty() {
      return Err(pre_stream(INVALID_REQUEST, "model id must not be empty"));
    }

    Ok(Self(Arc::from(raw)))
  }
}

/// 为两个 newtype 批量补齐通用实现：
///
/// - [`new`](Self::new) 内部构造器（已通过 parse 校验的场景可绕过，通常由 `parse` 或测试使用）
/// - [`as_str`](Self::as_str) 取原始字符串
/// - `Display` / `AsRef<str>`：便于日志 `%provider` 字段与错误消息拼接
/// - `Serialize` / `Deserialize`：按原始字符串序列化
///
/// 宏展开前：`ProviderId` / `ModelId` 各有自己的校验逻辑（`parse`），
/// 宏只负责公共样板，两者互不干扰。
macro_rules! impl_id {
  ($name: ident) => {
    impl $name {
      /// 内部构造器：跳过校验，仅供已通过 `parse` 或反序列化的场景使用。
      fn new(id: impl Into<Arc<str>>) -> Self {
        Self(id.into())
      }
      /// 取原始字符串。
      pub fn as_str(&self) -> &str {
        &self.0
      }
    }
    // `Display` 用于日志字段（`%provider`）与错误消息拼接，输出与原始字符串一致。
    impl core::fmt::Display for $name {
      fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
      }
    }
    // `AsRef<str>` 便于泛型 API 以 `&str` 形式接收该标识，无需显式 `as_str`。
    impl AsRef<str> for $name {
      fn as_ref(&self) -> &str {
        &self.0
      }
    }
    // 序列化为原始字符串，保证与 `parse` 的输入形式一致。
    impl serde::Serialize for $name {
      fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
      where
        S: serde::Serializer,
      {
        serializer.serialize_str(&self.0)
      }
    }
    // 反序列化：先按字符串读取，再走内部构造器。
    //
    // 此处不重复 `parse` 的字符集/非空校验；
    // 需要强校验的入口应使用 `parse`。
    impl<'de> serde::Deserialize<'de> for $name {
      fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
      where
        D: serde::Deserializer<'de>,
      {
        // 读取字符串
        // 非字符串输入（数字、null 等）在此处直接返回反序列化错误。
        let s = String::deserialize(deserializer)?;
        // 包装为强类型标识
        Ok(Self::new(s))
      }
    }
  };
}

impl_id!(ProviderId);
impl_id!(ModelId);

/// 模型引用：定位一个模型所需的 `(提供方 ID, 模型 ID)` 二元组。
pub type ModelRef = (ProviderId, ModelId);

#[cfg(test)]
mod tests {
  use super::*;

  /// 场景：合法 provider id 可通过多种访问方式读回原值。
  #[test]
  fn provider_id_roundtrips_through_accessors() {
    let id = ProviderId::parse("openai-1_x").expect("合法标识应通过");

    assert_eq!(id.as_str(), "openai-1_x");
    assert_eq!(id.to_string(), "openai-1_x");
    assert_eq!(id.as_ref(), "openai-1_x");
  }

  /// 场景：空串与大写/空格/点等非法字符均被拒绝为 INVALID_REQUEST。
  #[test]
  fn provider_id_rejects_invalid_input() {
    for raw in ["", "OpenAI", "open ai", "open.ai"] {
      let err = ProviderId::parse(raw).expect_err("非法标识应被拒绝");
      assert_eq!(err.code(), INVALID_REQUEST, "输入: {raw:?}");
    }
  }

  /// 场景：模型名由提供方定义，允许点、斜杠、冒号等字符。
  #[test]
  fn model_id_accepts_provider_defined_charset() {
    let id = ModelId::parse("openai/gpt-4o:latest").expect("合法模型名应通过");

    assert_eq!(id.as_str(), "openai/gpt-4o:latest");
    assert_eq!(id.to_string(), "openai/gpt-4o:latest");
    assert_eq!(id.as_ref(), "openai/gpt-4o:latest");
  }

  /// 场景：空串与纯空白模型名被拒绝。
  #[test]
  fn model_id_rejects_blank_input() {
    for raw in ["", "   ", "\t\n"] {
      let err = ModelId::parse(raw).expect_err("空白模型名应被拒绝");
      assert_eq!(err.code(), INVALID_REQUEST, "输入: {raw:?}");
    }
  }

  /// 场景：两个 newtype 的 serde 表示为原始字符串，可往返转换。
  #[test]
  fn ids_serialize_as_plain_strings() {
    let provider = ProviderId::parse("p1").expect("合法标识");
    assert_eq!(
      serde_json::to_value(&provider).expect("序列化应成功"),
      serde_json::json!("p1")
    );
    let provider: ProviderId =
      serde_json::from_value(serde_json::json!("p2")).expect("反序列化应成功");
    assert_eq!(provider.as_str(), "p2");

    let model = ModelId::parse("gpt-4o").expect("合法标识");
    assert_eq!(
      serde_json::to_value(&model).expect("序列化应成功"),
      serde_json::json!("gpt-4o")
    );
    let model: ModelId = serde_json::from_value(serde_json::json!("claude"))
      .expect("反序列化应成功");
    assert_eq!(model.as_str(), "claude");
  }

  /// 场景：反序列化时输入不是字符串。
  ///
  /// 预期：`String::deserialize` 返回错误并被 `?` 传播。
  #[test]
  fn ids_deserialize_reject_non_string() {
    assert!(serde_json::from_str::<ProviderId>("123").is_err());
    assert!(serde_json::from_str::<ModelId>("null").is_err());
  }
}
