//! 工具管理器：注册表 + Schema 校验缓存 + 整体熔断的编排层。

use std::{
  collections::HashMap,
  sync::{Arc, RwLock},
  time::Duration,
};

use ra_core::{
  circuit::{Admission, CircuitBreaker},
  error::RiboResult,
  message::{ToolCall, ToolSchema},
};

use crate::{
  context::ToolContext,
  error::{CANCELLED, cancelled, not_found, rate_limited},
  registry::ToolRegistry,
  schema::{self, CompiledValidator},
  traits::AgentTool,
  types::ToolExecutionResult,
};

/// 工具管理器配置。
pub struct ToolManagerConfig {
  /// 熔断阈值：整个工具面连续失败达到该值即打开熔断。
  pub failure_threshold: u64,
  /// 熔断打开后的冷却时长，期满进入半开放行单个探测。
  pub cooldown: Duration,
}

impl Default for ToolManagerConfig {
  /// 默认：连续失败 5 次熔断，冷却 30 秒。
  fn default() -> Self {
    Self {
      failure_threshold: 5,
      cooldown: Duration::from_secs(30),
    }
  }
}

/// 工具管理器：工具调用的统一入口。
///
/// 在 [`ToolRegistry`] 之上叠加两件事：
///
/// - **Schema 校验缓存**：注册期把每个工具的入参声明编译为
///   [`CompiledValidator`] 并缓存，执行期按工具名取用，避免重复编译；
/// - **熔断记账**：整个工具面共享一个 [`CircuitBreaker`]，工具连续
///   失败达到阈值后拒绝后续调用（`tool.rate_limited`），保护下游系统。
///
/// 取消语义：执行与取消信号竞争（`tokio::select!`），取消不计入
/// 熔断失败——取消既非成功也非业务失败（05 §5.2）。
pub struct ToolManager {
  /// 工具注册表。
  registry: ToolRegistry,
  /// 工具面整体熔断器。
  circuit: Arc<CircuitBreaker>,
  /// 工具名 → 编译期校验器缓存（读多写少，注册/注销时写）。
  validators: RwLock<HashMap<String, CompiledValidator>>,
}

impl Default for ToolManager {
  /// 等价于 [`ToolManager::new`]（默认配置）。
  fn default() -> Self {
    Self::new()
  }
}

impl ToolManager {
  /// 以默认配置创建管理器（连续失败 5 次熔断，冷却 30 秒）。
  pub fn new() -> Self {
    Self::with_config(ToolManagerConfig::default())
  }

  /// 以指定配置创建管理器。
  ///
  /// # 参数
  ///
  /// - `config`：熔断阈值与冷却时长，见 [`ToolManagerConfig`]。
  pub fn with_config(config: ToolManagerConfig) -> Self {
    Self {
      registry: ToolRegistry::default(),
      circuit: Arc::new(CircuitBreaker::new(
        config.failure_threshold,
        config.cooldown,
      )),
      validators: RwLock::new(HashMap::new()),
    }
  }

  /// 注册工具：先编译其入参 Schema，再入注册表并缓存校验器。
  ///
  /// Schema 非法时整体失败——带病注册的工具会在首次执行时才暴露
  /// 校验问题，注册期拦截能把错误前移到部署阶段。
  ///
  /// # Errors
  ///
  /// - 入参 Schema 非法 → `tool.invalid_arguments`；
  /// - 同名工具已注册 → `tool.already_registered`（此时校验器不缓存）。
  ///
  /// # Panics
  ///
  /// 校验器缓存写锁中毒时 panic（持锁期间发生 panic 才会中毒，
  /// 此时管理器内部状态已无法保证一致）。
  pub fn register(&self, tool: Arc<dyn AgentTool>) -> RiboResult<()> {
    let name = tool.schema().name.clone();

    // ===== 阶段 1：Schema 编译（失败则拒绝注册） =====
    let validator = schema::compile_validator(&tool.schema().input_schema)
      .map_err(|e| {
        tracing::warn!(
          tool = %name,
          error = %e,
          "tool registration failed: invalid input schema");
        e
      })?;

    // ===== 阶段 2：入注册表 + 缓存校验器 =====
    self.registry.registry(tool)?;
    self
      .validators
      .write()
      .expect("validator lock poisoned")
      .insert(name, validator);

    Ok(())
  }

