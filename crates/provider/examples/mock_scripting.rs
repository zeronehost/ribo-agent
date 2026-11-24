//! 脚本化步骤与调用记录示例（mock 适配器）。
//!
//! 运行：
//!
//! ```text
//! cargo run -p ra-provider --example mock_scripting
//! ```
//!
//! 依次演示五种 [`MockStep`](ra_provider::adapters::mock::MockStep)
//! 的行为差异与调用记录断言：
//!
//! | 步骤 | 行为 |
//! | --- | --- |
//! | `Complete` | 同步推完 `Start` + `Done`（不经后台任务） |
//! | `Events` | 后台回放事件，以 `Done(Stop)` 收敛 |
//! | `StreamFail` | 回放后以脚本化错误终结（错误码经事件路径传递） |
//! | `Fail` | 建流阶段失败（`stream()` 直接返回 `Err`） |
//! | `Silent` | 静默指定时长；期间取消立即以 `Aborted` 终结 |
//!
//! 收尾用 [`MockProvider::record`](ra_provider::adapters::mock::MockProvider::record)
//! 打印调用记录——mock 对调用方完全透明，针对它写的消费端代码无须
//! 修改即可直接用于真实适配器。

use std::{sync::Arc, time::Duration};

use ra_core::message::{AssistantMessage, Content, StopReason, TextContent};
use ra_provider::{
  adapters::mock::{MockProvider, MockStep},
  context::Context,
  error::{SERVER_ERROR, pre_stream},
  ids::{ModelId, ProviderId},
  model::Model,
  options::{CancelToken, ProviderOptions, StreamOptions},
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

#[tokio::main]
async fn main() {
  // ===== 组装脚本 =====
  // 注意 Fail 步骤的错误经 pre_stream 阶段门构造（stage = PreStream，
  // 契约层 ：建流阶段失败只能是 Err）。
  let steps = vec![
    MockStep::Complete(Arc::new(|_ctx| {
      (
        StopReason::Stop,
        AssistantMessage::terminal("x", "y", StopReason::Stop, "", 0),
      )
    })),
    MockStep::Events(vec![ProviderEvent::TextDelta {
      index: 0,
      delta: "事件回放".to_string(),
      partial: text_partial("事件回放"),
    }]),
    MockStep::StreamFail {
      events: vec![ProviderEvent::TextDelta {
        index: 0,
        delta: "回放后失败".to_string(),
        partial: text_partial("回放后失败"),
      }],
      reason: StopReason::Error,
      code: Some(SERVER_ERROR),
    },
    MockStep::Fail(pre_stream(SERVER_ERROR, "quota exhausted (mocked)")),
    MockStep::Silent {
      duration: Duration::from_secs(1),
    },
  ];

  let provider_id = ProviderId::parse("mock").expect("valid provider id");
  let model = Model::new(
    provider_id.clone(),
    ModelId::parse("mock-model").expect("valid model id"),
    "Mock Model",
    "http://localhost",
  );
  let mock = MockProvider::new(provider_id, vec![model.clone()], steps);
  let ctx = Context::default();

  // ===== 1. Complete：同步完成 =====
  {
    println!("--- Complete ---");
    let options = ProviderOptions {
      model: &model,
      context: &ctx,
      options: &StreamOptions::default(),
    };
    let message = mock
      .stream(options)
      .await
      .expect("建流成功")
      .result()
      .await
      .expect("Complete 正常收敛");
    println!(
      "归属被改写为 mock：provider={} model={}",
      message.provider, message.model
    );
  }

  // ===== 2. Events：后台回放，Done(Stop) 收敛 =====
  {
    println!("--- Events ---");
    let options = ProviderOptions {
      model: &model,
      context: &ctx,
      options: &StreamOptions::default(),
    };
    let mut stream = mock.stream(options).await.expect("建流成功");
    while let Some(event) = stream.next().await {
      if let ProviderEvent::TextDelta { delta, .. } = event {
        println!("[text-delta] +{delta:?}");
      }
    }
  }

  // ===== 3. StreamFail：错误码经事件路径传递 =====
  {
    println!("--- StreamFail ---");
    let options = ProviderOptions {
      model: &model,
      context: &ctx,
      options: &StreamOptions::default(),
    };
    let mut stream = mock.stream(options).await.expect("建流成功");
    while let Some(event) = stream.next().await {
      if let ProviderEvent::Error { reason, code, .. } = event {
        println!("终止 reason={reason:?} code={code:?}");
        break;
      }
    }
  }

  // ===== 4. Fail：建流阶段直接失败 =====
  {
    println!("--- Fail ---");
    let options = ProviderOptions {
      model: &model,
      context: &ctx,
      options: &StreamOptions::default(),
    };
    // 建流 Promise 失败：不产生事件流，直接 Err（PreStream 通道）
    match mock.stream(options).await {
      Err(err) => {
        println!("建流失败 code={}（经 pre_stream 阶段门构造）", err.code())
      }
      Ok(_) => println!("不应到达"),
    }
  }

  // ===== 5. Silent：静默期间取消立即终结 =====
  {
    println!("--- Silent + cancel ---");
    let mut stream_options = StreamOptions::default();
    let cancel = CancelToken::new();
    stream_options.cancel = Some(cancel.clone());
    let options = ProviderOptions {
      model: &model,
      context: &ctx,
      options: &stream_options,
    };
    let mut stream = mock.stream(options).await.expect("建流成功");
    // Start 先行（建流 Promise 不等待首内容事件），随后静默 1 秒；
    // 取消令牌在静默期内触发 → Aborted + CANCELLED
    cancel.cancel();
    while let Some(event) = stream.next().await {
      if let ProviderEvent::Error { reason, code, .. } = event {
        println!("取消生效 reason={reason:?} code={code:?}");
        break;
      }
    }
  }

  // ===== 6. 脚本耗尽：可预测的 INVALID_REQUEST 错误终结 =====
  {
    println!("--- 耗尽 ---");
    let options = ProviderOptions {
      model: &model,
      context: &ctx,
      options: &StreamOptions::default(),
    };
    let mut stream = mock.stream(options).await.expect("耗尽不阻断建流");
    while let Some(event) = stream.next().await {
      if let ProviderEvent::Error { code, .. } = event {
        println!("脚本耗尽 code={code:?}");
        break;
      }
    }
  }

  // ===== 调用记录：断言重试次数与降级路径的依据 =====
  println!("--- record ---");
  for call in mock.record() {
    println!(
      "调用 #{}：model={} labels={:?}",
      call.sequence, call.model, call.labels
    );
  }
}
