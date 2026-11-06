//! 适配器公共骨架：身份 / 模型目录 / HTTP 客户端 / 鉴权密钥。
//!
//! 三家线格式（Chat Completions、Responses、Anthropic Messages）的
//! `Provider::stream` 在 PreStream 阶段完全同构：
//!
//! 1. 模型归属校验（目录外模型直接拒绝）；
//! 2. 鉴权配置校验（缺密钥即部署错误）；
//! 3. 采样参数校验（非法参数不带病发出）；
//! 4. 发送请求并等待响应头（受取消 / 超时约束）；
//! 5. 非 2xx 与传输错误映射为建流错误。
//!
//! 上述步骤统一收敛到 [`ProviderCore`]，各适配器只保留线格式差异：
//! 端点拼接、鉴权头形态与请求体构造。

use ra_core::error::RiboResult;
use ra_core::message::Content;

use crate::{
  error::{
    AUTH_FAILED, CANCELLED, INVALID_REQUEST, TIMEOUT, from_http, from_reqwest,
    pre_stream,
  },
  ids::ProviderId,
  model::Model,
  options::StreamOptions,
};

/// 拼接内容块中的文本片段（忽略思考、工具调用等非文本块）。
///
/// 多个文本块以换行符连接——三家线格式（OpenAI Chat / Responses /
/// Anthropic）对同一条消息的多段文本均采用该表示，因此上提到共享层。
pub(crate) fn join_text(blocks: &[Content]) -> String {
  blocks
    .iter()
    .filter_map(|block| match block {
      Content::Text(text) => Some(text.content.as_str()),
      _ => None,
    })
    .collect::<Vec<_>>()
    .join("\n")
}

/// 适配器共享状态与建流骨架。
///
/// 字段语义与历史实现一致：
///
/// - `id`：提供方标识（注册校验保证目录内 `Model.provider` 与之一致）；
/// - `models`：模型目录快照（注册后不得就地修改）；
/// - `http`：共享客户端，只设 `connect_timeout`，**不得**设全局
///   `timeout`——建流超时必须只约束到响应头到达；
/// - `api_key`：只进入请求头，禁止进入日志与错误 context。
pub(crate) struct ProviderCore {
  id: ProviderId,
  models: Vec<Model>,
  http: reqwest::Client,
  api_key: Option<String>,
}

