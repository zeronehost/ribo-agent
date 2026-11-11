//! 回合驱动封装：复用 `CliSession::spawn_turn`（16 详细设计 §9）。

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
    return false; // 在途拒绝（16 §9）
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
  true
}

/// `/stop`：请求取消在途回合（后续 `AgentEnd` 自然复位，16 §9）。
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

/// `/new`：轮换会话键（在途拒绝，对齐 CLI `handle_new`）。
///
/// 缺省自动轮换：已带 `-N` 后缀的键递增（`default-2` → `default-3`），
/// 否则追加 `-1`（`default` → `default-1`）。
pub fn rotate_key(app: &mut AppState, name: Option<String>) {
  if matches!(app.turn, TurnState::Running { .. }) {
    return; // 在途拒绝（16 §9）
  }
  let next = match name {
    Some(custom) => custom,
    None => {
      let current = &app.status.session_key;
      match current.rsplit_once('-') {
        Some((prefix, n)) if n.parse::<usize>().is_ok() => {
          format!("{prefix}-{}", n.parse::<usize>().unwrap_or(0) + 1)
        }
        _ => format!("{current}-1"),
      }
    }
  };
  app.status.session_key = next;
}

#[cfg(test)]
mod tests {
  use super::*;
  use ra_cli::config::ConfigMode;

  fn test_state() -> AppState {
    AppState::new(ConfigMode::Mock, Vec::new(), "default".into())
  }

  // 场景：rotate_key 缺省自动轮换（无后缀 → -1；带后缀递增）。
  #[test]
  fn rotate_key_auto_increments_suffix() {
    let mut app = test_state();
    rotate_key(&mut app, None);
    assert_eq!(app.status.session_key, "default-1");
    rotate_key(&mut app, None);
    assert_eq!(app.status.session_key, "default-2");
  }

  // 场景：rotate_key 显式命名生效。
  #[test]
  fn rotate_key_explicit_name() {
    let mut app = test_state();
    rotate_key(&mut app, Some("work".into()));
    assert_eq!(app.status.session_key, "work");
  }
}
