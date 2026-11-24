//! OpenAI Responses API（`POST /responses`）适配。
//!
//! 与同目录的 Chat Completions 适配并列，复用同一条 `Provider` 契约，
//! 但在线格式上完全独立：
//!
//! - [`provider`]：`OpenAiResponsesAdapter`，`Provider` 实现（薄壳）；
//! - `build_params`：`Context` + `StreamOptions` → Responses 请求体；
//! - `sse`：Responses 事件流 → 统一事件模型（复用 `openai::sse` 传输层）。
//!
//! 设计取舍：
//!
//! 1. **事件优先**：每个 SSE 帧都带 `event:` 名，分派以事件名为准；
//! 2. **无哨兵**：正常终结靠 `response.completed` / `response.incomplete`，
//!    提前 EOF 由聚合器的截断检测（R2）收敛为 `Error`；
//! 3. **零重试**：适配器内部不重试，重试交由上层网关（与 Chat 适配一致）；
//! 4. **文本优先**：阶段 1 仅支持文本与工具调用，思考块丢弃并记 `warn`。

mod build_params;
mod provider;
mod sse;

pub use self::provider::OpenAiResponsesAdapter;
