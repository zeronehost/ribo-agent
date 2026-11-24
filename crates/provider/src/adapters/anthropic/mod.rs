//! Anthropic Claude 适配器骨架（阶段 1）。
//!
//! 本模块将 Anthropic Messages API 的线格式（`/v1/messages` + SSE 流）
//! 折叠为统一的 [`ProviderEvent`](crate::stream::ProviderEvent) 事件模型。
//!
//! # 架构
//!
//! - [`provider`] — `AnthropicProvider` 结构体 + [`Provider`](crate::traits::Provider) trait 实现，
//!   负责 PreStream 鉴权、HTTP 请求构建、SSE 解码任务启动。
//! - [`build_params`] — 把 [`Context`](crate::context::Context) 折叠为 Anthropic 请求 JSON body
//!   （messages / tools / tool_choice / system）。
//! - [`sse`] — Anthropic 方言的帧分派（`message_*` / `content_block_*` →
//!   `ProviderEvent`）与流内错误码表；解析任务骨架复用
//!   [`spawn_sse_task`](crate::adapters::parse_task::spawn_sse_task)。
//!
//! # 设计原则
//!
//! - **文本优先**：阶段 1 仅处理 `text` 块 + `tool_use` / `tool_result` 块；
//!   `thinking` / `redacted_thinking` / `image` 等块静默丢弃并记 warn 日志。
//! - **同构 OpenAI**：StreamAggregator / LineFlow / map_stop_reason 复用 `crate::stream`，
//!   SSE 帧分派 / spawn_parse_task 模式同构 openai 适配器。
//! - **零重试**：适配器内部不做任何重试，由上层网关组合。

pub mod build_params;
pub mod provider;
pub mod sse;

pub use self::provider::AnthropicProvider;
