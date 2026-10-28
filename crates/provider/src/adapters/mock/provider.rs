//! 回放式 mock provider。
//!
//! 职责：按脚本步骤驱动 [`ProviderEventStreamProducer`]，把预置事件
//! 折叠为与真实适配器完全同构的事件流。
//! `Start` 自动补发、终结恰好一次、`push` 返回 `false` 时静默退出。

use std::{
  collections::VecDeque,
  sync::{Arc, Mutex},
  time::Duration,
};

use ra_core::{
  error::RiboResult,
  message::{AssistantMessage, StopReason},
  utils::time::now_ms,
};

use crate::{
  adapters::mock::{MockCall, MockStep},
  error::{CANCELLED, INVALID_REQUEST},
  ids::{ModelId, ProviderId},
  model::Model,
  options::{CancelToken, ProviderOptions},
  stream::{ProviderEvent, ProviderEventStream, ProviderEventStreamProducer},
  traits::Provider,
};

/// 回放式 mock provider。
///
/// 额外能力：
/// - [`record`](Self::record) 返回调用记录（模型、序号、标签快照），
///   用于断言重试次数与降级路径；
/// - [`MockStep::Silent`] 步骤与 `start_paused` 配合可确定性测试
///   TTFT 守卫；
/// - 限速配置（按事件间隔）用于构造"合法但慢"的流，验证看门狗
///   不误杀。
///
/// # 示例
///
/// ```rust
/// use ra_provider::adapters::mock::{MockProvider, MockStep};
/// use ra_provider::ids::{ModelId, ProviderId};
/// use ra_provider::model::Model;
/// use ra_provider::traits::Provider;
///
/// let provider = ProviderId::parse("mock").expect("valid provider id");
/// let model = Model::new(
///   provider.clone(),
///   ModelId::parse("mock-model").expect("valid model id"),
///   "Mock Model",
///   "http://localhost",
/// );
/// let mock = MockProvider::new(provider, vec![model], Vec::new());
/// assert_eq!(mock.id().as_str(), "mock");
/// ```
pub struct MockProvider {
  /// 提供方标识。
  id: ProviderId,
  /// 对外声明的模型列表。
  models: Vec<Model>,
  /// 步骤队列（FIFO，跨请求共享；耗尽后以错误终结）。
  steps: Mutex<VecDeque<MockStep>>,
  /// 调用记录（追加式，供 `record()` 断言）。
  calls: Mutex<Vec<MockCall>>,
  /// 相邻事件之间的间隔（`None` 表示不引入真实延迟，仅让出执行权）。
  event_interval: Option<Duration>,
}

impl MockProvider {
  /// 构造 mock provider（默认不限速）。
  ///
  /// # 参数
  ///
  /// - `id`：提供方标识（如 `mock`）。
  /// - `models`：对外声明的模型目录（`supports` 按此线性查找）。
  /// - `steps`：按调用顺序消费的行为脚本；每次 `stream` 弹出一个。
  pub fn new(id: ProviderId, models: Vec<Model>, steps: Vec<MockStep>) -> Self {
    Self {
      id,
      models,
      steps: Mutex::new(steps.into()),
      calls: Mutex::new(Vec::new()),
      event_interval: None,
    }
  }

  /// 设置相邻事件间隔（模拟"合法但慢"的流）。
  pub fn with_event_interval(mut self, interval: Duration) -> Self {
    self.event_interval = Some(interval);
    self
  }

  /// 取全部调用记录的快照（供测试断言）。
  pub fn record(&self) -> Vec<MockCall> {
    self.calls.lock().unwrap().clone()
  }
}

#[async_trait::async_trait]
impl Provider for MockProvider {
  fn id(&self) -> &ProviderId {
    &self.id
  }

  fn models(&self) -> &[Model] {
    &self.models
  }

