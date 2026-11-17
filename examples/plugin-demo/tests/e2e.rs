//! 端到端集成测试（18_ §13.2，方式 B「预编译 wasm」）：
//!
//! 1. 以 `cargo build --target wasm32-unknown-unknown` 构建 PDK guest；
//! 2. `WasmPluginHost::load` 加载 manifest（真实 PDK 宏导出 + ABI v0）；
//! 3. `PluginRegistry` 原子注册 → `ToolManager` 全链路执行；
//! 4. 验证 config 信封、storage 跨 execute 持久、epoch 超时与
//!    熔断白名单（§1.3 决策 1）。
//!
//! 运行方式：本 crate 已是根 workspace 成员（根 `Cargo.toml` 的
//! `members = ["crates/*", "examples/*", ...]`），可直接
//! `cargo test -p plugin-demo`；`cargo test --workspace` 亦会覆盖本套件。
//!
//! 前置：`rustup target add wasm32-unknown-unknown`（CI 已安装）。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use ra_agent::events::AgentEvent;
use ra_cli::config::assemble::assemble_with_steps;
use ra_cli::config::{
  CompressionSchema, ConfigMode, HarnessLayerSchema, LoadOutcome,
  PluginAclSchema, PluginEntrySchema, PluginVerifySchema, PluginsSchema,
  ProviderSchema, RetrySchema, SessionSchema, ToolsSchema,
};
use ra_core::message::ToolCall;
use ra_core::message::{
  AssistantMessage, Content, Message, StopReason, TextContent,
};
use ra_plugins::acl::PluginAcl;
use ra_plugins::acl::UrlPolicy;
use ra_plugins::authority::AuthorityManifest;
use ra_plugins::registry::{PluginRegistry, plugin_aware_tool_manager_config};
use ra_plugins::wasm::host::WasmPluginHost;
use ra_provider::adapters::mock::MockStep;
use ra_provider::context::Context;
use ra_tools::context::ToolContext;
use ra_tools::manager::ToolManager;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// 构建 guest wasm 并读取字节（§13.4 方式 B）。
///
/// 构建失败直接 panic：集成测试的前提就是 guest 可构建，失败信息
/// 附 stderr 便于定位（编译错误 / 缺 wasm32 target）。
fn build_guest_wasm() -> Vec<u8> {
  let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
  let cargo = env!("CARGO");

  let output = Command::new(cargo)
    .arg("build")
    .arg("--manifest-path")
    .arg(&manifest)
    .arg("--target")
    .arg("wasm32-unknown-unknown")
    .output()
    .expect("spawn cargo build for guest");

  assert!(
    output.status.success(),
    "guest build failed:\n{}",
    String::from_utf8_lossy(&output.stderr)
  );

  // 本 crate 为根 workspace 成员，cargo build 的产物落在 workspace 根的
  // target 目录（而非 crate 内）——CARGO_MANIFEST_DIR 上溯两级即根。
  let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .and_then(Path::parent)
    .expect("examples/plugin-demo must live under workspace root");
  let wasm: PathBuf = workspace_root
    .join("target")
    .join("wasm32-unknown-unknown")
    .join("debug")
    .join("plugin_demo.wasm");
  std::fs::read(&wasm)
    .unwrap_or_else(|e| panic!("read guest artifact {}: {e}", wasm.display()))
}

/// 构造一次 `ToolCall`。
fn call(name: &str, arguments: Value) -> ToolCall {
  ToolCall {
    id: format!("call-{name}"),
    name: name.to_string(),
    arguments,
    signature: None,
    namespace: None,
  }
}

