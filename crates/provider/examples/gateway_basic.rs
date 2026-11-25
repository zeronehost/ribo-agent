//! 以网关为统一入口的多提供方流式请求示例。
//!
//! 运行：
//!
//! ```text
//! cargo run -p ra-provider --example gateway_basic
//! ```
//!
//! 演示要点：
//!
//! 1. 组装 [`Gateway`](ra_provider::gateway::Gateway)：注册 mock 提供方、
//!    注入 [`RouteMap`](ra_provider::gateway::RouteMap) 降级路由表、
//!    重复注册被拒绝（`DUPLICATE_PROVIDER`）；
//! 2. 同提供方降级：主模型建流失败（`SERVER_ERROR`）→ 沿
//!    `same_provider` 链切换到同提供方的备选模型；
//! 3. 跨提供方降级：`RATE_LIMITED` 可跨提供方 → 沿 `cross_provider`
//!    链把备选链投到目标提供方目录求值（"主链 ∩ 目标目录"）；
//! 4. 请求未注册模型 → `MODEL_NOT_FOUND`；
//! 5. 熔断：阈值 1 的网关首次失败即打开熔断，第二次请求被
//!    `CIRCUIT_OPEN` 拒绝；
//! 6. 并发准入：`max_in_flight = 1` 时未终结的流占用配额，第二个
//!    请求 `OVERLOADED`，流终结释放配额后恢复；
//! 7. [`ObservedStream`](ra_provider::stream::ObservedStream) 两条消费
//!    路径：`next()` 逐事件与 `result()` 直接取终态。

use std::{sync::Arc, time::Duration};

use ra_core::message::{AssistantMessage, Content, TextContent};
use ra_provider::{
  adapters::mock::{MockProvider, MockStep},
  context::Context,
  error::{RATE_LIMITED, SERVER_ERROR, pre_stream},
  gateway::{
    FallbackMatch, FallbackRule, FallbackTarget, Gateway, GatewayConfig,
    RouteMap,
  },
  ids::{ModelId, ProviderId},
  model::Model,
  options::StreamOptions,
  retry::RetryPolicy,
  stream::{ObservedStream, ProviderEvent},
};

/// 解析示例用的提供方 ID（示例内取值均合法）。
fn pid(raw: &str) -> ProviderId {
  ProviderId::parse(raw).expect("示例提供方 ID 合法")
}

/// 解析示例用的模型 ID。
fn mid(raw: &str) -> ModelId {
  ModelId::parse(raw).expect("示例模型 ID 合法")
}

/// 构造满足网关注册校验的模型（上下文窗口与输出限额非零且合理）。
fn model(provider_raw: &str, id: &str) -> Model {
  Model::new(pid(provider_raw), mid(id), id, "http://mock.local")
    .with_context_window(8192)
    .with_max_tokens(1024)
}

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

/// 成功脚本步骤：一段文本增量（适配器自动补发 `Start` 并以 `Done(Stop)` 收敛）。
fn success_step(text: &str) -> MockStep {
  MockStep::Events(vec![
    ProviderEvent::TextStart {
      index: 0,
      partial: text_partial(text),
    },
    ProviderEvent::TextDelta {
      index: 0,
      delta: text.to_string(),
      partial: text_partial(text),
    },
  ])
}

/// 建流失败脚本步骤（经 `pre_stream` 阶段门构造，保证 stage 为 PreStream）。
fn fail_step(code: &'static str, msg: &str) -> MockStep {
  MockStep::Fail(pre_stream(code, msg))
}

