//! 上下文压缩探针：加载 `ribo_compaction_mock.toml`，用脚本化 MockProvider
//! 连续跑多轮长对话，观察 Compactor 的触发日志（零 API 成本）。
//!
//! ```text
//! cargo run -p ra-cli --example compaction_probe
//! ```
//!
//! 预期日志（写入 `tmps/compaction-mock/.ribo/logs/cli.log.<日期>`）：
//!   compaction triggered ... → 第 3 轮起每轮触发（压缩不写回 session 的已知行为）。

use std::sync::Arc;

use ra_cli::config::{LoadOutcome, assemble::assemble_with_steps};
use ra_core::message::{AssistantMessage, Content, StopReason, TextContent};
use ra_provider::adapters::mock::MockStep;

fn step(text: &'static str) -> MockStep {
  MockStep::Complete(Arc::new(move |_| {
    let mut message = AssistantMessage::empty("mock", "mock-model", 0);
    message.content.push(Content::Text(TextContent {
      content: text.to_string(),
      signature: None,
    }));
    message.stop_reason = StopReason::Stop;
    (StopReason::Stop, message)
  }))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
  // 文件日志（本地时间）
  let outcome = LoadOutcome::load(
    Some(std::path::Path::new("ribo_compaction_mock.toml")),
    None,
  );
  let root = outcome.log_root();
  let _guard = ra_cli::logging::init_file_logging(&root)?;

  // 注入 8 步成功脚本，每步回一段长文本（让历史快速膨胀）
  let steps: Vec<MockStep> = (0..8)
    .map(|_| step("这是一段用于撑大上下文的模拟回复：所有权、借用与生命周期是 Rust 的三块基石；编译器在编译期静态检查借用关系，既保证内存安全又零运行时开销。"))
    .collect();
  let mut session =
    assemble_with_steps(&outcome, steps).expect("mock 配置装配成功");

  let key = "compaction-probe";
  for turn in 1..=6 {
    let long_input = format!(
      "第 {turn} 轮：请详细解释 Rust 的所有权机制以及它如何在编译期防止悬垂指针与数据竞争。\
       另外请说明借用检查器的规则，以及为什么这些规则不需要垃圾回收器就能保证内存安全。"
    );
    let result = session.harness.prompt(key, &long_input).await;
    // 排空本轮事件（避免 broadcast 缓冲堆积）
    while session.events.try_recv().is_ok() {}
    match result {
      Ok(messages) => {
        let chars: usize = messages
          .iter()
          .flat_map(|m| match m {
            ra_core::message::Message::User(u) => match &u.content {
              ra_core::message::UserContent::Text(t) => vec![t.as_str()],
              _ => vec![],
            },
            ra_core::message::Message::Assistant(a) => a
              .content
              .iter()
              .filter_map(|c| match c {
                Content::Text(t) => Some(t.content.as_str()),
                _ => None,
              })
              .collect(),
            _ => vec![],
          })
          .map(|s| s.chars().count())
          .sum();
        println!(
          "第 {turn} 轮完成：会话累计 {} 条消息 / {} 字符",
          messages.len(),
          chars
        );
      }
      Err(err) => {
        println!("第 {turn} 轮失败：{} {}", err.code(), err.message())
      }
    }
  }
  println!("日志目录：{}", root.display());
  Ok(())
}
