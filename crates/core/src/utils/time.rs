//! 时间相关辅助函数。

/// 返回当前 UTC 时间的毫秒级 Unix 时间戳。
///
/// 典型用途是为消息与事件打时间戳（例如助手消息的 `timestamp` 字段）。
///
/// # Examples
///
/// ```ignore
/// // utils 模块仅对 crate 内部可见，doctest 无法通过公开路径访问，
/// // 此示例仅示意调用方式：
/// let ts = crate::utils::time::now_ms();
/// assert!(ts > 0);
/// ```
pub fn now_ms() -> i64 {
  jiff::Timestamp::now().as_millisecond()
}
