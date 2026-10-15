//! `ra-provider`：模型提供方接入层。
//!
//! 本 crate 定义对接各家大模型服务所需的抽象与数据模型：
//!
//! - `model`：模型元信息，包括能力、模态、计费与思考档位；
//! - `context`：一次请求的对话上下文（系统提示词、消息历史、可用工具）；
//! - `options`：流式请求的运行选项及其局部覆盖（patch）；
//! - `stream`：流式响应的事件模型与生产/消费双通道；
//! - `provider`：提供方实现需要满足的 `Provider` trait
//!   以及请求前后的 `ProviderHooks` 钩子；
//! - `error`：本 crate 的错误构造工具。
//!
//! 当前模块均为私有，属于开发中的脚手架；对外 API 稳定后再按需提升可见性。

mod context;
pub mod error;
mod model;
mod options;
mod provider;
mod stream;

pub use self::{context::*, model::*, options::*, provider::*, stream::*};
