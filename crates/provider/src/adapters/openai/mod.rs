//! OpenAI 兼容适配器。
//!
//! 模块拆分：[`completion`] 承载 `Provider` 实现（薄壳），
//! `build_params` 管请求体构造，`sse` 管流式解析（传输层 + 帧分派 +
//! 事件聚合）。后两者为模块私有，仅经 `completion` 链路触达。

pub mod completion;

mod build_params;
mod sse;