  /// 注销工具并清除其校验器缓存。
  ///
  /// # Errors
  ///
  /// 工具不存在 → `tool.not_found`（此时缓存无需清理）。
  ///
  /// # Panics
  ///
  /// 校验器缓存写锁中毒时 panic（同 [`ToolManager::register`]）。
  pub fn unregister(&self, name: &str) -> RiboResult<()> {
    self.registry.unregister(name)?;
    self
      .validators
      .write()
      .expect("validator lock poisoned")
      .remove(name);
    Ok(())
  }

  /// 按名称取工具实例。
  pub fn get(&self, name: &str) -> Option<Arc<dyn AgentTool>> {
    self.registry.get(name)
  }

  /// 取全部工具实例（顺序不定）。
  pub fn list(&self) -> Vec<Arc<dyn AgentTool>> {
    self.registry.list()
  }

  /// 取全部工具声明（按名称排序，供组装模型请求的 tools 段）。
  pub fn get_tool_schemas(&self) -> Vec<ToolSchema> {
    self.registry.schemas()
  }

  /// 执行一次工具调用：准入 → 取工具 → 预处理 → 校验 → 执行 → 记账。
  ///
  /// 处理流程：
  ///
  /// 1. 熔断准入：打开即拒绝（`tool.rate_limited`）；半开放行的
  ///    探测请求在终结时回报结果；
  /// 2. 参数预处理：`prepare_arguments` 的 `Err` 不计熔断失败
  ///    （入参问题由模型修正，非工具故障）；
  /// 3. Schema 校验：缓存命中才校验；缓存未命中仅告警跳过
  ///    （防御注册链路与缓存不同步的缺陷，不阻断执行）；
  /// 4. 执行与取消竞争：取消优先于完成，二者均不计熔断失败；
  /// 5. 终结记账：成功闭合熔断计数；业务失败计入并原样透传错误
  ///    （D8：上层转为 `is_error` 结果回灌模型）。
  ///
  /// # Errors
  ///
  /// - 熔断打开 → `tool.rate_limited`；
  /// - 工具未注册 → `tool.not_found`；
  /// - 预处理 / 校验失败 → `tool.invalid_arguments`；
  /// - 被取消 → `tool.cancelled` 或工具自身的取消错误码；
  /// - 执行失败 → 工具错误原样透传。
  ///
  /// # Panics
  ///
  /// 校验器缓存读锁中毒时 panic（同 [`ToolManager::register`]）。
  pub async fn execute(
    &self,
    call: &ToolCall,
    ctx: &ToolContext,
  ) -> RiboResult<ToolExecutionResult> {
    let name = call.name.as_str();

    // ===== 阶段 1：熔断准入 =====
    // probe 标记本请求是否为半开探测：终结时需回报，取消时需归还资格。
    let probe = match self.circuit.admit() {
      Admission::Allow => false,
      Admission::AllowProbe => true,
      Admission::Reject => {
        tracing::warn!(tool = %name, "tool execution rejected: circuit open");
        return Err(rate_limited(name));
      }
    };

    // ===== 阶段 2：取工具（未注册不记账，属用法错误） =====
    let Some(tool) = self.registry.get(name) else {
      return Err(not_found(name));
    };

    // ===== 阶段 3：参数预处理（入参问题不计熔断失败） =====
    // 预处理失败属入参错误：记 warn 供排查，但不计入熔断失败
    let args =
      tool
        .prepare_arguments(call.arguments.clone())
        .inspect_err(|e| {
          tracing::warn!(
            tool = %name, code = %e.code(),
            "tool prepare_arguments failed"
          );
        })?;

    // ===== 阶段 4：Schema 校验（读锁内取缓存副本，缩小临界区） =====
    let validator = {
      let guard = self.validators.read().expect("validator lock poisoned");
      guard.get(name).cloned()
    };
    if let Some(v) = &validator {
      // 校验失败同样属入参错误：warn 记录后原样返回，不计熔断
      schema::validate_arguments(name, v, &args).inspect_err(|e| {
        tracing::warn!(
          tool = %name, code = %e.code(),
          "tool arguments failed schema validation"
        );
      })?;
    } else {
      tracing::warn!(tool = %name, "validator cache miss, skip schema validation");
    }

    // ===== 阶段 5：执行与取消竞争 =====
    // biased 保证取消信号优先被轮询；工具实例克隆进 select 分支，
    // 与管理器生命周期解耦。
    let tool = tool.clone();
    let ctx_signal = ctx.signal().clone();
    let outcome = tokio::select! {
      biased;
      _ = ctx_signal.cancelled() => None,
      result = tool.execute(&call.id, args, ctx) => Some(result),
    };

    // ===== 阶段 6：终结记账 =====
    match outcome {
      Some(Ok(result)) => {
        self.circuit.record_success();
        tracing::info!(tool = %name, call_id = %call.id, "tool executed");
        Ok(result)
      }
      Some(Err(e)) if e.code() == CANCELLED => {
        // 取消不是业务失败：只归还探测资格，不污染熔断统计（05 §5.2）。
        if probe {
          self.circuit.abort_probe();
        }
        tracing::warn!(tool = %name, call_id = %call.id, "tool execution cancelled");
        Err(e)
      }
      Some(Err(e)) => {
        self.circuit.record_failure();
        tracing::error!(tool = %name, call_id = %call.id, code = %e.code(), "tool execution failed");
        Err(e) // 错误原样透传（D8：上层转为 is_error 结果回灌）
      }
      None => {
        // select 中取消先到：归还探测资格并合成取消错误。
        if probe {
          self.circuit.abort_probe();
        }
        tracing::warn!(tool = %name, call_id = %call.id, "tool execution cancelled");
        Err(cancelled(name))
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use std::{sync::Arc, time::Duration};

  use ra_core::message::{Schema, TextContent, ToolCall, ToolSchema};
  use serde_json::json;
  use tokio_util::sync::CancellationToken;

  use super::*;
  use crate::{
    error::{
      ALREADY_REGISTERED, CANCELLED, EXEC_FAILED, INVALID_ARGUMENTS, NOT_FOUND,
      RATE_LIMITED, exec_failed, invalid_arguments,
    },
    types::ToolOutput,
  };

  // ===== 测试辅助 =====

  /// 测试工具的执行行为。
  #[derive(Clone, Copy)]
  enum Behavior {
    /// execute 返回成功文本 "ok"。
    Succeed,
    /// execute 返回业务失败（`tool.exec_failed`）。
    Fail,
  }

  /// 可控测试工具：入参声明必填字符串字段 `x`，execute 按注入的
  /// 行为返回，可开启 prepare_arguments 失败注入。
  struct TestTool {
    /// 构造期固化的工具声明。
    schema: ToolSchema,
    /// execute 行为。
    behavior: Behavior,
    /// 是否注入 prepare_arguments 失败。
    prepare_fails: bool,
  }

  impl TestTool {
    /// 构造可控测试工具并装箱为 trait 对象。
    fn boxed(
      name: &str,
      behavior: Behavior,
      prepare_fails: bool,
    ) -> Arc<dyn AgentTool> {
      Arc::new(Self {
        schema: ToolSchema {
          name: name.to_string(),
          description: "controllable test tool".to_string(),
          input_schema: Schema(json!({
            "type": "object",
            "properties": { "x": { "type": "string" } },
            "required": ["x"],
            "additionalProperties": false
          })),
          constrained_sampling: None,
        },
        behavior,
        prepare_fails,
      })
    }

    /// 可控行为工具（参数预处理正常透传）。
    fn controlled(name: &str, behavior: Behavior) -> Arc<dyn AgentTool> {
      Self::boxed(name, behavior, false)
    }

    /// 注入 prepare_arguments 失败的工具。
    fn prepare_failing(name: &str) -> Arc<dyn AgentTool> {
      Self::boxed(name, Behavior::Succeed, true)
    }
  }

  #[async_trait::async_trait]
  impl AgentTool for TestTool {
    fn schema(&self) -> &ToolSchema {
      &self.schema
    }

    fn label(&self) -> &str {
      "Test"
    }

    fn prepare_arguments(
      &self,
      args: serde_json::Value,
    ) -> RiboResult<serde_json::Value> {
      // 注入预处理失败：模拟入参形态问题（语义上不应计入熔断记账）
      if self.prepare_fails {
        return Err(invalid_arguments(
          self.schema.name.as_str(),
          "injected prepare failure",
        ));
      }
      Ok(args)
    }

    async fn execute(
      &self,
      _tool_call_id: &str,
      _args: serde_json::Value,
      _ctx: &ToolContext,
    ) -> RiboResult<ToolExecutionResult> {
      match self.behavior {
        Behavior::Succeed => Ok(ToolExecutionResult {
          content: vec![ToolOutput::Text(TextContent {
            content: "ok".to_string(),
            signature: None,
          })],
          ..Default::default()
        }),
        Behavior::Fail => Err(exec_failed(
          self.schema.name.as_str(),
          "injected business failure",
        )),
      }
    }
  }

  /// 构造一次工具调用（默认入参为合法字符串字段 `x`）。
  fn call(name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
      id: "call-1".to_string(),
      name: name.to_string(),
      arguments: args,
      signature: None,
      namespace: None,
    }
  }

  /// 小阈值熔断配置：threshold 可调，冷却 20ms（用真实短睡眠推进）。
  fn config(threshold: u64) -> ToolManagerConfig {
    ToolManagerConfig {
      failure_threshold: threshold,
      cooldown: Duration::from_millis(20),
    }
  }

  /// 取结果首个文本块内容。
  fn first_text(result: &ToolExecutionResult) -> &str {
    match &result.content[0] {
      ToolOutput::Text(t) => &t.content,
      ToolOutput::Image(_) => panic!("expected text output"),
    }
  }

  /// 构造已取消的执行上下文。
  fn cancelled_ctx() -> ToolContext {
    let token = CancellationToken::new();
    token.cancel();
    ToolContext::new(token, Arc::new(|_| {}))
  }

  // ===== 用例 =====

  /// 场景：execute 成功路径——返回工具产出内容，多次成功后仍可调用。
  ///
  /// 预期：返回内容为工具文本 "ok"；熔断保持闭合（可观察行为：
  /// 连续成功调用均不被 `tool.rate_limited` 拒绝）。
  #[tokio::test]
  async fn execute_success_returns_content_and_keeps_circuit_closed() {
    let manager = ToolManager::new();
    manager
      .register(TestTool::controlled("calm", Behavior::Succeed))
      .expect("register calm tool");

    let result = manager
      .execute(&call("calm", json!({ "x": "v" })), &ToolContext::detached())
      .await
      .expect("first execution should succeed");
    assert_eq!(first_text(&result), "ok");

    // 连续多次成功：若熔断被误记账，这里会以 rate_limited 失败
    for _ in 0..5 {
      manager
        .execute(&call("calm", json!({ "x": "v" })), &ToolContext::detached())
        .await
        .expect("circuit should stay closed on success");
    }
  }

  /// 场景：调用未注册的工具。
  ///
  /// 预期：返回 `tool.not_found`。
  #[tokio::test]
  async fn unregistered_tool_returns_not_found() {
    let manager = ToolManager::new();

    let err = manager
      .execute(&call("ghost", json!({})), &ToolContext::detached())
      .await
      .unwrap_err();
    assert_eq!(err.code(), NOT_FOUND);
  }

  /// 场景：入参错误不计熔断记账——prepare_arguments 失败与 Schema
  /// 校验失败（传错类型参数）连续多次且超过熔断阈值。
  ///
  /// 预期：每次都返回 `tool.invalid_arguments`；熔断器保持闭合，
  /// 之后的合法调用正常成功。
  #[tokio::test]
  async fn invalid_arguments_do_not_trip_breaker() {
    // 阈值 2：连续 3 次入参错误若被记账必然打开熔断
    let manager = ToolManager::with_config(config(2));
    manager
      .register(TestTool::prepare_failing("prep_fail"))
      .expect("register prep_fail tool");
    manager
      .register(TestTool::controlled("typed", Behavior::Succeed))
      .expect("register typed tool");

    // ===== 阶段 1：预处理失败连续 3 次（超过阈值 2） =====
    for _ in 0..3 {
      let err = manager
        .execute(
          &call("prep_fail", json!({ "x": "v" })),
          &ToolContext::detached(),
        )
        .await
        .unwrap_err();
      assert_eq!(err.code(), INVALID_ARGUMENTS);
    }

    // ===== 阶段 2：Schema 校验失败（类型错误）连续 3 次 =====
    for _ in 0..3 {
      let err = manager
        .execute(
          &call("typed", json!({ "x": 123 })),
          &ToolContext::detached(),
        )
        .await
        .unwrap_err();
      assert_eq!(err.code(), INVALID_ARGUMENTS);
    }

    // ===== 阶段 3：熔断仍闭合——合法调用成功 =====
    let result = manager
      .execute(
        &call("typed", json!({ "x": "v" })),
        &ToolContext::detached(),
      )
      .await
      .expect("circuit should stay closed: argument errors are not counted");
    assert_eq!(first_text(&result), "ok");
  }

  /// 场景：执行失败连续达到阈值 → 熔断打开，后续请求被拒。
  ///
  /// 预期：前 2 次失败原样透传 `tool.exec_failed`；之后包括成功
  /// 工具在内的所有请求都返回 `tool.rate_limited`（整个工具面共享熔断）。
  #[tokio::test]
  async fn consecutive_failures_open_circuit_and_reject() {
    let manager = ToolManager::with_config(config(2));
    manager
      .register(TestTool::controlled("boom", Behavior::Fail))
      .expect("register boom tool");
    manager
      .register(TestTool::controlled("calm", Behavior::Succeed))
      .expect("register calm tool");
    let ctx = ToolContext::detached();

    // ===== 阶段 1：两次业务失败达到阈值 =====
    for _ in 0..2 {
      let err = manager
        .execute(&call("boom", json!({ "x": "v" })), &ctx)
        .await
        .unwrap_err();
      assert_eq!(err.code(), EXEC_FAILED);
    }

    // ===== 阶段 2：冷却期内一律拒绝（含原本会成功的工具） =====
    for name in ["boom", "calm"] {
      let err = manager
        .execute(&call(name, json!({ "x": "v" })), &ctx)
        .await
        .unwrap_err();
      assert_eq!(err.code(), RATE_LIMITED);
    }
  }

  /// 场景：半开恢复——冷却期满后放行探测，探测成功闭合熔断。
  ///
  /// 预期：冷却期后首个请求被放行并成功，熔断恢复闭合，后续请求正常。
  #[tokio::test]
  async fn half_open_probe_success_recovers_circuit() {
    let manager = ToolManager::with_config(config(2));
    manager
      .register(TestTool::controlled("boom", Behavior::Fail))
      .expect("register boom tool");
    manager
      .register(TestTool::controlled("calm", Behavior::Succeed))
      .expect("register calm tool");
    let ctx = ToolContext::detached();

    // ===== 阶段 1：打满失败 → 熔断打开 =====
    for _ in 0..2 {
      manager
        .execute(&call("boom", json!({ "x": "v" })), &ctx)
        .await
        .unwrap_err();
    }

    // ===== 阶段 2：等待冷却期满（20ms 冷却，睡 60ms）进入半开 =====
    tokio::time::sleep(Duration::from_millis(60)).await;

    // ===== 阶段 3：半开探测成功 → 闭合 =====
    let result = manager
      .execute(&call("calm", json!({ "x": "v" })), &ctx)
      .await
      .expect("probe should be admitted after cooldown and succeed");
    assert_eq!(first_text(&result), "ok");

    // ===== 阶段 4：恢复闭合后继续正常放行 =====
    manager
      .execute(&call("calm", json!({ "x": "v" })), &ctx)
      .await
      .expect("circuit should be closed after successful probe");
  }

  /// 场景：半开探测失败 → 重新打开熔断。
  ///
  /// 预期：探测失败原样透传 `tool.exec_failed`，随后的请求在新的
  /// 冷却期内被 `tool.rate_limited` 拒绝。
  #[tokio::test]
  async fn half_open_probe_failure_reopens_circuit() {
    let manager = ToolManager::with_config(config(2));
    manager
      .register(TestTool::controlled("boom", Behavior::Fail))
      .expect("register boom tool");
    manager
      .register(TestTool::controlled("calm", Behavior::Succeed))
      .expect("register calm tool");
    let ctx = ToolContext::detached();

    // ===== 阶段 1：打满失败 → 打开 → 冷却期满进入半开 =====
    for _ in 0..2 {
      manager
        .execute(&call("boom", json!({ "x": "v" })), &ctx)
        .await
        .unwrap_err();
    }
    tokio::time::sleep(Duration::from_millis(60)).await;

    // ===== 阶段 2：探测失败 → 立即重新打开 =====
    let err = manager
      .execute(&call("boom", json!({ "x": "v" })), &ctx)
      .await
      .unwrap_err();
    assert_eq!(err.code(), EXEC_FAILED);

    // ===== 阶段 3：新冷却期内拒绝一切请求 =====
    let err = manager
      .execute(&call("calm", json!({ "x": "v" })), &ctx)
      .await
      .unwrap_err();
    assert_eq!(err.code(), RATE_LIMITED);
  }

  /// 场景：ctx 已取消 → 返回 `tool.cancelled` 且不计熔断失败。
  ///
  /// 预期：阈值为 1 时，一次取消后合法调用仍能成功（取消若被记账
  /// 为失败，熔断会打开并拒绝后续请求）。
  #[tokio::test]
  async fn cancelled_request_is_not_counted_as_failure() {
    let manager = ToolManager::with_config(config(1));
    manager
      .register(TestTool::controlled("calm", Behavior::Succeed))
      .expect("register calm tool");

    let err = manager
      .execute(&call("calm", json!({ "x": "v" })), &cancelled_ctx())
      .await
      .unwrap_err();
    assert_eq!(err.code(), CANCELLED);

    // 取消未记账：阈值 1 下若计入失败，这里会被 rate_limited 拒绝
    let result = manager
      .execute(&call("calm", json!({ "x": "v" })), &ToolContext::detached())
      .await
      .expect("cancellation must not count as circuit failure");
    assert_eq!(first_text(&result), "ok");
  }

  /// 场景：半开状态下的探测请求被取消 → 探测资格被归还。
  ///
  /// 预期：取消返回 `tool.cancelled`；随后新请求仍可获得探测资格
  /// 并成功闭合熔断（若资格未归还，新请求会被 `tool.rate_limited` 拒绝）。
  #[tokio::test]
  async fn cancelled_probe_returns_probe_eligibility() {
    let manager = ToolManager::with_config(config(1));
    manager
      .register(TestTool::controlled("boom", Behavior::Fail))
      .expect("register boom tool");
    manager
      .register(TestTool::controlled("calm", Behavior::Succeed))
      .expect("register calm tool");

    // ===== 阶段 1：单次失败即打开熔断（阈值 1） =====
    let err = manager
      .execute(&call("boom", json!({ "x": "v" })), &ToolContext::detached())
      .await
      .unwrap_err();
    assert_eq!(err.code(), EXEC_FAILED);

    // ===== 阶段 2：冷却期满进入半开，用已取消的 ctx 发探测请求 =====
    tokio::time::sleep(Duration::from_millis(60)).await;
    let err = manager
      .execute(&call("boom", json!({ "x": "v" })), &cancelled_ctx())
      .await
      .unwrap_err();
    assert_eq!(err.code(), CANCELLED);

    // ===== 阶段 3：探测资格已归还——新请求可探测并成功闭合 =====
    let result = manager
      .execute(&call("calm", json!({ "x": "v" })), &ToolContext::detached())
      .await
      .expect("probe eligibility should be returned after cancellation");
    assert_eq!(first_text(&result), "ok");
  }

  /// 场景：重复注册同名工具。
  ///
  /// 预期：第二次注册返回 `tool.already_registered`。
  #[tokio::test]
  async fn duplicate_register_is_rejected() {
    let manager = ToolManager::new();
    manager
      .register(TestTool::controlled("dup", Behavior::Succeed))
      .expect("first registration");

    let err = manager
      .register(TestTool::controlled("dup", Behavior::Succeed))
      .unwrap_err();
    assert_eq!(err.code(), ALREADY_REGISTERED);
  }

  /// 场景：注销后执行。
  ///
  /// 预期：unregister 成功后 execute 返回 `tool.not_found`。
  #[tokio::test]
  async fn execute_after_unregister_returns_not_found() {
    let manager = ToolManager::new();
    manager
      .register(TestTool::controlled("gone", Behavior::Succeed))
      .expect("register gone tool");
    manager.unregister("gone").expect("unregister gone tool");

    let err = manager
      .execute(&call("gone", json!({ "x": "v" })), &ToolContext::detached())
      .await
      .unwrap_err();
    assert_eq!(err.code(), NOT_FOUND);
  }
}