impl ProviderCore {
  /// 构造共享骨架。
  pub(crate) fn new(
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

  /// 提供方标识。
  pub(crate) fn id(&self) -> &ProviderId {
    &self.id
  }

  /// 模型目录快照。
  pub(crate) fn models(&self) -> &[Model] {
    &self.models
  }

  /// 鉴权密钥：缺失即部署配置错误。
  ///
  /// 与 [`crate::traits::Provider::supports`] 的默认实现一致，
  /// 按模型目录线性查找。
  pub(crate) fn api_key(&self) -> RiboResult<&str> {
    match self.api_key.as_deref() {
      Some(key) => Ok(key),
      None => {
        tracing::error!(provider = self.id.as_str(), "api key not configured");
        Err(pre_stream(
          AUTH_FAILED,
          "api key not configured for provider",
        ))
      }
    }
  }

  /// PreStream 校验：模型归属 → 鉴权 → 采样参数。
  ///
  /// # 参数
  ///
  /// - `model`：本次调用的模型；
  /// - `options`：流式运行选项（温度校验只读取 `sampling.temperature`）；
  /// - `max_temperature`：该提供方允许的温度上限（含）。
  ///
  /// # 返回
  ///
  /// 校验通过时返回鉴权密钥，供调用方构造鉴权头。
  pub(crate) fn validate(
    &self,
    model: &Model,
    options: &StreamOptions,
    max_temperature: f64,
  ) -> RiboResult<&str> {
    // ===== 1. 模型归属：不支持的模型属路由错误，立即拒绝 =====
    if !self.models.iter().any(|m| m.id == model.id) {
      tracing::warn!(
        provider = self.id.as_str(),
        model = model.id.as_str(),
        "model not exposed by provider"
      );
      return Err(pre_stream(
        INVALID_REQUEST,
        format!(
          "model {} is not exposed by provider {}",
          model.id.as_str(),
          self.id.as_str()
        ),
      ));
    }

    // ===== 2. 鉴权配置 =====
    let api_key = self.api_key()?;

    // ===== 3. 采样参数合法性 =====
    if let Some(t) = options.sampling.temperature
      && !(0.0..=max_temperature).contains(&t)
    {
      return Err(pre_stream(
        INVALID_REQUEST,
        format!("temperature {t} out of range [0, {max_temperature}]"),
      ));
    }

    Ok(api_key)
  }

  /// 发送请求并等待响应头，返回 2xx 响应。
  ///
  /// `build` 由调用方注入线格式差异（端点鉴权头、`content-type`、请求体）；
  /// 模型自带 `header` 与调用方 `extra_headers` 在此统一追加。
  ///
  /// # 超时语义
  ///
  /// 建流超时只约束到响应头到达：用 `select!` 竞争实现而非
  /// per-request `timeout`——后者覆盖整个 body 读取期，会把建流超时
  /// 误扩展为「流总时长超时」。`options.timeout` 为 `None` 表示不限制。
  pub(crate) async fn send_stream<F>(
    &self,
    endpoint: &str,
    model: &Model,
    options: &StreamOptions,
    build: F,
  ) -> RiboResult<reqwest::Response>
  where
    F: FnOnce(reqwest::RequestBuilder) -> reqwest::RequestBuilder,
  {
    // ===== 请求构造：鉴权由闭包注入，附加头在此统一追加 =====
    let mut req = build(self.http.post(endpoint));
    for (key, value) in model.header.iter().flatten() {
      req = req.header(key, value);
    }
    for (key, value) in &options.extra_headers {
      req = req.header(key, value);
    }

    // 外部调用起点：记录出站请求锚点（endpoint 不含鉴权信息）
    tracing::debug!(
      provider = self.id.as_str(),
      model = model.id.as_str(),
      endpoint = %endpoint,
      "sending provider request"
    );

    // ===== 发送并等待响应头（受 cancel / timeout 约束）=====
    let response = tokio::select! {
      biased;
      // 建流等待中取消：立即放弃，被 drop 的 send future 会中止底层请求。
      _ = async {
        match &options.cancel {
          Some(token) => token.cancelled().await,
          // 显式 None = 不可取消：退化为永不触发的分支
          None => std::future::pending::<()>().await,
        }
      } => {
        tracing::info!(
          provider = self.id.as_str(), model = model.id.as_str(),
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
          provider = self.id.as_str(), model = model.id.as_str(),
          "pre-stream timeout while awaiting response headers"
        );
        return Err(pre_stream(TIMEOUT, "response headers not received in time"));
      }
      resp = req.send() => resp,
    };

    // ===== 非 2xx / 传输失败：建流错误（可重试 / 可降级）=====
    match response {
      Ok(resp) if resp.status().is_success() => Ok(resp),
      Ok(resp) => {
        let status = resp.status();
        // body 截断（512 字节、UTF-8 边界）与换行清理由 from_http 统一处理
        let body_text = resp.text().await.unwrap_or_default();
        tracing::warn!(
          provider = self.id.as_str(),
          model = model.id.as_str(),
          status = status.as_u16(),
          "pre-stream http error"
        );
        Err(from_http(status.as_u16(), &body_text))
      }
      // DNS / TCP / TLS / 连接超时：保留 source 链
      Err(err) => {
        tracing::warn!(
          provider = self.id.as_str(), model = model.id.as_str(),
          error = %err, "pre-stream transport error"
        );
        Err(from_reqwest(err))
      }
    }
  }
}