  async fn stream(
    &self,
    options: ProviderOptions<'_>,
  ) -> RiboResult<ProviderEventStream> {
    // ===== 阶段 1：登记调用 + 弹出步骤 =====
    // 先记录后弹出：即使脚本耗尽，调用记录也必须完整（断言重试次数依赖它）。
    // 序号先于 push 取出：push 会可变借用 calls，参数内再取 len 会冲突。
    {
      let mut calls = self.calls.lock().unwrap();
      let sequence = calls.len();
      calls.push(MockCall {
        model: options.model.id.clone(),
        sequence,
        labels: options.options.labels.clone(),
      });
    }
    let step = self.steps.lock().unwrap().pop_front();

    // ===== 阶段 2：建流并按步骤类型分派 =====
    let (producer, stream) = ProviderEventStreamProducer::build();
    match step {
      // 建流失败：直接返回 Err（PreStream 通道）
      Some(MockStep::Fail(err)) => return Err(err),
      // 脚本耗尽：Start + 错误终结（不 panic、不挂起，保持可预测；
      // 错误码取 INVALID_REQUEST 表示用法错误）
      None => {
        tracing::warn!(provider = %self.id, "mock script exhausted");
        let partial = Arc::new(AssistantMessage::empty(
          self.id.as_str(),
          options.model.id.as_str(),
          now_ms(),
        ));
        let message = AssistantMessage::terminal(
          self.id.as_str(),
          options.model.id.as_str(),
          StopReason::Error,
          "no more mock responses queued",
          now_ms(),
        );
        producer.push(ProviderEvent::Start { partial });
        producer.error(StopReason::Error, message, Some(INVALID_REQUEST));
      }
      // 同步完成：不经过后台任务，建流前推完 Start + Done
      Some(MockStep::Complete(build)) => {
        let (reason, mut message) = build(options.context);
        // 改写归属与时间戳：mock 对调用方完全透明
        message.provider = self.id.as_str().to_string();
        message.model = options.model.id.as_str().to_string();
        message.timestamp = now_ms();
        let partial = Arc::new(AssistantMessage::empty(
          self.id.as_str(),
          options.model.id.as_str(),
          message.timestamp,
        ));
        producer.push(ProviderEvent::Start { partial });
        producer.done(reason, message);
      }
      // 事件回放：整体移交后台任务，Start 自动补发 + 限速 + 取消监听
      Some(MockStep::Events(events)) => {
        spawn_mock_replay(
          producer,
          events,
          None,
          self.event_interval,
          options.options.cancel.clone().unwrap_or_default(),
          self.id.clone(),
          options.model.id.clone(),
        );
      }
      // 事件回放 + 脚本化失败终结
      Some(MockStep::StreamFail {
        events,
        reason,
        code,
      }) => {
        spawn_mock_replay(
          producer,
          events,
          Some((reason, code)),
          self.event_interval,
          options.options.cancel.clone().unwrap_or_default(),
          self.id.clone(),
          options.model.id.clone(),
        );
      }
      // 静默：用于 TTFT 守卫与看门狗测试
      Some(MockStep::Silent { duration }) => {
        spawn_mock_silent(
          producer,
          duration,
          options.options.cancel.clone().unwrap_or_default(),
          self.id.clone(),
          options.model.id.clone(),
        );
      }
    }
    Ok(stream)
  }
}

