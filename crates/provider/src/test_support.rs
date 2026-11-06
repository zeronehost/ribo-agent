//! 测试辅助工具（仅测试构建启用）。
//!
//! 三家适配器的单元测试共享同一套脚手架：全局 tracing 初始化、
//! 聚合器构造、事件流拉取与固定长度 HTTP 响应拼装。
//! 与具体线格式相关的服务端脚本（`serve` / `serve_partial`）仍留在
//! 各适配器测试模块内。

use std::sync::Once;

use crate::{
  ids::{ModelId, ProviderId},
  stream::{ProviderEvent, ProviderEventStream, StreamAggregator},
};

/// 安装全局 tracing 订阅者（输出丢弃），使日志宏内的字段表达式真正执行。
///
/// `tracing` 宏默认在无订阅者时跳过字段求值；覆盖率统计需要这些行被执行。
/// 多次调用只生效一次，测试可安全并发调用。
pub(crate) fn init() {
  static ONCE: Once = Once::new();
  ONCE.call_once(|| {
    let subscriber = tracing_subscriber::fmt()
      .with_max_level(tracing::Level::TRACE)
      .with_writer(std::io::sink)
      .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);
  });
}

/// 构造测试用聚合器（`provider` / `model` 为合法标识字符串）。
pub(crate) fn aggregator(provider: &str, model: &str) -> StreamAggregator {
  init();
  StreamAggregator::new(
    ProviderId::parse(provider).expect("valid provider id"),
    ModelId::parse(model).expect("valid model id"),
  )
}

/// 拉取全部事件直到流结束。
pub(crate) async fn collect_events(
  stream: &mut ProviderEventStream,
) -> Vec<ProviderEvent> {
  let mut events = Vec::new();
  while let Some(event) = stream.next().await {
    events.push(event);
  }
  events
}

/// 构造固定 `Content-Length` 的 SSE HTTP 响应。
///
/// 固定长度可让客户端读到确定边界，避免依赖 `chunked` 编码。
pub(crate) fn http_response(status_line: &str, body: &str) -> String {
  format!(
    "HTTP/1.1 {status_line}\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
    body.len()
  )
}