/// 场景 1（§14 DoD 1 / 2 的最小闭环）：加载 → 注册 → 全链路执行
/// echo，插件工具以 `demo__echo` 之名对主循环可见；greet 验证
/// config 信封注入与 storage 跨 execute 持久（§13.1 三态回归）。
#[tokio::test]
async fn demo_plugin_loads_registers_and_executes() {
  let wasm = build_guest_wasm();
  let host = WasmPluginHost::new().expect("wasm host init");

  // ACL：log + storage 开启（最小权限，http 关闭）
  let acl = PluginAcl {
    log: true,
    storage: true,
    ..PluginAcl::default()
  };
  let plugin = host
    .load(&wasm, Some(json!({ "greeting_prefix": "你好" })), &acl)
    .expect("load demo plugin");
  assert_eq!(plugin.id(), "demo");

  // manifest 来自真实 PDK 宏导出（list_tools ABI v0）
  let mut tool_names: Vec<String> =
    plugin.tools().into_iter().map(|m| m.name).collect();
  tool_names.sort();
  assert_eq!(tool_names, vec!["echo", "fetch", "greet", "spin"]);

  // 注册（原子批量注入）+ 前缀可见性
  let manager =
    Arc::new(ToolManager::with_config(plugin_aware_tool_manager_config()));
  let registry = PluginRegistry::new(Arc::clone(&manager));
  registry.register(plugin).expect("register demo plugin");

  let mut schemas: Vec<String> = manager
    .get_tool_schemas()
    .into_iter()
    .map(|s| s.name)
    .collect();
  schemas.sort();
  assert_eq!(
    schemas,
    vec!["demo__echo", "demo__fetch", "demo__greet", "demo__spin"]
  );

  // echo 全链路：ToolManager → PluginAgentTool → run_tool → guest
  let result = manager
    .execute(
      &call("demo__echo", json!({ "x": 1 })),
      &ToolContext::detached(),
    )
    .await
    .expect("echo executes");
  assert_eq!(result.details, Some(json!({ "x": 1 })));

  // greet ①：首次调用（key 不存在 → seen=0，config 前缀生效）
  let r1 = manager
    .execute(
      &call("demo__greet", json!({ "name": "a" })),
      &ToolContext::detached(),
    )
    .await
    .expect("greet executes (first)");
  assert_eq!(r1.details, Some(json!({ "message": "你好, a! seen=0" })));

  // greet ②：storage 跨 execute 持久（Stateless Store 下经
  // PluginShared 存活，§13.1「greet 首次 seen=0」三态回归）
  let r2 = manager
    .execute(
      &call("demo__greet", json!({ "name": "a" })),
      &ToolContext::detached(),
    )
    .await
    .expect("greet executes (second)");
  assert_eq!(r2.details, Some(json!({ "message": "你好, a! seen=1" })));
}

/// 场景 2（§14 DoD 2）：死循环 guest 在 epoch 预算内被打断
/// （`plugin.plugin_timeout`）；装配白名单下连续超时**不**打开
/// 工具面熔断（第 7 次仍为超时而非 `tool.rate_limited`，§1.6 验收）。
#[tokio::test]
async fn spin_times_out_without_opening_circuit() {
  let wasm = build_guest_wasm();
  let host = WasmPluginHost::new().expect("wasm host init");

  let acl = PluginAcl::default(); // spin 不需要任何 IO 能力
  let plugin = host.load(&wasm, None, &acl).expect("load demo plugin");

  let manager =
    Arc::new(ToolManager::with_config(plugin_aware_tool_manager_config()));
  let registry = PluginRegistry::new(Arc::clone(&manager));
  registry.register(plugin).expect("register demo plugin");

  for i in 0..7 {
    let err = manager
      .execute(&call("demo__spin", json!({})), &ToolContext::detached())
      .await
      .expect_err("spin must time out");
    assert_eq!(
      err.code(),
      "plugin.plugin_timeout",
      "iteration {i}: {}",
      err.code()
    );
  }
}

/// 场景 3（§14 DoD 3「拒绝越权」）：ACL 未授予 http 能力时，guest 的
/// `ribo::http_get` 在 host function 入口被拦（trap `forbidden:http` →
/// `plugin.plugin_forbidden`，不计熔断白名单）。CI 安全：无任何网络
/// 请求发出即被拒绝。
#[tokio::test]
async fn http_get_denied_without_http_acl() {
  let wasm = build_guest_wasm();
  let host = WasmPluginHost::new().expect("wasm host init");

  // ACL：log 授予（echo 需要）、http 未授予——guest 的任何 http
  // 尝试都会在 host function 入口被拦
  let acl = PluginAcl {
    log: true,
    ..PluginAcl::default()
  };
  let plugin = host.load(&wasm, None, &acl).expect("load demo plugin");

  let manager =
    Arc::new(ToolManager::with_config(plugin_aware_tool_manager_config()));
  let registry = PluginRegistry::new(Arc::clone(&manager));
  registry.register(plugin).expect("register demo plugin");

  let err = manager
    .execute(
      &call("demo__fetch", json!({ "url": "https://example.com/x" })),
      &ToolContext::detached(),
    )
    .await
    .expect_err("http must be denied without ACL");
  assert_eq!(err.code(), "plugin.plugin_forbidden", "got: {}", err.code());
  // 越权错误不计熔断（§1.3 白名单）：连续越权后工具面仍可用
  let echo = manager
    .execute(
      &call("demo__echo", json!({ "ok": 1 })),
      &ToolContext::detached(),
    )
    .await;
  assert!(echo.is_ok(), "circuit must not open on forbidden errors");
}

