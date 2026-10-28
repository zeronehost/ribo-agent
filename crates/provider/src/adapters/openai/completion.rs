use super::{build_params, sse};
use crate::error::{from_http, from_reqwest};
use ra_core::error::RiboResult;

use crate::{
  error::{AUTH_FAILED, CANCELLED, INVALID_REQUEST, TIMEOUT, pre_stream},
  ids::ProviderId,
  model::Model,
  options::ProviderOptions,
  stream::{ProviderEventStream, ProviderEventStreamProducer},
  traits::Provider,
};

/// OpenAI 兼容适配器。
///
/// 持有状态与 [`Provider`] 实现解耦：请求体构造委托 `build_params`、
/// 流式解析委托 `sse::spawn_parse_task`。
pub struct OpenAiCompatibleAdapter {
  /// 提供方标识（注册校验保证目录内 `Model.provider` 与之一致）。
  id: ProviderId,
  /// 模型目录快照（只读，注册后不得就地修改；端点取自 `Model.base_url`）。
  models: Vec<Model>,
  /// 共享 HTTP 客户端：只设 `connect_timeout`，**不得**设全局 `timeout`——
  /// 建流超时必须只约束到响应头到达，否则会把慢启动误判为失败。
  http: reqwest::Client,
  /// 鉴权密钥：只进入请求头，禁止进入日志与错误 context。
  api_key: Option<String>,
}

impl OpenAiCompatibleAdapter {
  /// 构造 OpenAI 兼容适配器。
  ///
  /// # 参数
  ///
  /// - `id`：提供方标识（小写 ASCII，如 `openai`、`deepseek`）。
  /// - `models`：模型目录快照；`stream` 按 `Model.id` 做归属校验，
  ///   端点与附加头逐模型取自目录项。
  /// - `http`：共享 HTTP 客户端；调用方只应设置 `connect_timeout`，
  ///   不得设置全局 `timeout`（建流超时语义）。
  /// - `api_key`：Bearer 鉴权密钥；缺失时 `stream` 以 `AUTH_FAILED`
  ///   建流错误拒绝。密钥只进入请求头，不进入日志与错误 context。
  ///
  /// # 示例
  ///
  /// ```no_run
  /// // 此示例需要外部 HTTP 服务，因此只编译不运行
  /// use ra_provider::adapters::openai::completion::OpenAiCompatibleAdapter;
  /// use ra_provider::ids::ProviderId;
  /// use ra_provider::traits::Provider;
  ///
  /// let adapter = OpenAiCompatibleAdapter::new(
  ///   ProviderId::parse("openai").expect("valid provider id"),
  ///   Vec::new(), // 模型目录由部署配置注入
  ///   reqwest::Client::new(),
  ///   Some("sk-...".to_string()),
  /// );
  /// assert!(!adapter.is_retrying_provider());
  /// ```
  pub fn new(
    id: ProviderId,
    models: Vec<Model>,
    http: reqwest::Client,
    api_key: Option<String>,
  ) -> Self {
    Self {
      id,
      models,
      http,
      api_key,
    }
  }
}

