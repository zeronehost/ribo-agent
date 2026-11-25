//! 适配器共享的 SSE 解析任务骨架。
//!
//! 各服务商的差异只在「帧 → 事件」的方言映射上（[`LineFlow`] 的返回值）。
//! 建流之后的编排完全同构，因此收敛到一处：
//!
//! - 生产者义务第 1 条：先推 `Start`；
//! - `cancel` 与 body 读取竞争，取消以 `Aborted` 终结；
//! - 逐帧分派，消费端消失即停止解析并释放连接；
//! - EOF 冲刷残余帧后 `finalize`（含截断检测）；
//! - 收尾兜底：绝不出现「既未终结、消费端也未放弃」的静默退出。
//!
//! 方言层只需提供 `dispatch` 闭包；流内私有状态可被闭包捕获。

use crate::{
  error::{CANCELLED, INVALID_RESPONSE, TRANSPORT},
  ids::{ModelId, ProviderId},
  options::CancelToken,
  stream::{
    LineFlow, ProviderEvent, ProviderEventStreamProducer, SseFrame, SseParser,
    StreamAggregator,
  },
};
use ra_core::message::StopReason;

/// 以指定错误码终结流（消费 `producer` 的终结能力，保证恰好一次）。
///
/// # 参数
///
/// - `producer`：生产端槽位；`take` 后不可能二次终结。
/// - `aggregator`：用于构造携带已聚合内容的终态消息。
/// - `reason`：终态原因（`Error` / `Aborted`）。
/// - `message`：面向调用方的错误描述（不得包含密钥等敏感信息）。
/// - `code`：可选内部错误码；`None` 表示未分类。
///
/// # 返回
///
/// 恒为 [`LineFlow::Terminated`]，便于在分派分支中直接返回。
pub(crate) fn terminate(
  producer: &mut Option<ProviderEventStreamProducer>,
  aggregator: &StreamAggregator,
  reason: StopReason,
  message: impl Into<String>,
  code: Option<&'static str>,
) -> LineFlow {
  let message = message.into();
  let terminal = aggregator.terminal_error(reason.clone(), &message);
  if let Some(p) = producer.take() {
    p.error(reason, terminal, code);
  }
  LineFlow::Terminated
}

/// 以 [`INVALID_RESPONSE`] 终结流（JSON 非法 / 必需结构缺失的唯一出口）。
pub(crate) fn invalid_response(
  producer: &mut Option<ProviderEventStreamProducer>,
  aggregator: &StreamAggregator,
  reason: impl Into<String>,
) -> LineFlow {
  let reason = reason.into();
  tracing::warn!(
    provider = aggregator.provider().as_str(),
    reason = %reason,
    "invalid provider response, terminating stream"
  );
  terminate(
    producer,
    aggregator,
    StopReason::Error,
    reason,
    Some(INVALID_RESPONSE),
  )
}

/// 启动 SSE 解析任务。
///
/// 自带 `tokio::spawn`，调用方（`Provider::stream`）直接调用即可。
///
/// # 参数
///
/// - `response`：已拿到响应头的流式响应体。
/// - `producer`：事件生产端（本函数负责其生命周期与终结）。
/// - `cancel`：取消令牌；取消时以 `Aborted + CANCELLED` 终结。
/// - `provider` / `model`：用于聚合器元信息与结构化日志。
/// - `dispatch`：方言分派函数，返回 [`LineFlow`] 指示继续/终结/消费端消失。
///
/// # 生产者义务
///
/// `Start` 是第一个事件；每个内容事件携带最新 `partial` 快照
/// （由 [`StreamAggregator`] 保证）；每条结束路径恰好终结一次。
pub(crate) fn spawn_sse_task<F>(
  mut response: reqwest::Response,
  producer: ProviderEventStreamProducer,
  cancel: CancelToken,
  provider: ProviderId,
  model: ModelId,
  mut dispatch: F,
) where
  F: FnMut(
      &SseFrame,
      &mut StreamAggregator,
      &mut Option<ProviderEventStreamProducer>,
    ) -> LineFlow
    + Send
    + 'static,
{
  tokio::spawn(async move {
    // 初始化阶段
    let mut producer = Some(producer);
    let mut aggregator = StreamAggregator::new(provider, model);
    let mut parser = SseParser::default();
    let mut consumer_gone = false;

    // 生产者义务第 1 条：Start 必须是第一个事件。
    let started = ProviderEvent::Start {
      partial: aggregator.snapshot(),
    };
    if !producer.as_ref().is_some_and(|p| p.push(started)) {
      // 消费端在建流返回前已 drop 流：合法退出路径
      return;
    }

    // 主循环：SSE 字节流 vs 取消
    // 流式阶段不设总时长超时（首字迟滞归 TTFT 守卫、停滞归网关看门狗），
    // 适配器只处理网络错误与取消。
    loop {
      tokio::select! {
        biased;
        _ = cancel.cancelled() => {
          tracing::info!(
            provider = aggregator.provider().as_str(),
            model = aggregator.model().as_str(),
            "stream aborted by cancellation"
          );
          // 流式取消以 Aborted 终结，不得伪装成传输错误
          terminate(
            &mut producer,
            &aggregator,
            StopReason::Aborted,
            "cancelled while streaming",
            Some(CANCELLED),
          );
          return; // response 就地 drop → 底层连接释放
        }

        chunk = response.chunk() => match chunk {
          // 流正常结束：冲刷残余帧后聚合
          Ok(None) => {
            let mut terminated_early = false;
            for frame in parser.finish() {
              match dispatch(&frame, &mut aggregator, &mut producer) {
                LineFlow::Continue => {}
                LineFlow::Terminated => {
                  terminated_early = true;
                  break;
                }
                LineFlow::ConsumerGone => {
                  consumer_gone = true;
                  break;
                }
              }
            }
            // finalize 幂等；未收到方言终结事件时按截断终结
            if !terminated_early && !consumer_gone {
              aggregator.finalize(&mut producer);
            }
            break;
          }

          // 正常数据：切帧并逐帧驱动状态机
          Ok(Some(bytes)) => {
            'pump: for frame in parser.feed(&bytes) {
              match dispatch(&frame, &mut aggregator, &mut producer) {
                LineFlow::Continue => {}
                LineFlow::Terminated => break 'pump,
                LineFlow::ConsumerGone => {
                  consumer_gone = true;
                  break 'pump;
                }
              }
            }
            if consumer_gone {
              break; // 停止解析；response 随任务结束 drop
            }
          }

          // 网络中断 / 读失败：以 TRANSPORT 终结
          Err(err) => {
            tracing::warn!(
              provider = aggregator.provider().as_str(),
              model = aggregator.model().as_str(),
              error = %err,
              "stream read failed"
            );
            terminate(
              &mut producer,
              &aggregator,
              StopReason::Error,
              format!("stream read failed: {err}"),
              Some(TRANSPORT),
            );
            break;
          }
        }
      }
    }

    // 收尾兜底
    match producer {
      // 正常路径：done / error 已消费终结能力
      None => {}
      // push false 退出：消费端已不存在，STREAM_INTERRUPTED 无人接收
      Some(_) if consumer_gone => {}
      // 逻辑缺陷：既未终结、消费端也未放弃
      Some(p) => {
        tracing::error!(
          provider = aggregator.provider().as_str(),
          "parse task exited without terminal event"
        );
        drop(p);
      }
    }
  });
}
