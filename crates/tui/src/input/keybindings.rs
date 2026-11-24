//! 键位表：`KeyEvent → Action` 集中管理（对齐 pi-tui
//! keybindings；组合键进表统一管理，禁止散落 `if`）。

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// 键盘动作（主循环据此分派）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
  /// 提交输入（Enter）。
  Submit,
  /// Tab 补全。
  Complete,
  /// 退出程序（Ctrl-C，不交给默认 SIGINT）。
  Exit,
  /// 上滚一行（滚轮 / 上方向键）。
  ScrollUp,
  /// 下滚一行（滚轮 / 下方向键）。
  ScrollDown,
  /// 上翻页。
  PageUp,
  /// 下翻页。
  PageDown,
  /// 关闭浮层 / 清空输入（Esc）。
  Cancel,
}

/// 是否为**按下**的 Ctrl-C 键事件（raw mode 下为字节 0x03）。
///
/// 必须校验 `kind`：Windows conPTY 会为同一次按键同时产出 Press /
/// Release 两个事件。退出流程也调用本函数，若不
/// 排除 Release，同一次 Ctrl-C 的 Release 会被误判成「第二次 Ctrl-C」
/// 而直接强退（退出码 1），使优雅退出失效。
pub fn is_ctrl_c(key: &KeyEvent) -> bool {
  key.code == KeyCode::Char('c')
    && key.modifiers.contains(KeyModifiers::CONTROL)
    && key.kind == KeyEventKind::Press
}

/// 键事件 → 动作（仅匹配表内组合；其余交给编辑器）。
///
/// 滚动方向键在输入框为多行编辑时仍作用于对话区——编辑器以单行
/// 提交为主，方向键全部让位滚动（Ctrl-V/PageUp/PageDown 滚动）。
pub fn map_key(key: &KeyEvent) -> Option<Action> {
  // 仅响应按下事件（Windows conPTY 会同时产出 Press/Release）
  if key.kind != KeyEventKind::Press {
    return None;
  }
  if is_ctrl_c(key) {
    return Some(Action::Exit);
  }
  match key.code {
    KeyCode::Enter => Some(Action::Submit),
    KeyCode::Tab => Some(Action::Complete),
    KeyCode::Esc => Some(Action::Cancel),
    KeyCode::Up => Some(Action::ScrollUp),
    KeyCode::Down => Some(Action::ScrollDown),
    KeyCode::PageUp => Some(Action::PageUp),
    KeyCode::PageDown => Some(Action::PageDown),
    KeyCode::Char('v') if key.modifiers.contains(KeyModifiers::CONTROL) => {
      Some(Action::PageUp)
    }
    _ => None,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crossterm::event::{KeyEventKind, KeyEventState};

  fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent {
      code,
      modifiers,
      kind: KeyEventKind::Press,
      state: KeyEventState::empty(),
    }
  }

  // 场景：核心键位映射。
  #[test]
  fn maps_core_keys() {
    assert_eq!(
      map_key(&key(KeyCode::Enter, KeyModifiers::NONE)),
      Some(Action::Submit)
    );
    assert_eq!(
      map_key(&key(KeyCode::Tab, KeyModifiers::NONE)),
      Some(Action::Complete)
    );
    assert_eq!(
      map_key(&key(KeyCode::Esc, KeyModifiers::NONE)),
      Some(Action::Cancel)
    );
    assert_eq!(
      map_key(&key(KeyCode::PageUp, KeyModifiers::NONE)),
      Some(Action::PageUp)
    );
    assert_eq!(
      map_key(&key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
      Some(Action::Exit)
    );
    assert_eq!(
      map_key(&key(KeyCode::Char('v'), KeyModifiers::CONTROL)),
      Some(Action::PageUp)
    );
  }

  // 场景：Release 事件不产生动作（Windows conPTY 双发）。
  #[test]
  fn ignores_release_events() {
    let mut release = key(KeyCode::Enter, KeyModifiers::NONE);
    release.kind = KeyEventKind::Release;
    assert_eq!(map_key(&release), None);
  }

  // 场景：is_ctrl_c 仅匹配按下的 Ctrl+C；同一次按键的 Release 不算
  //（否则退出流程会把 Release 误判为「第二次 Ctrl-C」而强退）。
  #[test]
  fn ctrl_c_detection() {
    assert!(is_ctrl_c(&key(KeyCode::Char('c'), KeyModifiers::CONTROL)));
    assert!(!is_ctrl_c(&key(KeyCode::Char('c'), KeyModifiers::NONE)));
    assert!(!is_ctrl_c(&key(KeyCode::Esc, KeyModifiers::CONTROL)));

    let mut release = key(KeyCode::Char('c'), KeyModifiers::CONTROL);
    release.kind = KeyEventKind::Release;
    assert!(!is_ctrl_c(&release), "Release 不应计为一次 Ctrl-C");

    let mut repeat = key(KeyCode::Char('c'), KeyModifiers::CONTROL);
    repeat.kind = KeyEventKind::Repeat;
    assert!(!is_ctrl_c(&repeat), "Repeat 不应计为一次 Ctrl-C");
  }

  // 场景：普通字符键不产生动作（交给编辑器）。
  #[test]
  fn plain_chars_pass_through() {
    assert_eq!(map_key(&key(KeyCode::Char('a'), KeyModifiers::NONE)), None);
  }
}