/// 场景 4（B0.5，18_ §14 DoD #1）：CLI 装配层全链路——`plugins` 层
/// 配置驱动加载（`assemble` 内完成 fs 读取 → `WasmPluginHost::load`
/// → `PluginRegistry` 注册）→ mock provider 脚本发起 `demo__echo`
/// tool_call → 插件执行 → 结果回灌 → 最终回答。
///
/// 与场景 1-3 的差异：此处走的是 `ra_cli::config::assemble` 装配层
/// 与 `AgentLoop` 主循环（CLI / TUI 的实际入口），不是 `ToolManager`
/// 直调——即 DoD #1「Agent 识别意图 → 调插件 → LLM 回答」的最小闭环。
#[tokio::test]
async fn assemble_full_agent_loop_invokes_plugin_tool() {
  let wasm = build_guest_wasm();
  let dir = tempfile::tempdir().expect("temp dir");
  let root = dir.path().to_path_buf();
  std::fs::write(root.join("demo.wasm"), &wasm).expect("write wasm fixture");

  let outcome = LoadOutcome {
    provider: ProviderSchema::default(),
    retry: RetrySchema::default(),
    compression: CompressionSchema::default(),
    session: SessionSchema::default(),
    tools: ToolsSchema::default(),
    plugins: PluginsSchema {
      enabled: true,
      roots: Vec::new(),
      entries: vec![PluginEntrySchema {
        path: PathBuf::from("demo.wasm"),
        config: Some(json!({ "greeting_prefix": "你好" })),
        acl: PluginAclSchema {
          log: true,
          storage: true,
          ..PluginAclSchema::default()
        },
        manifest: None,
        provenance: None,
      }],
      verify: PluginVerifySchema::default(),
    },
    harness: HarnessLayerSchema {
      root: root.clone(),
      ..HarnessLayerSchema::default()
    },
    mode: ConfigMode::Mock,
    warnings: Vec::new(),
  };

  // 第 1 步：LLM 发起 demo__echo 调用（消息携带 ToolCall 内容块 +
  // stop_reason = ToolUse → 触发工具批）；第 2 步：消费回灌结果后收尾。
  let steps = vec![
    MockStep::Complete(Arc::new(|_ctx: &Context| {
      let mut msg = AssistantMessage::empty("mock", "mock-model", 0);
      msg.content = vec![Content::ToolCall(ToolCall {
        id: "call-echo".to_string(),
        name: "demo__echo".to_string(),
        arguments: json!({ "x": 1 }),
        signature: None,
        namespace: None,
      })];
      msg.stop_reason = StopReason::ToolUse;
      (StopReason::ToolUse, msg)
    })),
    MockStep::Complete(Arc::new(|_ctx: &Context| {
      let mut msg = AssistantMessage::empty("mock", "mock-model", 0);
      msg.content = vec![Content::Text(TextContent {
        content: "echo tool answered: 1".to_string(),
        signature: None,
      })];
      msg.stop_reason = StopReason::Stop;
      (StopReason::Stop, msg)
    })),
  ];

  let session = assemble_with_steps(&outcome, steps).expect("assemble");
  assert!(session.plugin_registry.is_some(), "registry retained");
  assert_eq!(session.loaded_plugins, vec!["demo"]);

  // 先订阅事件（harness 为 pub Arc），再发起回合；task 完成即全部
  // 事件已发射，broadcast 缓冲保证不丢。
  let mut events = session.harness.subscribe();
  let handle = session.spawn_turn("default", "please echo");
  handle.task.await.expect("turn task completes");

  let mut tool_ends: Vec<(String, bool)> = Vec::new();
  let mut final_texts: Vec<String> = Vec::new();
  while let Ok(event) = events.try_recv() {
    match event {
      AgentEvent::ToolExecutionEnd {
        tool_name,
        is_error,
        ..
      } => {
        tool_ends.push((tool_name, is_error));
      }
      AgentEvent::MessageEnd {
        message: Message::Assistant(m),
      } => {
        for block in &m.content {
          if let Content::Text(t) = block {
            final_texts.push(t.content.clone());
          }
        }
      }
      _ => {}
    }
  }
  assert_eq!(
    tool_ends,
    vec![("demo__echo".to_string(), false)],
    "plugin tool must execute once and succeed"
  );
  assert!(
    final_texts.iter().any(|t| t.contains("echo tool answered")),
    "final answer must follow the tool round: {final_texts:?}"
  );
}

