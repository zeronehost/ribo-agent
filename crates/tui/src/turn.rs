//! 回合驱动封装：复用 `CliSession::spawn_turn`。

use crate::state::{AppState, TurnState};
use crate::view::input as input_view;

/// 提交用户消息发起回合（Idle 时）；在途时拒绝（对齐 CLI 串行回合）。
///
/// 返回是否实际发起了回合（在途 / 空文本返回 false）。
pub fn submit(
  app: &mut AppState,
  session: &ra_cli::config::assemble::CliSession,
) -> bool {
  if matches!(app.turn, TurnState::Running { .. }) {
    return false; // 在途拒绝
  }
  let text = input_view::editor_text(app);
  if text.trim().is_empty() {
    return false; // 空输入忽略（对齐 CLI）
  }
  let key = app.status.session_key.clone();
  let handle = session.spawn_turn(&key, &text);
  input_view::clear_editor(app);
  app.turn = TurnState::Running { key, handle };
  app.status.in_flight = true;
  // 主动发起回合 = 想看新输出：解除上滚状态、恢复粘底，
  // 否则回合中新增内容全部落在视口外（「剩余内容渲染不出来」）
  app.scroll.follow_end = true;
  true
}

/// `/stop`：请求取消在途回合（后续 `AgentEnd` 自然复位）。
///
/// 返回是否确有在途回合被请求取消。
pub async fn request_stop(
  app: &mut AppState,
  session: &ra_cli::config::assemble::CliSession,
) -> bool {
  if let TurnState::Running { key, .. } = &app.turn {
    let _ = session.harness.stop(key).await;
    true
  } else {
    false
  }
}

#[cfg(test)]
mod tests {
  // （sessions 合并原 /new 后无本模块单测；命令解析见 ra-cli commands）
}