#[async_trait::async_trait]
impl Provider for OpenAiCompatibleAdapter {
  fn id(&self) -> &ProviderId {
    &self.id
  }
  fn models(&self) -> &[Model] {
    &self.models
  }
  // is_retrying_provider：裸适配器不带重试，使用 trait 默认实现（false）。
  async fn stream(
    &self,
    options: ProviderOptions<'_>,
  ) -> RiboResult<ProviderEventStream> {
    // 解构借用入参：引用生命周期绑定调用方栈帧，
    // 不得把引用跨任务保存（需要保留时先克隆内部数据）。
    let ProviderOptions {
      model,
      context,
      options,
    } = options;

    // ===== 阶段 1：本地校验（PreStream）=====
    // 1a. 模型归属：不支持的模型属路由错误，立即拒绝。
    if !self.supports(&model.id) {
      tracing::warn!(
        provider = self.id().as_str(),
        model = model.id.as_str(),
        "model not exposed by provider"
      );
      return Err(pre_stream(
        INVALID_REQUEST,
        format!(
          "model {} is not exposed by provider {}",
          model.id.as_str(),
          self.id().as_str()
        ),
      ));
    }
    // 1b. 鉴权配置：StreamOptions 不携带密钥（见 crate::options），
    //     缺失即部署配置错误，在本地校验阶段拦截。
    let Some(api_key) = &self.api_key else {
      tracing::error!(provider = self.id().as_str(), "api key not configured");
      return Err(pre_stream(
        AUTH_FAILED,
        "api key not configured for provider",
      ));
    };
    // 1c. 选项合法性：非法采样参数不带病发出。
    if let Some(t) = options.sampling.temperature
      && !(0.0..=2.0).contains(&t)
    {
      return Err(pre_stream(
        INVALID_REQUEST,
        format!("temperature {t} out of range [0, 2]"),
      ));
    }

    // ===== 阶段 2：构造请求（PreStream）=====
    // 端点取自模型快照（支持按模型代理）；鉴权外附加头 = 模型自带 header
    // + 调用方 extra_headers。请求体由 build_params 模块负责
    // （Context / StreamOptions → 线格式）。
    let endpoint =
      format!("{}/chat/completions", model.base_url.trim_end_matches('/'));
    let body = build_params::build(model, context, options);
    let mut req = self.http.post(&endpoint).bearer_auth(api_key).json(&body);
    for (key, value) in model.header.iter().flatten() {
      req = req.header(key, value);
    }
    for (key, value) in &options.extra_headers {
      req = req.header(key, value);
    }

    // ===== 阶段 3：发送并等待响应头（PreStream，受 timeout 约束）=====
    // 建流超时只约束到响应头到达：用 select! 竞争实现而非
    // per-request timeout——后者覆盖整个 body 读取期，会把建流超时
    // 误扩展为"流总时长超时"，与阶段语义冲突。
    // options.timeout 为 None 表示不限制（crate::options 语义）。
    let response = tokio::select! {
      biased;
      // 建流等待中取消：立即放弃，
      // 被 drop 的 send future 会中止底层请求。
      _ = async {
        match &options.cancel {
          Some(t) => t.cancelled().await,
          // 显式 None = 不可取消：退化为永不触发的分支
          None => std::future::pending::<()>().await,
        }
      } => {
        tracing::info!(
          provider = self.id().as_str(), model = model.id.as_str(),
          "pre-stream cancelled"
        );
        return Err(pre_stream(
          CANCELLED,
          "cancelled while awaiting response headers",
        ));
      }
      _ = async {
        match options.timeout {
          Some(d) => tokio::time::sleep(d).await,
          None => std::future::pending::<()>().await,
        }
      } => {
        tracing::warn!(
          provider = self.id().as_str(), model = model.id.as_str(),
          "pre-stream timeout while awaiting response headers"
        );
        return Err(pre_stream(TIMEOUT, "response headers not received in time"));
      }
      resp = req.send() => resp,
    };

    // ===== 非 2xx / 传输失败：建流错误（可重试 / 可降级）=====
    let response = match response {
      Ok(resp) if resp.status().is_success() => resp,
      Ok(resp) => {
        let status = resp.status();
        // body 截断（512 字节、UTF-8 边界）与换行清理由 from_http 统一处理
        let body_text = resp.text().await.unwrap_or_default();
        tracing::warn!(
          provider = self.id().as_str(),
          model = model.id.as_str(),
          status = status.as_u16(),
          "pre-stream http error"
        );
        return Err(from_http(status.as_u16(), &body_text));
      }
      // DNS / TCP / TLS / 连接超时：保留 source 链
      Err(err) => {
        tracing::warn!(
          provider = self.id().as_str(), model = model.id.as_str(),
          error = %err, "pre-stream transport error"
        );
        return Err(from_reqwest(err));
      }
    };

    // ===== 阶段 4：建立事件通道（PreStream）=====
    let (producer, stream) = ProviderEventStreamProducer::build();

    // ===== 阶段 5：spawn 解析任务（InStream）=====
    // 建流 Promise：此处返回 Ok 不保证已有任何内容事件；
    // 首字迟滞由 TTFT 守卫或看门狗处理，绝不在建流路径等待首包。
    // 解析任务骨架见，各适配器在自己的 sse 模块中提供同构实现。
    sse::spawn_parse_task(
      response,
      producer,
      options.cancel.clone().unwrap_or_default(),
      self.id().clone(),
      model.id.clone(),
    );

    Ok(stream)
  }
}

