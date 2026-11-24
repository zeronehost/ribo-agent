//! 服务商线格式适配器：把各家私有协议折叠为统一事件模型。
//!
//! 各适配器只依赖契约层（[`crate::traits`] / [`crate::stream`] /
//! [`crate::error`]），内部零重试（依赖单向原则）——重试由
//! 上层调用方或网关组合。

mod common;
mod parse_task;

pub mod anthropic;
pub mod mock;
pub mod openai;
