//! 最小 provider 流式消费示例（mock 适配器）。
//!
//! 运行：
//!
//! ```text
//! cargo run -p ra-provider --example mock_basic
//! ```
//!
//! 演示要点：
//!
//! 1. 组装 [`MockProvider`](ra_provider::adapters::mock::MockProvider)
//!    （模型目录 + 脚本化步骤）；
//! 2. 构造 [`ProviderOptions`](ra_provider::options::ProviderOptions)
//!    并建流——建流 Promise：返回即通道就绪，不等待首内容事件；
//! 3. `next()` 逐事件消费：`Start` → `TextStart` → `TextDelta*` → `Done`；
//! 4. 每个事件都携带截至当前的半成品快照（`partial`），消费方无须
//!    自行聚合增量；
//! 5. `result()` 与逐事件路径共享同一终态（oneshot 通道独立兑现）。

use std::sync::Arc;

use ra_core::message::{AssistantMessage, Content, TextContent};
use ra_provider::{
  adapters::mock::{MockProvider, MockStep},
  context::Context,
  ids::{ModelId, ProviderId},
  model::Model,
  options::{ProviderOptions, StreamOptions},
  stream::ProviderEvent,
  traits::Provider,
};

/// 构造携带单段文本的半成品快照（模拟模型已输出 `text`）。
fn text_partial(text: &str) -> Arc<AssistantMessage> {
  Arc::new(AssistantMessage {
    content: vec![Content::Text(TextContent {
      content: text.to_string(),
      signature: None,
    })],
    ..AssistantMessage::empty("mock", "mock-model", 0)
  })
}

/// 取消息中的第一段文本（示例辅助）。
fn first_text(message: &AssistantMessage) -> String {
  message
    .content
    .iter()
    .find_map(|c| match c {
      Content::Text(t) => Some(t.content.clone()),
      _ => None,
    })
    .unwrap_or_default()
}

#[tokio::main]
async fn main() {
  // 组装 mock provider
  // 脚本：回放 "你好，" + "Rust！" 两段增量；适配器自动补发 Start，
  // 并以 Done(Stop) 收敛（最终消息取最后一个事件的 partial）。
  let provider_id = ProviderId::parse("mock").expect("valid provider id");
  let model = Model::new(
    provider_id.clone(),
    ModelId::parse("mock-model").expect("valid model id"),
    "Mock Model",
    "http://localhost",
  );
  let script = vec![MockStep::Events(vec![
    ProviderEvent::TextStart {
      index: 0,
      partial: text_partial("你好，"),
    },
    ProviderEvent::TextDelta {
      index: 0,
      delta: "你好，".to_string(),
      partial: text_partial("你好，"),
    },
    ProviderEvent::TextDelta {
      index: 0,
      delta: "Rust！".to_string(),
      partial: text_partial("你好，Rust！"),
    },
  ])];
  let mock = MockProvider::new(provider_id, vec![model.clone()], script);

  // 建流
  let stream_options = StreamOptions::default();
  let ctx = Context::default();
  let options = ProviderOptions {
    model: &model,
    context: &ctx,
    options: &stream_options,
  };
  let mut stream = mock.stream(options).await.expect("建流成功");

  // 逐事件消费
  while let Some(event) = stream.next().await {
    match event {
      ProviderEvent::Start { partial } => {
        println!(
          "[start] provider={} model={}",
          partial.provider, partial.model
        );
      }
      ProviderEvent::TextStart { index, .. } => {
        println!("[text-start] index={index}");
      }
      ProviderEvent::TextDelta { delta, partial, .. } => {
        // partial 是截至当前的聚合快照；这里取文本总长示意
        let total: usize = partial
          .content
          .iter()
          .map(|c| match c {
            Content::Text(t) => t.content.len(),
            _ => 0,
          })
          .sum();
        println!("[text-delta] +{delta:?}（快照累计 {total} 字节）");
      }
      ProviderEvent::Done { reason, message } => {
        println!("[done] reason={reason:?}");
        println!("[done] 最终消息文本：{}", first_text(&message));
        break;
      }
      ProviderEvent::Error { message, .. } => {
        println!(
          "[error] {}",
          message.error_message.as_deref().unwrap_or("(no message)")
        );
        break;
      }
      _ => {}
    }
  }

  // result()：与逐事件路径共享同一终态
  // Done 事件已被上面的循环读走，但 oneshot 终态通道独立兑现，
  // result() 仍能取到最终消息。
  let message = stream.result().await.expect("终态消息");
  println!("[result] stop_reason={:?}", message.stop_reason);
}