#[cfg(test)]
mod tests {
  use std::{collections::BTreeMap, net::SocketAddr, time::Duration};

  use tokio::io::{AsyncReadExt, AsyncWriteExt};
  use tokio::net::TcpListener;

  use super::*;
  use crate::{
    context::Context,
    error::{RATE_LIMITED, TRANSPORT},
    ids::ModelId,
    options::{CancelToken, SamplingOptions, StreamOptions},
  };

  /// 启动一次性本地 HTTP 服务。
  ///
  /// `response = Some(..)` 时立即返回固定响应；`None` 时接受连接后保持
  /// 静默 150ms，用于超时/取消场景。
  async fn serve(
    response: Option<String>,
  ) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    crate::test_support::init();
    let listener = TcpListener::bind("127.0.0.1:0")
      .await
      .expect("绑定本地端口");
    let addr = listener.local_addr().expect("读取本地地址");
    let handle = tokio::spawn(async move {
      let (mut socket, _) = listener.accept().await.expect("接受连接");
      // 先读取请求（头 + 小 JSON 体），避免未读数据导致 Windows 上的 RST。
      let mut request = [0u8; 8192];
      let _ = socket.read(&mut request).await;
      match response {
        Some(body) => {
          let _ = socket.write_all(body.as_bytes()).await;
          let _ = socket.shutdown().await;
        }
        None => {
          tokio::time::sleep(Duration::from_millis(150)).await;
          drop(socket);
        }
      }
    });
    (addr, handle)
  }

  /// 启动一次性本地服务：先发送 `head`，保持连接 `hold_ms` 后关闭。
  ///
  /// 用于“流式传输中途”的取消/读错误场景。
  async fn serve_partial(
    head: String,
    hold_ms: u64,
  ) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    crate::test_support::init();
    let listener = TcpListener::bind("127.0.0.1:0")
      .await
      .expect("绑定本地端口");
    let addr = listener.local_addr().expect("读取本地地址");
    let handle = tokio::spawn(async move {
      let (mut socket, _) = listener.accept().await.expect("接受连接");
      let mut request = [0u8; 8192];
      let _ = socket.read(&mut request).await;
      let _ = socket.write_all(head.as_bytes()).await;
      tokio::time::sleep(Duration::from_millis(hold_ms)).await;
      let _ = socket.shutdown().await;
    });
    (addr, handle)
  }

  /// 构造固定 Content-Length 的 HTTP 响应。
  fn http_response(status_line: &str, body: &str) -> String {
    format!(
      "HTTP/1.1 {status_line}\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
      body.len()
    )
  }

  fn model_id(raw: &str) -> ModelId {
    ModelId::parse(raw).expect("测试模型 ID 合法")
  }

  /// 构造指向本地服务的适配器（模型带一个附加请求头，覆盖 header 注入）。
  fn adapter(
    addr: SocketAddr,
    api_key: Option<&str>,
  ) -> OpenAiCompatibleAdapter {
    let pid = ProviderId::parse("p1").expect("测试提供方 ID 合法");
    let model = Model::new(
      pid.clone(),
      model_id("m1"),
      "Model One",
      format!("http://{addr}"),
    )
    .with_header(BTreeMap::from([(
      "x-trace-id".to_string(),
      "1".to_string(),
    )]));
    OpenAiCompatibleAdapter::new(
      pid,
      vec![model],
      reqwest::Client::new(),
      api_key.map(str::to_string),
    )
  }

  fn other_model() -> Model {
    Model::new(
      ProviderId::parse("p1").expect("测试提供方 ID 合法"),
      model_id("other"),
      "Other",
      "http://127.0.0.1:1",
    )
  }

  /// 场景：读取适配器的身份与模型目录。
  #[test]
  fn exposes_identity_and_models() {
    let addr = "127.0.0.1:1".parse::<SocketAddr>().expect("地址");
    let adapter = adapter(addr, Some("key"));

    assert_eq!(adapter.id().as_str(), "p1");
    assert_eq!(adapter.models().len(), 1);
    assert!(!adapter.is_retrying_provider());
  }

  /// 场景：请求未在目录中暴露的模型。
  ///
  /// 预期：本地校验阶段返回 INVALID_REQUEST，不发起请求。
  #[tokio::test]
  async fn rejects_model_not_exposed() {
    let addr = "127.0.0.1:1".parse::<SocketAddr>().expect("地址");
    let adapter = adapter(addr, Some("key"));
    let model = other_model();
    let context = Context::default();
    let options = StreamOptions::default();

    let err = adapter
      .stream(ProviderOptions {
        model: &model,
        context: &context,
        options: &options,
      })
      .await
      .err()
      .expect("未暴露的模型应被拒绝");

    assert_eq!(err.code(), INVALID_REQUEST);
  }

  /// 场景：未配置 API Key。
  ///
  /// 预期：返回 AUTH_FAILED。
  #[tokio::test]
  async fn rejects_missing_api_key() {
    let addr = "127.0.0.1:1".parse::<SocketAddr>().expect("地址");
    let adapter = adapter(addr, None);
    let model = adapter.models()[0].clone();
    let context = Context::default();
    let options = StreamOptions::default();

    let err = adapter
      .stream(ProviderOptions {
        model: &model,
        context: &context,
        options: &options,
      })
      .await
      .err()
      .expect("缺少密钥应被拒绝");

    assert_eq!(err.code(), AUTH_FAILED);
  }

  /// 场景：采样温度超出合法区间。
  ///
  /// 预期：返回 INVALID_REQUEST，不发起请求。
  #[tokio::test]
  async fn rejects_invalid_temperature() {
    let addr = "127.0.0.1:1".parse::<SocketAddr>().expect("地址");
    let adapter = adapter(addr, Some("key"));
    let model = adapter.models()[0].clone();
    let context = Context::default();
    let options = StreamOptions {
      sampling: SamplingOptions {
        temperature: Some(3.0),
        ..SamplingOptions::default()
      },
      ..StreamOptions::default()
    };

    let err = adapter
      .stream(ProviderOptions {
        model: &model,
        context: &context,
        options: &options,
      })
      .await
      .err()
      .expect("非法温度应被拒绝");

    assert_eq!(err.code(), INVALID_REQUEST);
  }

  /// 场景：上游返回非 2xx。
  ///
  /// 预期：按 HTTP 状态映射为 RATE_LIMITED。
  #[tokio::test]
  async fn maps_http_error_status() {
    let (addr, handle) =
      serve(Some(http_response("429 Too Many Requests", "slow"))).await;
    let adapter = adapter(addr, Some("key"));
    let model = adapter.models()[0].clone();
    let context = Context::default();
    let options = StreamOptions::default();

    let err = adapter
      .stream(ProviderOptions {
        model: &model,
        context: &context,
        options: &options,
      })
      .await
      .err()
      .expect("非 2xx 应返回错误");
    handle.await.expect("服务端任务不应 panic");

    assert_eq!(err.code(), RATE_LIMITED);
  }

  /// 场景：连接失败（端口不可达）。
  ///
  /// 预期：映射为 TRANSPORT。
  #[tokio::test]
  async fn maps_transport_error() {
    let addr = "127.0.0.1:1".parse::<SocketAddr>().expect("地址");
    let adapter = adapter(addr, Some("key"));
    let model = adapter.models()[0].clone();
    let context = Context::default();
    let options = StreamOptions::default();

    let err = adapter
      .stream(ProviderOptions {
        model: &model,
        context: &context,
        options: &options,
      })
      .await
      .err()
      .expect("连接失败应返回错误");

    assert_eq!(err.code(), TRANSPORT);
  }

  /// 场景：等待响应头超时。
  ///
  /// 预期：返回 TIMEOUT。
  #[tokio::test]
  async fn times_out_waiting_for_headers() {
    let (addr, handle) = serve(None).await;
    let adapter = adapter(addr, Some("key"));
    let model = adapter.models()[0].clone();
    let context = Context::default();
    let options = StreamOptions {
      timeout: Some(Duration::from_millis(50)),
      ..StreamOptions::default()
    };

    let err = adapter
      .stream(ProviderOptions {
        model: &model,
        context: &context,
        options: &options,
      })
      .await
      .err()
      .expect("等待响应头应超时");
    handle.await.expect("服务端任务不应 panic");

    assert_eq!(err.code(), TIMEOUT);
  }

  /// 场景：等待响应头期间被取消。
  ///
  /// 预期：返回 CANCELLED。
  #[tokio::test]
  async fn cancels_while_waiting_for_headers() {
    let (addr, handle) = serve(None).await;
    let adapter = adapter(addr, Some("key"));
    let model = adapter.models()[0].clone();
    let context = Context::default();
    let cancel = CancelToken::new();
    let canceller = {
      let cancel = cancel.clone();
      tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel.cancel();
      })
    };
    let options = StreamOptions {
      cancel: Some(cancel),
      ..StreamOptions::default()
    };

    let err = adapter
      .stream(ProviderOptions {
        model: &model,
        context: &context,
        options: &options,
      })
      .await
      .err()
      .expect("取消应返回错误");
    canceller.await.expect("取消任务不应 panic");
    handle.await.expect("服务端任务不应 panic");

    assert_eq!(err.code(), CANCELLED);
  }

  /// 场景：成功建流并消费 SSE。
  ///
  /// 预期：返回事件流，最终消息包含聚合文本且以 Stop 结束。
  #[tokio::test]
  async fn streams_sse_events_and_finishes() {
    let body = "data: {\"id\":\"r1\",\"model\":\"m1\",\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
    let (addr, handle) = serve(Some(http_response("200 OK", body))).await;
    let adapter = adapter(addr, Some("key"));
    let model = adapter.models()[0].clone();
    let context = Context::default();
    let options = StreamOptions {
      cancel: None,
      extra_headers: vec![("x-extra".to_string(), "1".to_string())],
      ..StreamOptions::default()
    };

    let stream = adapter
      .stream(ProviderOptions {
        model: &model,
        context: &context,
        options: &options,
      })
      .await
      .expect("建流应成功");
    let message = stream.result().await.expect("应收到终态");
    handle.await.expect("服务端任务不应 panic");

    assert_eq!(message.stop_reason, ra_core::message::StopReason::Stop);
    assert_eq!(message.content.len(), 1);
  }

  /// 场景：SSE 在末尾只有 `[DONE]` 且没有空行。
  ///
  /// 预期：EOF 冲刷残余帧后由 `[DONE]` 触发终结（走 EOF 冲刷的
  /// Terminated 分支）。
  #[tokio::test]
  async fn eof_with_trailing_done_without_blank_line() {
    let (addr, handle) =
      serve(Some(http_response("200 OK", "data: [DONE]"))).await;
    let adapter = adapter(addr, Some("key"));
    let model = adapter.models()[0].clone();
    let context = Context::default();
    let options = StreamOptions::default();

    let stream = adapter
      .stream(ProviderOptions {
        model: &model,
        context: &context,
        options: &options,
      })
      .await
      .expect("建流应成功");
    let message = stream.result().await.expect("应以 [DONE] 收敛");
    handle.await.expect("服务端任务不应 panic");

    assert!(message.content.is_empty());
  }

  /// 场景：消费端在建流后立即放弃。
  ///
  /// 预期：解析任务观察到消费端关闭并静默退出，不产生错误。
  #[tokio::test]
  async fn drop_stream_before_consuming() {
    let body = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
    let (addr, handle) = serve(Some(http_response("200 OK", body))).await;
    let adapter = adapter(addr, Some("key"));
    let model = adapter.models()[0].clone();
    let context = Context::default();
    let options = StreamOptions::default();

    let stream = adapter
      .stream(ProviderOptions {
        model: &model,
        context: &context,
        options: &options,
      })
      .await
      .expect("建流应成功");
    drop(stream);
    for _ in 0..5 {
      tokio::task::yield_now().await;
    }
    handle.await.expect("服务端任务不应 panic");
  }

  /// 场景：SSE 在最后一个帧后直接 EOF（无空行哨兵）。
  ///
  /// 预期：解析器冲刷残余帧，finalize 正常收敛。
  #[tokio::test]
  async fn stream_eof_without_done_still_converges() {
    let body =
      r#"data: {"choices":[{"delta":{"content":"x"},"finish_reason":"stop"}]}"#;
    let (addr, handle) = serve(Some(http_response("200 OK", body))).await;
    let adapter = adapter(addr, Some("key"));
    let model = adapter.models()[0].clone();
    let context = Context::default();
    let options = StreamOptions::default();

    let stream = adapter
      .stream(ProviderOptions {
        model: &model,
        context: &context,
        options: &options,
      })
      .await
      .expect("建流应成功");
    let message = stream.result().await.expect("EOF 后应收敛");
    handle.await.expect("服务端任务不应 panic");

    assert_eq!(message.stop_reason, ra_core::message::StopReason::Stop);
    assert_eq!(message.content.len(), 1);
  }

  /// 场景：流式传输中途取消。
  ///
  /// 预期：以 Aborted 终结，错误码为 CANCELLED。
  #[tokio::test]
  async fn cancels_mid_stream() {
    let head = format!(
      "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 100000\r\n\r\n{}",
      "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n"
    );
    let (addr, handle) = serve_partial(head, 300).await;
    let adapter = adapter(addr, Some("key"));
    let model = adapter.models()[0].clone();
    let context = Context::default();
    let cancel = CancelToken::new();
    let canceller = {
      let cancel = cancel.clone();
      tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        cancel.cancel();
      })
    };
    let options = StreamOptions {
      cancel: Some(cancel),
      ..StreamOptions::default()
    };

    let stream = adapter
      .stream(ProviderOptions {
        model: &model,
        context: &context,
        options: &options,
      })
      .await
      .expect("建流应成功");
    let message = stream.result().await.expect("取消应以终态返回");
    canceller.await.expect("取消任务不应 panic");
    handle.await.expect("服务端任务不应 panic");

    assert_eq!(message.stop_reason, ra_core::message::StopReason::Aborted);
  }

  /// 场景：响应体以 chunked 编码传输但中途被截断。
  ///
  /// 预期：读取失败映射为 TRANSPORT，并以 Error 终结。
  #[tokio::test]
  async fn read_error_maps_to_transport() {
    let raw = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n10\r\ndata: {\"choices\"";
    let (addr, handle) = serve(Some(raw.to_string())).await;
    let adapter = adapter(addr, Some("key"));
    let model = adapter.models()[0].clone();
    let context = Context::default();
    let options = StreamOptions::default();

    let stream = adapter
      .stream(ProviderOptions {
        model: &model,
        context: &context,
        options: &options,
      })
      .await
      .expect("响应头应到达");
    let message = stream.result().await.expect("读失败应以终态返回");
    handle.await.expect("服务端任务不应 panic");

    assert_eq!(message.stop_reason, ra_core::message::StopReason::Error);
    assert!(
      message
        .error_message
        .as_deref()
        .is_some_and(|text| text.contains("stream read failed"))
    );
  }
}
