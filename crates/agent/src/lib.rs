//! ra-agent：Agent 运行时核心。
//!
//! 实现事件驱动的 Agent 主循环，完整覆盖「LLM 调用 → 工具调用收集 →
//! 工具执行 → 结果回灌 → 终止判定」的核心循环，并配套：
//!
//! - [`agent_loop`]：主循环与流式折叠、工具批执行（并行/顺序/截断失败）；
//! - [`queue`]：steering / follow-up 待注入消息 FIFO（`All` / 单条排水）；
//! - [`abort`]：`CancellationToken` 树形取消句柄，贯穿 LLM 流与工具执行；
//! - [`session`]：内存会话容器（转录、串行锁、取消根、过期清理）；
//! - [`events`] / [`emit`]：生命周期事件定义与广播 / 收集实现；
//! - [`adapter`]：ra-tools `ToolManager` 到 [`traits::ToolProvider`] 的适配；
//! - [`agent`]：`BaseAgent` 门面（组装依赖、重入拒绝、注入与取消入口）。
//!
//! 消息模型全链路复用
//! ra-core，LLM 边界直持 `Arc<Gateway>`，工具边界经 ra-tools
//! `ToolManager` 并以错误回灌，错误统一 `RiboError`
//! （`domain = "agent"`）。

#[cfg(test)]
mod test_support;

pub mod abort;
pub mod adapter;
pub mod agent;
pub mod agent_loop;
pub mod emit;
pub mod error;
pub mod events;
pub mod queue;
pub mod session;
pub mod traits;
pub mod types;
