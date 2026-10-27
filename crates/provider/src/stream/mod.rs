//! 事件流传输机制：无界 mpsc 事件通道 + oneshot 终态。
//!
//! 生产端与消费端通过 [`ProviderEventStreamProducer::build`] 同时创建两对通道：
//!
//! - 无界 mpsc：承载所有事件（含终结），供逐事件消费路径 [`ProviderEventStream::next`]。
//! - oneshot：承载终态 `AssistantMessage`，供只取最终结果路径
//!   [`ProviderEventStream::result`]。
//!
//! 终结事件在推入 mpsc 的同一时刻兑现 oneshot，使"逐条消费"与"只取终态"
//! 两种用法共享同一份语义。
//!
//! # 不变量
//!
//! 1. `Start` 必须先于任何其他事件被推送；
//! 2. 终结只能发生一次——`done` / `error` 消费 `self`，编译期排除二次终结；
//! 3. 若生产端在未终结时被 drop（逻辑漏洞或任务异常退出），
//!    消费端 [`ProviderEventStream::result`] 以
//!    `Err(STREAM_INTERRUPTED, InStream)` 结束，保证永不永久挂起。

mod event;
mod event_stream;
mod observed_stream;

pub use self::{event::ProviderEvent, event_stream::*, observed_stream::*};