/// 后台回放事件序列：`Start` 自动补发 → 逐事件限速推送 → 按脚本终结。
///
/// # 参数
///
/// - `terminal`：`None` 表示以 `Done(Stop)` 收尾；`Some((reason, code))`
///   表示以 `producer.error(reason, .., code)` 收尾（`StreamFail` 步骤）。
fn spawn_mock_replay(
  producer: ProviderEventStreamProducer,
  events: Vec<ProviderEvent>,
  terminal: Option<(StopReason, Option<&'static str>)>,
  interval: Option<Duration>,
  cancel: CancelToken,
  provider: ProviderId,
  model: ModelId,
) {
  tokio::spawn(async move {
    // producer 以 Option 持有：done/error 消费 self，每条路径先 take
    let mut producer = Some(producer);

    // ===== 阶段 1：Start 自动补发 =====
    // 归属与时间戳在此统一注入，脚本步骤不携带 Start。
    let partial = Arc::new(AssistantMessage::empty(
      provider.as_str(),
      model.as_str(),
      now_ms(),
    ));
    if let Some(p) = producer.as_ref() {
      p.push(ProviderEvent::Start { partial });
    }

    // ===== 阶段 2：逐事件回放（限速 + 取消监听）=====
    // 记录最后一个内容事件的 partial：Events 步骤的 Done 最终消息由它收敛。
    let mut last_partial: Option<AssistantMessage> = None;
    for event in events {
      // 每个事件前等待一个间隔；期间取消立即生效
      if tick_or_cancel(interval, &cancel).await {
        tracing::debug!(provider = %provider, "mock replay cancelled");
        let message = AssistantMessage::terminal(
          provider.as_str(),
          model.as_str(),
          StopReason::Aborted,
          "cancelled while replaying mock events",
          now_ms(),
        );
        if let Some(p) = producer.take() {
          p.error(StopReason::Aborted, message, Some(CANCELLED));
        }
        return;
      }
      // 终结路径 take 后立即 return，因此此处 producer 必然存在。
      let p = producer
        .as_ref()
        .expect("producer is only taken on terminal paths");
      if let Some(partial) = event.partial() {
        last_partial = Some((**partial).clone());
      }
      // push 返回 false = 消费端已放弃：静默退出，不再终结
      if !p.push(event) {
        return;
      }
    }

    // ===== 阶段 3：收尾终结 =====
    match terminal {
      // StreamFail：以脚本指定的原因与错误码终结
      // （StopReason 不实现 Copy：message 携带克隆，原值留给 error）
      Some((reason, code)) => {
        let message = AssistantMessage::terminal(
          provider.as_str(),
          model.as_str(),
          reason.clone(),
          "mock stream failed as scripted",
          now_ms(),
        );
        if let Some(p) = producer.take() {
          p.error(reason, message, code);
        }
      }
      // Events：以 Done(Stop) 收敛，最终消息取最后一个 partial
      None => {
        let mut message = last_partial.unwrap_or_else(|| {
          AssistantMessage::empty(provider.as_str(), model.as_str(), now_ms())
        });
        message.stop_reason = StopReason::Stop;
        if let Some(p) = producer.take() {
          p.done(StopReason::Stop, message);
        }
      }
    }
  });
}

/// 后台静默：`Start` 后保持沉默 `duration`，随后按取消与否终结。
///
/// 配合 `tokio::time::pause`（`start_paused`）可确定性测试 TTFT 守卫：
/// 推进虚拟时钟即可精确触发"首内容事件超时"分支。
fn spawn_mock_silent(
  producer: ProviderEventStreamProducer,
  duration: Duration,
  cancel: CancelToken,
  provider: ProviderId,
  model: ModelId,
) {
  tokio::spawn(async move {
    let mut producer = Some(producer);

    // Start 先行：建流 Promise 不等待首内容事件
    let partial = Arc::new(AssistantMessage::empty(
      provider.as_str(),
      model.as_str(),
      now_ms(),
    ));
    if let Some(p) = producer.as_ref() {
      p.push(ProviderEvent::Start { partial });
    }

    // ===== 静默等待：取消与到期竞争 =====
    tokio::select! {
      biased;
      _ = cancel.cancelled() => {
        // 取消 → Aborted 终结（不计入上游故障）
        tracing::debug!(provider = %provider, "silent mock cancelled");
        let message = AssistantMessage::terminal(
          provider.as_str(),
          model.as_str(),
          StopReason::Aborted,
          "cancelled during silent mock",
          now_ms(),
        );
        if let Some(p) = producer.take() {
          p.error(StopReason::Aborted, message, Some(CANCELLED));
        }
      }
      _ = tokio::time::sleep(duration) => {
        // 静默期满：以 Stop 收敛空消息（合法但无内容）；
        // empty() 的 stop_reason 是 Pending，须与 Done 的 reason 对齐
        let mut message =
          AssistantMessage::empty(provider.as_str(), model.as_str(), now_ms());
        message.stop_reason = StopReason::Stop;
        if let Some(p) = producer.take() {
          p.done(StopReason::Stop, message);
        }
      }
    }
  });
}

/// 等待一个事件间隔或取消信号；返回 `true` 表示已被取消。
///
/// `interval` 为 `None` 时仅让出执行权（不引入真实延迟），
/// 同时仍监听取消——保证零延迟回放也能即时中止。
async fn tick_or_cancel(
  interval: Option<Duration>,
  cancel: &CancelToken,
) -> bool {
  tokio::select! {
    biased;
    _ = cancel.cancelled() => true,
    _ = async {
      match interval {
        Some(d) => tokio::time::sleep(d).await,
        None => tokio::task::yield_now().await,
      }
    } => false,
  }
}

#[cfg(test)]
mod tests {
  use std::{sync::Arc, time::Duration};

  use ra_core::message::{Content, StopReason, TextContent};

  use super::*;
  use crate::{
    context::Context,
    error::{SERVER_ERROR, pre_stream},
    options::StreamOptions,
  };

  /// 构造带模型目录的 mock provider（日志订阅由 test_support 初始化）。
  fn mock_provider(steps: Vec<MockStep>) -> (MockProvider, Model) {
    crate::test_support::init();
    let provider = ProviderId::parse("mock").expect("valid provider id");
    let model = Model::new(
      provider.clone(),
      ModelId::parse("mock-model").expect("valid model id"),
      "Mock Model",
      "http://localhost",
    );
    (
      MockProvider::new(provider, vec![model.clone()], steps),
      model,
    )
  }

  /// 构造携带单段文本的半成品快照（模拟模型已输出内容）。
  fn text_partial(text: &str) -> Arc<AssistantMessage> {
    Arc::new(AssistantMessage {
      content: vec![Content::Text(TextContent {
        content: text.to_string(),
        signature: None,
      })],
      ..AssistantMessage::empty("mock", "mock-model", 0)
    })
  }

  /// 场景：Fail 步骤走建流阶段错误通道（契约层 P1）。
  ///
  /// 预期：`stream` 返回 Err，错误码保持脚本原样。
  #[tokio::test]
  async fn fail_step_returns_pre_stream_error() {
    let (provider, model) =
      mock_provider(vec![MockStep::Fail(pre_stream(SERVER_ERROR, "boom"))]);
    let stream_options = StreamOptions::default();
    let ctx = Context::default();
    // expect_err 要求 Ok 侧实现 Debug；ProviderEventStream 未实现，
    // 丢弃 Ok 侧仅保留错误语义
    let err = provider
      .stream(ProviderOptions {
        model: &model,
        context: &ctx,
        options: &stream_options,
      })
      .await
      .map(|_| ())
      .expect_err("Fail 步骤应导致建流失败");
    assert_eq!(err.code(), SERVER_ERROR);
  }

  /// 场景：脚本耗尽时建流仍成功，随后以 INVALID_REQUEST 错误终结。
  ///
  /// 预期：首个事件是自动补发的 Start，第二个事件是
  /// `Error(StopReason::Error, Some(INVALID_REQUEST))`。
  #[tokio::test]
  async fn exhausted_script_yields_start_then_invalid_request() {
    let (provider, model) = mock_provider(Vec::new());
    let stream_options = StreamOptions::default();
    let ctx = Context::default();
    let mut stream = provider
      .stream(ProviderOptions {
        model: &model,
        context: &ctx,
        options: &stream_options,
      })
      .await
      .expect("脚本耗尽不应阻断建流");

    assert!(
      matches!(stream.next().await, Some(ProviderEvent::Start { .. })),
      "首个事件应为自动补发的 Start"
    );
    match stream.next().await {
      Some(ProviderEvent::Error { reason, code, .. }) => {
        assert_eq!(reason, StopReason::Error);
        assert_eq!(code, Some(INVALID_REQUEST));
      }
      other => panic!("预期 Error 事件，实际 {other:?}"),
    }
  }

  /// 场景：Complete 步骤同步推完 Start + Done，不经过后台任务。
  ///
  /// 预期：result() 返回脚本生成的消息，且归属与时间戳被改写为 mock。
  #[tokio::test]
  async fn complete_step_pushes_start_and_done_synchronously() {
    // 闭包刻意返回错误归属（x/y），验证适配器会改写为 mock 的真实归属
    let build = |_ctx: &Context| {
      (
        StopReason::Stop,
        AssistantMessage::terminal("x", "y", StopReason::Stop, "", 0),
      )
    };
    let (provider, model) =
      mock_provider(vec![MockStep::Complete(Arc::new(build))]);
    let stream_options = StreamOptions::default();
    let ctx = Context::default();
    let message = provider
      .stream(ProviderOptions {
        model: &model,
        context: &ctx,
        options: &stream_options,
      })
      .await
      .expect("Complete 步骤建流成功")
      .result()
      .await
      .expect("Complete 步骤应正常收敛");

    assert_eq!(message.provider, "mock");
    assert_eq!(message.model, "mock-model");
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert!(message.timestamp > 0, "时间戳应被改写为当前时间");
  }

  /// 场景：Events 步骤回放事件并以 Done(Stop) 收敛。
  ///
  /// 预期：Start 自动补发；1 个 TextStart + 2 个 TextDelta 按序到达；
  /// 最终消息取最后一个内容事件的 partial（"hello"），stop_reason 为 Stop。
  #[tokio::test]
  async fn events_step_replays_and_converges_to_done() {
    let events = vec![
      ProviderEvent::TextStart {
        index: 0,
        partial: text_partial("he"),
      },
      ProviderEvent::TextDelta {
        index: 0,
        delta: "he".to_string(),
        partial: text_partial("he"),
      },
      ProviderEvent::TextDelta {
        index: 0,
        delta: "llo".to_string(),
        partial: text_partial("hello"),
      },
    ];
    let (provider, model) = mock_provider(vec![MockStep::Events(events)]);
    let stream_options = StreamOptions::default();
    let ctx = Context::default();
    let mut stream = provider
      .stream(ProviderOptions {
        model: &model,
        context: &ctx,
        options: &stream_options,
      })
      .await
      .expect("Events 步骤建流成功");

    assert!(
      matches!(stream.next().await, Some(ProviderEvent::Start { .. })),
      "Start 应自动补发"
    );
    let mut content_events = 0;
    loop {
      match stream.next().await {
        Some(event) if event.is_content() => content_events += 1,
        Some(ProviderEvent::Done { reason, message }) => {
          assert_eq!(reason, StopReason::Stop);
          assert_eq!(message.content.len(), 1, "最终消息应取最后的 partial");
          break;
        }
        Some(other) => panic!("意外事件 {other:?}"),
        None => panic!("流应在 Done 后正常结束，不应提前断流"),
      }
    }
    assert_eq!(
      content_events, 3,
      "1 个 TextStart + 2 个 TextDelta 应按序回放"
    );
  }

  /// 场景：StreamFail 步骤先回放事件再以脚本化错误终结。
  ///
  /// 预期：内容事件先回放；Error 事件携带脚本指定的 SERVER_ERROR。
  /// （错误码经事件路径断言——result() 只回终态消息，不区分成败。）
  #[tokio::test]
  async fn stream_fail_step_terminates_with_scripted_error() {
    let events = vec![ProviderEvent::TextStart {
      index: 0,
      partial: text_partial("partial"),
    }];
    let (provider, model) = mock_provider(vec![MockStep::StreamFail {
      events,
      reason: StopReason::Error,
      code: Some(SERVER_ERROR),
    }]);
    let stream_options = StreamOptions::default();
    let ctx = Context::default();
    let mut stream = provider
      .stream(ProviderOptions {
        model: &model,
        context: &ctx,
        options: &stream_options,
      })
      .await
      .expect("StreamFail 建流成功");

    assert!(
      matches!(stream.next().await, Some(ProviderEvent::Start { .. })),
      "Start 应自动补发"
    );
    let mut saw_content = false;
    loop {
      match stream.next().await {
        Some(event) if event.is_content() => saw_content = true,
        Some(ProviderEvent::Error { reason, code, .. }) => {
          assert!(saw_content, "错误前应已回放内容事件");
          assert_eq!(reason, StopReason::Error);
          assert_eq!(code, Some(SERVER_ERROR));
          break;
        }
        Some(other) => panic!("意外事件 {other:?}"),
        None => panic!("流应以 Error 终结，不应提前断流"),
      }
    }
  }

  /// 场景：Silent 步骤在静默期满后以 Done(Stop) 收敛空消息。
  ///
  /// 预期：start_paused 虚拟时钟下确定性到期；最终消息无内容。
  #[tokio::test(start_paused = true)]
  async fn silent_step_yields_done_after_duration() {
    let (provider, model) = mock_provider(vec![MockStep::Silent {
      duration: Duration::from_secs(5),
    }]);
    let stream_options = StreamOptions::default();
    let ctx = Context::default();
    let message = provider
      .stream(ProviderOptions {
        model: &model,
        context: &ctx,
        options: &stream_options,
      })
      .await
      .expect("Silent 步骤建流成功")
      .result()
      .await
      .expect("静默期满应正常收敛");

    assert_eq!(message.stop_reason, StopReason::Stop);
    assert!(message.content.is_empty(), "静默期满应收敛空消息");
  }

  /// 场景：静默期间取消令牌触发。
  ///
  /// 预期：以 Aborted + CANCELLED 终结（取消不计入上游故障）。
  /// （错误码经事件路径断言——result() 只回终态消息，不区分成败。）
  #[tokio::test]
  async fn silent_step_aborts_on_cancel() {
    let (provider, model) = mock_provider(vec![MockStep::Silent {
      duration: Duration::from_secs(60),
    }]);
    let stream_options = StreamOptions::default();
    let cancel = stream_options.cancel.clone().expect("默认选项自带取消令牌");
    let ctx = Context::default();
    let mut stream = provider
      .stream(ProviderOptions {
        model: &model,
        context: &ctx,
        options: &stream_options,
      })
      .await
      .expect("Silent 步骤建流成功");

    assert!(
      matches!(stream.next().await, Some(ProviderEvent::Start { .. })),
      "Start 应先于取消到达"
    );
    cancel.cancel();
    match stream.next().await {
      Some(ProviderEvent::Error { reason, code, .. }) => {
        assert_eq!(reason, StopReason::Aborted);
        assert_eq!(code, Some(CANCELLED));
      }
      other => panic!("预期取消终结，实际 {other:?}"),
    }
  }

  /// 场景：限速回放期间取消立即生效。
  ///
  /// 预期：Start 到达后取消，回放任务在事件间隔处中止，
  /// 后续事件被 `Error(Aborted, CANCELLED)` 终结取代。
  /// 间隔取 1 小时保证取消必然先于首个 tick 到期（确定性）。
  #[tokio::test]
  async fn event_interval_replay_aborts_on_cancel() {
    let events = vec![
      ProviderEvent::TextDelta {
        index: 0,
        delta: "a".to_string(),
        partial: text_partial("a"),
      },
      ProviderEvent::TextDelta {
        index: 0,
        delta: "b".to_string(),
        partial: text_partial("ab"),
      },
    ];
    let (provider, model) = mock_provider(vec![MockStep::Events(events)]);
    let mut stream_options = StreamOptions::default();
    let cancel = CancelToken::new();
    stream_options.cancel = Some(cancel.clone());
    let ctx = Context::default();
    let mut stream = provider
      .stream(ProviderOptions {
        model: &model,
        context: &ctx,
        options: &stream_options,
      })
      .await
      .expect("限速回放建流成功");

    assert!(
      matches!(stream.next().await, Some(ProviderEvent::Start { .. })),
      "Start 应先于取消到达"
    );
    cancel.cancel();
    match stream.next().await {
      Some(ProviderEvent::Error { reason, code, .. }) => {
        assert_eq!(reason, StopReason::Aborted);
        assert_eq!(code, Some(CANCELLED));
      }
      other => panic!("预期取消终结，实际 {other:?}"),
    }
  }

  /// 场景：多次调用生成递增序号的记录，并快照请求标签。
  ///
  /// 预期：record() 按调用顺序返回；sequence 从 0 单调递增；
  /// model 与 labels 原样快照（即使脚本已耗尽，记录依然完整）。
  #[tokio::test]
  async fn record_tracks_model_sequence_and_labels() {
    let (provider, model) =
      mock_provider(vec![MockStep::Fail(pre_stream(SERVER_ERROR, "boom"))]);
    let mut stream_options = StreamOptions::default();
    stream_options
      .labels
      .insert("route".to_string(), "test".to_string());
    let ctx = Context::default();
    let opts = ProviderOptions {
      model: &model,
      context: &ctx,
      options: &stream_options,
    };

    // 连续两次调用：第一次消费 Fail 步骤（建流返回 Err，忽略结果），
    // 第二次脚本耗尽（两者都产生完整调用记录——先记录后弹出保证这一点）
    let _ = provider.stream(opts).await;
    let _ = provider
      .stream(opts)
      .await
      .expect("第二次建流")
      .result()
      .await;

    let calls = provider.record();
    assert_eq!(calls.len(), 2, "两次调用都应被记录");
    assert_eq!(calls[0].sequence, 0);
    assert_eq!(calls[1].sequence, 1);
    assert_eq!(calls[0].model, model.id);
    assert_eq!(
      calls[0].labels.get("route").map(String::as_str),
      Some("test")
    );
  }

  /// 场景：消费端在建流后立即丢弃。
  ///
  /// 预期：回放任务在 push 返回 false 时静默退出——既不终结、
  /// 不 panic、也不挂起。
  #[tokio::test]
  async fn consumer_drop_stops_replay_silently() {
    let events = vec![ProviderEvent::TextDelta {
      index: 0,
      delta: "x".to_string(),
      partial: text_partial("x"),
    }];
    let (provider, model) = mock_provider(vec![MockStep::Events(events)]);
    let stream_options = StreamOptions::default();
    let ctx = Context::default();
    let stream = provider
      .stream(ProviderOptions {
        model: &model,
        context: &ctx,
        options: &stream_options,
      })
      .await
      .expect("建流成功");

    // 立即放弃消费，并给回放任务让出调度机会让其观察到对端关闭
    drop(stream);
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
  }

  /// 场景：为回放设置事件间隔并读取身份信息。
  ///
  /// 预期：每个事件之间按间隔推进虚拟时钟；`id` / `models` 透传目录。
  #[tokio::test(start_paused = true)]
  async fn event_interval_delays_each_event() {
    let events = vec![
      ProviderEvent::TextDelta {
        index: 0,
        delta: "a".to_string(),
        partial: text_partial("a"),
      },
      ProviderEvent::TextDelta {
        index: 0,
        delta: "b".to_string(),
        partial: text_partial("ab"),
      },
    ];
    let (provider, model) = mock_provider(vec![MockStep::Events(events)]);
    let provider = provider.with_event_interval(Duration::from_millis(10));

    assert_eq!(provider.id().as_str(), "mock");
    assert_eq!(provider.models().len(), 1);

    let stream_options = StreamOptions::default();
    let ctx = Context::default();
    let start = tokio::time::Instant::now();
    let message = provider
      .stream(ProviderOptions {
        model: &model,
        context: &ctx,
        options: &stream_options,
      })
      .await
      .expect("限速回放建流成功")
      .result()
      .await
      .expect("限速回放应正常收敛");

    assert!(
      start.elapsed() >= Duration::from_millis(20),
      "两个事件应各等待一个间隔"
    );
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(message.content.len(), 1);
  }

  /// 场景：Events 步骤为空且未指定终结。
  ///
  /// 预期：以 Done(Stop) 收敛空消息（走空快照兜底分支）。
  #[tokio::test]
  async fn empty_events_step_converges_with_empty_message() {
    let (provider, model) = mock_provider(vec![MockStep::Events(Vec::new())]);
    let stream_options = StreamOptions::default();
    let ctx = Context::default();

    let message = provider
      .stream(ProviderOptions {
        model: &model,
        context: &ctx,
        options: &stream_options,
      })
      .await
      .expect("空事件脚本建流成功")
      .result()
      .await
      .expect("空事件脚本应正常收敛");

    assert_eq!(message.stop_reason, StopReason::Stop);
    assert!(message.content.is_empty());
  }
}
