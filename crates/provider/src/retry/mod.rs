//! 请求级重试设施。
//!
//! 由四个子模块组成：
//!
//! - `policy`：重试策略——指数退避曲线、按错误判定是否重试、按请求选项覆盖参数；
//! - `backoff`：退避休眠（可被取消）与 `retry_after` 解析；
//! - `first_token`：首包超时守卫——建流成功后先窥视首个内容事件再交付；
//! - `retrying`：把上述能力组合为自带重试的 `Provider` 包装器。
//!
//! 模块遵循 Provider 契约：只有 PreStream 阶段的错误才参与重试判定，
//! 流内错误一律原样上抛。

mod backoff;
mod first_token;
mod policy;
mod retrying;

pub use self::{first_token::*, policy::*, retrying::*};