/// 零退避、零重试的网关配置：建流失败立刻进入降级，演示路径不引入等待。
fn base_config() -> GatewayConfig {
  GatewayConfig {
    retry: RetryPolicy {
      initial_backoff: Duration::ZERO,
      max_retries: 0,
      jitter_ratio: 0.,
      ..RetryPolicy::default()
    },
    ..GatewayConfig::default()
  }
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

/// 消费观察流：逐事件打印后取终态消息（两条消费路径共用同一终态）。
async fn consume(mut stream: ObservedStream, tag: &str) {
  while let Some(event) = stream.next().await {
    match event {
      ProviderEvent::Start { partial } => {
        println!(
          "[{tag}] start: provider={} model={}",
          partial.provider, partial.model
        );
      }
      ProviderEvent::TextDelta { delta, .. } => {
        println!("[{tag}] text-delta: {delta:?}");
      }
      ProviderEvent::Done { reason, .. } => {
        println!("[{tag}] done: reason={reason:?}");
      }
      ProviderEvent::Error { code, .. } => {
        println!("[{tag}] error: code={code:?}");
      }
      _ => {}
    }
  }
  let message = stream.result().await.expect("终态消息应可用");
  println!(
    "[{tag}] final: stop_reason={:?} text={:?}",
    message.stop_reason,
    first_text(&message)
  );
}

#[tokio::main]
async fn main() {
  let ctx = Context::default();

  // 主网关组装：两个降级场景共用一个网关
  // alpha：第一次调用（alpha-main）建流失败，第二次（降级候选
  // alpha-backup）成功——mock 脚本按调用顺序逐个弹出。
  let alpha = MockProvider::new(
    pid("alpha"),
    vec![model("alpha", "alpha-main"), model("alpha", "alpha-backup")],
    vec![
      fail_step(SERVER_ERROR, "alpha-main is down"),
      success_step("来自 alpha-backup 的同提供方降级响应"),
    ],
  );
  // beta：beta-model 建流被限流（RATE_LIMITED 可跨提供方降级）。
  let beta = MockProvider::new(
    pid("beta"),
    vec![model("beta", "beta-model")],
    vec![fail_step(RATE_LIMITED, "beta quota exhausted")],
  );
  // gamma：跨提供方降级目标，承载备选模型 gamma-model。
  let gamma = MockProvider::new(
    pid("gamma"),
    vec![model("gamma", "gamma-model")],
    vec![success_step("来自 gamma-model 的跨提供方降级响应")],
  );

  // 降级路由表
  // 隐式同提供方链：alpha-main 失败 → alpha-backup（声明序）；
  // fallback 规则：beta 通道失败 → gamma 通道（{ provider } 形态 =
  // 同名 → 目标默认两级解析，命中 gamma-model）。
  let mut routes = RouteMap::default();
  routes.insert_same_provider(mid("alpha-main"), vec![mid("alpha-backup")]);
  routes.insert_fallback_rules(vec![FallbackRule {
    match_: FallbackMatch {
      provider: pid("beta"),
      model: None, // provider 级通配
    },
    targets: vec![FallbackTarget::Provider(pid("gamma"))],
  }]);

  let mut gateway = Gateway::new(base_config()).with_routes(routes);
  let alpha_arc = Arc::new(alpha);
  gateway
    .register(alpha_arc.clone())
    .expect("alpha 应注册成功");
  gateway.register(Arc::new(beta)).expect("beta 应注册成功");
  gateway.register(Arc::new(gamma)).expect("gamma 应注册成功");

  // 重复注册同一提供方 ID → DUPLICATE_PROVIDER
  let err = gateway.register(alpha_arc).expect_err("重复注册应被拒绝");
  println!("[注册] 重复注册被拒绝：code={}", err.code());

  // 场景 1：同提供方降级（SERVER_ERROR → alpha-backup）
  println!("\n===== 场景 1：同提供方降级（SERVER_ERROR → alpha-backup）=====");
  let stream = gateway
    .stream(&mid("alpha-main"), &ctx, StreamOptions::default())
    .await
    .expect("主候选失败后应降级到 alpha-backup 建流成功");
  consume(stream, "场景1").await;

  // 场景 2：跨提供方降级（RATE_LIMITED → gamma-model）
  println!("\n===== 场景 2：跨提供方降级（RATE_LIMITED → gamma-model）=====");
  let message = gateway
    .stream(&mid("beta-model"), &ctx, StreamOptions::default())
    .await
    .expect("应跨提供方降级到 gamma 建流成功")
    .result()
    .await
    .expect("终态消息应可用");
  println!(
    "[场景2] result(): stop_reason={:?} text={:?}",
    message.stop_reason,
    first_text(&message)
  );

  // 场景 3：请求未注册模型（MODEL_NOT_FOUND）
  println!("\n===== 场景 3：请求未注册模型（MODEL_NOT_FOUND）=====");
  match gateway
    .stream(&mid("no-such-model"), &ctx, StreamOptions::default())
    .await
  {
    Ok(_) => panic!("未注册模型不应建流成功"),
    Err(e) => println!("[场景3] code={} msg={}", e.code(), e.message()),
  }

  // 场景 4：熔断（threshold=1，首次失败即打开）
  println!("\n===== 场景 4：熔断（threshold=1，首次失败即打开）=====");
  let delta = MockProvider::new(
    pid("delta"),
    vec![model("delta", "delta-model")],
    vec![fail_step(SERVER_ERROR, "delta is down")],
  );
  let mut circuit_gateway = Gateway::new(GatewayConfig {
    circuit_threshold: 1,
    ..base_config()
  });
  circuit_gateway
    .register(Arc::new(delta))
    .expect("delta 应注册成功");

  // 第一次：建流失败（SERVER_ERROR 计入熔断，达到阈值 → 打开）；
  // 路由表为空无候选可降级，最后一次错误原样返回。
  let err = circuit_gateway
    .stream(&mid("delta-model"), &ctx, StreamOptions::default())
    .await
    .map(|_| ())
    .expect_err("第一次请求应建流失败");
  println!("[场景4a] code={}", err.code());

  // 第二次：唯一候选被熔断拒绝，链上无其他候选 → CIRCUIT_OPEN。
  let err = circuit_gateway
    .stream(&mid("delta-model"), &ctx, StreamOptions::default())
    .await
    .map(|_| ())
    .expect_err("第二次请求应被熔断拒绝");
  println!("[场景4b] code={}", err.code());

  // 场景 5：并发准入（max_in_flight=1）
  println!("\n===== 场景 5：并发准入（max_in_flight=1）=====");
  let echo = MockProvider::new(
    pid("echo"),
    vec![model("echo", "echo-model")],
    vec![
      success_step("第一个流的内容"),
      success_step("第二个流的内容"),
    ],
  );
  let mut overload_gateway = Gateway::new(GatewayConfig {
    max_in_flight: 1,
    ..base_config()
  });
  overload_gateway
    .register(Arc::new(echo))
    .expect("echo 应注册成功");

  // 第一个请求建流成功；在途配额由流持有至终结。
  let held = overload_gateway
    .stream(&mid("echo-model"), &ctx, StreamOptions::default())
    .await
    .expect("第一个请求应建流成功");

  // 不消费 held，配额未释放 → 第二个请求被 OVERLOADED 拒绝。
  let err = overload_gateway
    .stream(&mid("echo-model"), &ctx, StreamOptions::default())
    .await
    .map(|_| ())
    .expect_err("配额占用期间应被拒绝");
  println!("[场景5a] code={}", err.code());

  // 消费第一个流 → 终结时释放配额。
  consume(held, "场景5").await;

  // 配额已归还 → 再次建流成功（直接走 result() 取终态）。
  let message = overload_gateway
    .stream(&mid("echo-model"), &ctx, StreamOptions::default())
    .await
    .expect("配额释放后应建流成功")
    .result()
    .await
    .expect("终态消息应可用");
  println!("[场景5b] 恢复：stop_reason={:?}", message.stop_reason);
}
