//! serde 属性辅助函数。

/// 供 `#[serde(skip_serializing_if = "...")]` 使用：值为 `false` 时跳过该字段。
///
/// serde 的 `skip_serializing_if` 只能接受函数路径、无法直接内联闭包或取反，
/// 因此用该函数表达「默认值 `false` 不序列化、为 `true` 时才输出」的语义。
///
/// # 参数
///
/// - `b`：被检查的布尔字段引用。
///
/// # 返回
///
/// 返回 `true` 表示应跳过序列化，即入参为 `false`。
pub(crate) fn is_false(b: &bool) -> bool {
  !*b
}
