//! 输入子系统：键盘 task（spawn_blocking）与键位表（16 §7）。

pub mod completion;
pub mod keybindings;
pub mod keyboard;

use crossterm::event::Event;

/// 主循环输入事件（键盘 task 经 mpsc 投喂）。
#[derive(Debug, Clone)]
pub enum AppInput {
  /// 键盘按键（raw mode 下含 Ctrl-C 字节 0x03）。
  Key(crossterm::event::KeyEvent),
  /// 终端尺寸变化。
  Resize(u16, u16),
}

/// 过滤出键盘 / 尺寸事件（其余鼠标等事件直接忽略）。
pub fn to_app_input(event: Event) -> Option<AppInput> {
  match event {
    Event::Key(key) => Some(AppInput::Key(key)),
    Event::Resize(w, h) => Some(AppInput::Resize(w, h)),
    _ => None,
  }
}