// ===== B1.1 / B1.2 / B1.5 / B1.8：http-proxy 成功链路 / DoS 响应上限 /
// 并发取消（2026-11-16，18_ §13.1 核对表缺口）=====

/// 极简本地 HTTP mock（零新增依赖：std TcpListener）。
///
/// - 绑定 127.0.0.1:0，端口回读；
/// - accept 循环在独立 std 线程，测试进程退出即回收（不 join）；
/// - `delay_ms` 在读请求后、写响应前睡眠——构造「响应在途」供取消
///   场景（B1.5）即时打断。
fn spawn_http_mock(resp: Vec<u8>, delay_ms: u64) -> u16 {
  let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock");
  let port = listener.local_addr().expect("mock addr").port();
  std::thread::spawn(move || {
    for stream in listener.incoming() {
      let Ok(mut stream) = stream else { continue };
      let mut buf = [0u8; 1024];
      let _ = std::io::Read::read(&mut stream, &mut buf);
      if delay_ms > 0 {
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
      }
      let _ = std::io::Write::write_all(&mut stream, &resp);
      let _ = std::io::Write::flush(&mut stream);
    }
  });
  port
}

/// 200 响应报文（Content-Length + body）。
fn http_ok(body: &str) -> Vec<u8> {
  format!(
    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
    body.len(),
    body
  )
  .into_bytes()
}

/// chunked 响应报文（无 Content-Length：reqwest 走分块累计路径）。
fn http_chunked(chunks: &[&str]) -> Vec<u8> {
  let mut out =
    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
      .to_vec();
  for c in chunks {
    out.extend_from_slice(format!("{:x}\r\n", c.len()).as_bytes());
    out.extend_from_slice(c.as_bytes());
    out.extend_from_slice(b"\r\n");
  }
  out.extend_from_slice(b"0\r\n\r\n");
  out
}

/// B1.1 公共装配：demo guest + http ACL（loopback 显式 opt-in 或默认
/// 严格模式）注册进插件感知 ToolManager。
async fn demo_with_http_acl(url_policy: Arc<UrlPolicy>) -> Arc<ToolManager> {
  let wasm = build_guest_wasm();
  let host = WasmPluginHost::new().expect("wasm host init");
  let acl = PluginAcl {
    log: true,
    http: true,
    url_policy: Some(url_policy),
    ..PluginAcl::default()
  };
  let plugin = host.load(&wasm, None, &acl).expect("load demo plugin");
  let manager =
    Arc::new(ToolManager::with_config(plugin_aware_tool_manager_config()));
  let registry = PluginRegistry::new(Arc::clone(&manager));
  registry.register(plugin).expect("register demo plugin");
  manager
}

/// 场景 5（B1.1，§14 DoD 3「受控 URL」CI 层）：白名单（含显式 loopback
/// opt-in）内的请求成功到达本地 mock——http-proxy 成功链路首次覆盖。
#[tokio::test]
async fn http_get_succeeds_with_loopback_whitelist() {
  let port = spawn_http_mock(http_ok("pong"), 0);
  let manager =
    demo_with_http_acl(Arc::new(UrlPolicy::allowing_loopback(["127.0.0.1"])))
      .await;
  let result = manager
    .execute(
      &call(
        "demo__fetch",
        json!({ "url": format!("http://127.0.0.1:{port}/hello") }),
      ),
      &ToolContext::detached(),
    )
    .await
    .expect("whitelisted loopback fetch must succeed");
  assert_eq!(result.details, Some(json!({ "body": "pong" })));
}

