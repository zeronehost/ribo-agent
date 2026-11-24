//! OpenAI 适配器。
//!
//! 两条互不相干的线格式各占一个子树：
//!
//! - [`completion`]：Chat Completions（`POST /chat/completions`）；
//! - [`responses`]：Responses API（`POST /responses`）。
//!
//! 每个子树内 `Provider` 实现（薄壳）对外公开，`build_params`（请求体
//! 构造）与 `sse`（传输层 + 帧分派 + 事件聚合）为模块私有。
//! Responses 子树复用本层的 SSE 传输解析与错误码映射（见 `sse`）。

pub mod completion;
pub mod responses;

mod sse;
