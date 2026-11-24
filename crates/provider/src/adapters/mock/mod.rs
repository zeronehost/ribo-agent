//! 回放式 mock 适配器：测试与示例的默认提供方。
//!
//! 模块拆分：[`script`] 定义脚本化步骤、[`recorder`] 定义
//! 调用记录、[`provider`] 承载 [`MockProvider`]（`Provider` 实现 +
//! 后台回放任务），全部公开类型经本模块再导出。
//!
//! # 契约
//!
//! mock 与真实适配器共享同一条终结纪律：`Start` 自动补发、
//! 终结恰好一次、`push` 返回 `false` 时静默退出——因此针对 mock 写的
//! 消费端代码无须修改即可直接用于真实适配器。
//!
//! # 示例
//!
//! ```rust
//! use ra_provider::adapters::mock::{MockProvider, MockStep};
//! use ra_provider::ids::{ModelId, ProviderId};
//! use ra_provider::model::Model;
//! use ra_provider::traits::Provider;
//!
//! tokio::runtime::Runtime::new().unwrap().block_on(async {
//!   let provider = ProviderId::parse("mock").expect("valid provider id");
//!   let model = Model::new(
//!     provider.clone(),
//!     ModelId::parse("mock-model").expect("valid model id"),
//!     "Mock Model",
//!     "http://localhost",
//!   );
//!   let mock = MockProvider::new(provider, vec![model], Vec::new());
//!
//!   // 脚本耗尽：建流成功，但流以 INVALID_REQUEST 错误终结（不 panic）
//!   let options = ra_provider::options::ProviderOptions {
//!     model: &mock.models()[0],
//!     context: &ra_provider::context::Context::default(),
//!     options: &ra_provider::options::StreamOptions::default(),
//!   };
//!   let stream = mock.stream(options).await.expect("建流成功");
//! });
//! ```

mod provider;
mod recorder;
mod script;

pub use provider::MockProvider;
pub use recorder::MockCall;
pub use script::MockStep;