/// 场景 6（B1.1）：默认 `UrlPolicy::new` 不放行 loopback——白名单即便
/// 声明 127.0.0.1 也被保留段拦截（`plugin.plugin_forbidden`），显式
/// opt-in 是唯一通道。
#[tokio::test]
async fn loopback_is_blocked_without_explicit_opt_in() {
  let port = spawn_http_mock(http_ok("should-not-reach"), 0);
  let manager =
    demo_with_http_acl(Arc::new(UrlPolicy::new(["127.0.0.1"]))).await;
  let err = manager
    .execute(
      &call(
        "demo__fetch",
        json!({ "url": format!("http://127.0.0.1:{port}/hello") }),
      ),
      &ToolContext::detached(),
    )
    .await
    .expect_err("loopback must stay blocked without opt-in");
  assert_eq!(err.code(), "plugin.plugin_forbidden", "got: {}", err.code());
}

/// 场景 7（B1.1，§13.1「302 跳转被拒」）：宿主 client 禁用自动重定向
/// （`redirect::Policy::none`）——302 响应原样返回，Location 目标
/// 不会被请求。
#[tokio::test]
async fn redirect_is_not_followed() {
  let port = spawn_http_mock(
    b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/moved\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
      .to_vec(),
    0,
  );
  let manager =
    demo_with_http_acl(Arc::new(UrlPolicy::allowing_loopback(["127.0.0.1"])))
      .await;
  let result = manager
    .execute(
      &call(
        "demo__fetch",
        json!({ "url": format!("http://127.0.0.1:{port}/jump") }),
      ),
      &ToolContext::detached(),
    )
    .await
    .expect("302 must be returned as-is (not followed)");
  assert_eq!(result.details, Some(json!({ "body": "" })));
}

/// 场景 8（B1.2，§13.1 DoS 表「超大响应·首块即超限」）：带
/// Content-Length 的 4MB+1 body——`content_length` 预检拦截，
/// `plugin.plugin_exec_failed`（`response too large`）。
#[tokio::test]
async fn oversized_response_first_block_is_rejected() {
  let big = "x".repeat(4 * 1024 * 1024 + 1);
  let resp = format!(
    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
    big.len(),
    big
  )
  .into_bytes();
  let port = spawn_http_mock(resp, 0);
  let manager =
    demo_with_http_acl(Arc::new(UrlPolicy::allowing_loopback(["127.0.0.1"])))
      .await;
  let err = manager
    .execute(
      &call(
        "demo__fetch",
        json!({ "url": format!("http://127.0.0.1:{port}/big") }),
      ),
      &ToolContext::detached(),
    )
    .await
    .expect_err("oversized response must be rejected");
  assert_eq!(
    err.code(),
    "plugin.plugin_exec_failed",
    "got: {}",
    err.code()
  );
  assert!(
    err.message().contains("response too large"),
    "got: {}",
    err.message()
  );
}

/// 场景 9（B1.2，§13.1 DoS 表「超大响应·分块累计超限」）：无
/// Content-Length 的 chunked 响应累计 5MB——`Content-Length` 缺失时
/// 必须靠分块累计拦截（§6.3 分块累计路径）。
#[tokio::test]
async fn oversized_response_chunked_accumulation_is_rejected() {
  let chunk = "y".repeat(1024 * 1024);
  let chunks: Vec<&str> = vec![&chunk; 5];
  let port = spawn_http_mock(http_chunked(&chunks), 0);
  let manager =
    demo_with_http_acl(Arc::new(UrlPolicy::allowing_loopback(["127.0.0.1"])))
      .await;
  let err = manager
    .execute(
      &call(
        "demo__fetch",
        json!({ "url": format!("http://127.0.0.1:{port}/chunked") }),
      ),
      &ToolContext::detached(),
    )
    .await
    .expect_err("chunked accumulation must be rejected");
  assert!(
    err.message().contains("response too large"),
    "got: {}",
    err.message()
  );
}

