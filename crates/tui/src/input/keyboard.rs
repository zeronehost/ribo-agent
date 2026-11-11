//! 键盘读取 task：同步阻塞的 `event::read()` 必须在 `spawn_blocking`
//! 中运行（16 §2.1 / §7.2，R-B1——异步任务内直接调用会冻结 Tokio worker）。

use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;

use super::AppInput;
use super::to_app_input;

/// 启动键盘读取 task（参数必须是 **Sender**：键盘 task 持有并
/// `blocking_send` 投喂；主循环持有 `Receiver`）。
///
/// task 生命周期：`event::read()` 永久阻塞读取；主循环退出（Receiver
/// drop）后 `blocking_send` 返回 `Err`，循环自然结束。
pub fn start_keyboard(tx: Sender<AppInput>) -> JoinHandle<()> {
  tokio::task::spawn_blocking(move || {
    // 同步阻塞读取：单事件粒度投喂，背压由 blocking_send 承担
    //（主循环消费远快于键盘输入，短暂阻塞键盘线程可接受）
    while let Ok(event) = crossterm::event::read() {
      let Some(input) = to_app_input(event) else {
        continue; // 非键盘 / 非尺寸事件忽略
      };
      if tx.blocking_send(input).is_err() {
        break; // 主循环已退出
      }
    }
  })
}
