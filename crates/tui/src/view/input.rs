//! 输入编辑器视图：包裹 `RefCell<TextArea>`（16 §3.1 / §7.3）。

use ratatui::Frame;
use ratatui::layout::Rect;
use tui_textarea::TextArea;

use super::View;
use crate::state::AppState;

/// 输入区最大行数（超出后编辑器内部滚动）。
pub const MAX_INPUT_ROWS: usize = 6;

/// 输入编辑器视图（五区中唯一需要可变渲染的组件，R-B3）。
pub struct InputView;

impl View for InputView {
  fn render(&self, frame: &mut Frame, area: Rect, state: &AppState) {
    // set_block 需要可变借用（R-B3 的 RefCell 通道）；0.7 起
    // render_widget 直接受 &TextArea（内部 scroll 更新已在上游解决）
    let mut editor = state.editor.borrow_mut();
    editor.set_block(
      ratatui::widgets::Block::bordered()
        .title("输入 (Enter 发送 · Tab 补全 · Ctrl-C 退出)"),
    );
    frame.render_widget(&*editor, area);
  }

  fn constraints(&self) -> ratatui::layout::Constraint {
    // 动态高度由根布局按编辑器行数计算（见 view::RootLayout::render）
    ratatui::layout::Constraint::Length(3)
  }
}

/// 编辑器当前文本（多行 join）。
pub fn editor_text(state: &AppState) -> String {
  state.editor.borrow().lines().join("\n")
}

/// 清空编辑器（提交后调用；`replace` 整体替换，RefCell 语义，16 §9）。
pub fn clear_editor(state: &AppState) {
  state.editor.replace(TextArea::new(vec![String::new()]));
}

/// 设置编辑器文本（测试辅助 / 补全重建用；单行整体替换）。
pub fn set_editor_text(state: &AppState, text: &str) {
  state.editor.replace(TextArea::new(vec![text.to_string()]));
}