/// 场景 10（B1.5，§13.1「并发取消不误杀」/ §14 DoD 5）：10 个任务
/// 并发——1 个 fetch（mock 延迟 500ms + 预取消令牌）被即时取消
/// （`tool.cancelled`），其余 9 个 echo 正常完成、互不影响。
#[tokio::test]
async fn concurrent_cancel_does_not_kill_others() {
  let port = spawn_http_mock(http_ok("slow"), 500);
  let manager =
    demo_with_http_acl(Arc::new(UrlPolicy::allowing_loopback(["127.0.0.1"])))
      .await;

  let mut handles = Vec::new();
  for i in 0..10 {
    let manager = Arc::clone(&manager);
    if i == 3 {
      let token = CancellationToken::new();
      token.cancel(); // 预取消：host function select 立即命中
      let ctx = ToolContext::new(token, Arc::new(|_| {}));
      let url = format!("http://127.0.0.1:{port}/slow");
      handles.push(tokio::spawn(async move {
        let r = manager
          .execute(&call("demo__fetch", json!({ "url": url })), &ctx)
          .await;
        ("cancelled", r)
      }));
    } else {
      handles.push(tokio::spawn(async move {
        let r = manager
          .execute(
            &call("demo__echo", json!({ "i": i })),
            &ToolContext::detached(),
          )
          .await;
        ("echo", r)
      }));
    }
  }

  let mut cancelled = 0;
  let mut ok_echo = 0;
  for h in handles {
    let (kind, r) = h.await.expect("task must not panic");
    match kind {
      "cancelled" => {
        let err = r.expect_err("pre-cancelled fetch must be cancelled");
        assert_eq!(err.code(), "tool.cancelled", "got: {}", err.code());
        cancelled += 1;
      }
      _ => {
        r.expect("echo must succeed despite concurrent cancel");
        ok_echo += 1;
      }
    }
  }
  assert_eq!((cancelled, ok_echo), (1, 9), "cancel must not kill others");
}

/// 场景 11（B1.8，§13.1 PDK 分配器专项）：512KB 大参数 echo——guest
/// 堆（dlmalloc）大幅增长 + 宿主写回 ABI 缓冲的场景，复验 v0.8.5
/// 「bump 与 dlmalloc 重叠」修复不再回归（`Value` 索引与字符串完整）。
/// 多工具宏展开（echo/greet/spin/fetch 四工具收集）已由场景 1 覆盖。
#[tokio::test]
async fn large_payload_echo_roundtrip() {
  let wasm = build_guest_wasm();
  let host = WasmPluginHost::new().expect("wasm host init");
  let acl = PluginAcl {
    log: true,
    ..PluginAcl::default()
  };
  let plugin = host.load(&wasm, None, &acl).expect("load demo plugin");
  let manager =
    Arc::new(ToolManager::with_config(plugin_aware_tool_manager_config()));
  let registry = PluginRegistry::new(Arc::clone(&manager));
  registry.register(plugin).expect("register demo plugin");

  let payload = "z".repeat(512 * 1024);
  let result = manager
    .execute(
      &call("demo__echo", json!({ "payload": payload })),
      &ToolContext::detached(),
    )
    .await
    .expect("large payload echo must round-trip");
  assert_eq!(
    result.details,
    Some(json!({ "payload": payload })),
    "guest heap growth must not corrupt host-written ABI buffers"
  );
}

/// 场景 12（B3.2，18_ §12「权威 schema 来源」）：权威清单 plugin_id
/// 与 guest 自述不一致 → 拒绝加载（信任链闭合：宿主侧载清单为信任
/// 源，guest 自述降级为交叉校验）。
#[tokio::test]
async fn authority_manifest_mismatch_is_rejected() {
  let wasm = build_guest_wasm();
  let host = WasmPluginHost::new().expect("wasm host init");
  let auth =
    AuthorityManifest::from_json(br#"{"plugin_id":"not-demo","tools":[]}"#)
      .expect("authority manifest");
  // Arc<dyn Plugin> 无 Debug，无法用 expect_err（需打印 Ok 值）
  let err = match host.load_with_authority(
    &wasm,
    None,
    &PluginAcl::default(),
    Some(&auth),
  ) {
    Err(err) => err,
    Ok(_) => panic!("authority mismatch must be rejected"),
  };
  assert!(
    err.message().contains("authority manifest mismatch"),
    "got: {}",
    err.message()
  );
}
