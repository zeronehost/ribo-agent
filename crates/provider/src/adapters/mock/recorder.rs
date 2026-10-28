//! 调用记录：mock 每次被调用的快照。
//!
//! 替代"计数器"式断言——记录按序追加，可断言调用次数、顺序与
//! 请求标签，支撑重试次数与降级路径的测试。

use std::collections::BTreeMap;

use crate::ids::ModelId;

/// 一次 mock 调用的记录。
#[derive(Debug, Clone, PartialEq)]
pub struct MockCall {
  /// 被请求的模型。
  pub model: ModelId,
  /// 调用序号（该 provider 实例内单调递增，从 0 开始）。
  pub sequence: usize,
  /// 请求附带的标签快照（用于关联上层调用）。
  pub labels: BTreeMap<String, String>,
}
