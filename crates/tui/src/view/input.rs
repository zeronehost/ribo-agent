//! 输入编辑器视图：包裹 `RefCell<TextArea>`。

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui_textarea::{CursorMove, DataCursor, TextArea};

use super::View;
use crate::state::AppState;

/// 输入区最大行数（超出后视图内部滚动）。
pub const MAX_INPUT_ROWS: usize = 6;

/// 把逻辑行按可用宽度折成视觉行（字符级贪心、CJK 宽字符友好）。
///
/// 与根布局的高度估算共用本函数，保证「折行口径」唯一：长单行不再
/// 挤在一行、把后续内容滚出视口。折行结果与
/// [`crate::render_helpers::hard_wrap`] 严格一致。
pub fn wrapped_lines(state: &AppState, width: u16) -> Vec<String> {
  let width = usize::from(width.max(4));
  state
    .editor
    .borrow()
    .lines()
    .iter()
    .flat_map(|line| crate::render_helpers::hard_wrap(line, width))
    .collect()
}

/// 当前文本在给定宽度下的视觉行数（空编辑器占 1 行）。
pub fn visual_rows(state: &AppState, width: u16) -> usize {
  wrapped_lines(state, width).len().max(1)
}

/// 把主编辑器光标 `(逻辑行, 字符列)` 映射到折行后的视觉坐标。
///
/// `col` 是字符索引（ratatui-textarea 语义），`hard_wrap` 按显示宽度折行
/// 但不切断字符，因此逐段累加字符数即可定位。光标恰好落在折行边界
/// （`remaining == 段字符数`）时取下一视觉行行首，避免停在满行末尾
/// 而看不见后续输入。
fn map_cursor(
  lines: &[String],
  cursor: (usize, usize),
  width: u16,
) -> (u16, u16) {
  let width = usize::from(width.max(4));
  let (row, col) = cursor;
  let mut vrow = 0usize;
  for line in lines.iter().take(row) {
    vrow += crate::render_helpers::hard_wrap(line, width).len();
  }
  let wrapped = lines
    .get(row)
    .map(|line| crate::render_helpers::hard_wrap(line, width))
    .unwrap_or_else(|| vec![String::new()]);
  let mut remaining = col;
  let mut found = None;
  for (i, seg) in wrapped.iter().enumerate() {
    let chars = seg.chars().count();
    if remaining < chars {
      found = Some((vrow + i, remaining));
      break;
    }
    if remaining == chars {
      // 折行边界：满行末尾等同下一行行首；文末则留在末段末尾。
      found = Some(if i + 1 < wrapped.len() {
        (vrow + i + 1, 0)
      } else {
        (vrow + i, chars)
      });
      break;
    }
    remaining -= chars;
  }
  let (vr, vc) = found.unwrap_or_else(|| {
    let last = wrapped.len().saturating_sub(1);
    (
      vrow + last,
      wrapped.last().map(|s| s.chars().count()).unwrap_or(0),
    )
  });
  (
    vr.min(u16::MAX as usize) as u16,
    vc.min(u16::MAX as usize) as u16,
  )
}

/// 当前光标在折行视图中的视觉坐标。
fn visual_cursor(state: &AppState, width: u16) -> (u16, u16) {
  let editor = state.editor.borrow();
  let DataCursor(row, col) = editor.cursor();
  map_cursor(editor.lines(), (row, col), width)
}

/// 输入编辑器视图（五区中唯一需要可变渲染的组件）。
pub struct InputView;

impl View for InputView {
  fn render(&self, frame: &mut Frame, area: Rect, state: &AppState) {
    // 边框占左右各 1 列，按内宽把长文本折成视觉行。用临时 TextArea
    // 承载折行结果：TextArea 自带的 scroll_top_row 会跟随映射后的光标，
    // 既能垂直滚动又不退化成水平滚动截字。主编辑器原文不变。
    let inner_width = area.width.saturating_sub(2);
    let mut view = TextArea::new(wrapped_lines(state, inner_width));
    let (vrow, vcol) = visual_cursor(state, inner_width);
    view.move_cursor(CursorMove::Jump(vrow, vcol));
    view.set_block(
      ratatui::widgets::Block::bordered()
        .title("输入 (Enter 发送 · Tab 补全 · Ctrl-C 退出)"),
    );
    frame.render_widget(&view, area);
  }

  fn constraints(&self) -> ratatui::layout::Constraint {
    // 动态高度由根布局按折行后的视觉行数计算（见 view::RootLayout::render）
    ratatui::layout::Constraint::Length(3)
  }
}

/// 编辑器当前文本（多行 join）。
pub fn editor_text(state: &AppState) -> String {
  state.editor.borrow().lines().join("\n")
}

/// 清空编辑器（提交后调用；`replace` 整体替换，RefCell 语义）。
pub fn clear_editor(state: &AppState) {
  state.editor.replace(TextArea::new(vec![String::new()]));
}

/// 设置编辑器文本（测试辅助 / 补全重建用；单行整体替换）。
pub fn set_editor_text(state: &AppState, text: &str) {
  state.editor.replace(TextArea::new(vec![text.to_string()]));
}

#[cfg(test)]
mod tests {
  use super::*;

  // 场景：ASCII 长行按显示宽度折成多个视觉行。
  // 预期：10 宽下 25 字符 → 3 行（10/10/5）。
  #[test]
  fn wraps_ascii_long_line() {
    let lines = ["a".repeat(25)];
    let wrapped = crate::render_helpers::hard_wrap(&lines[0], 10);
    assert_eq!(wrapped, vec!["a".repeat(10), "a".repeat(10), "a".repeat(5)]);
  }

  // 场景：光标落在折行边界前后的视觉坐标。
  // 预期：10 宽下 25 字符，col=15 → (行 1, 列 5)；
  //       col=20 恰在折行边界 → 下一行行首 (2, 0)；文末 col=25 → (2, 5)。
  #[test]
  fn maps_cursor_to_visual_position() {
    let lines = ["a".repeat(25)];
    assert_eq!(map_cursor(&lines, (0, 0), 10), (0, 0));
    assert_eq!(map_cursor(&lines, (0, 15), 10), (1, 5));
    assert_eq!(map_cursor(&lines, (0, 20), 10), (2, 0));
    assert_eq!(map_cursor(&lines, (0, 25), 10), (2, 5));
  }

  // 场景：CJK 宽字符按显示宽度折行，光标映射用字符索引。
  // 预期：10 宽下 6 个汉字（宽 12）→ 2 行（5 字 / 1 字）；
  //       col=5 恰在边界 → (1, 0)。
  #[test]
  fn maps_cjk_cursor() {
    let lines = ["汉".repeat(6)];
    assert_eq!(map_cursor(&lines, (0, 4), 10), (0, 4));
    assert_eq!(map_cursor(&lines, (0, 5), 10), (1, 0));
    assert_eq!(map_cursor(&lines, (0, 6), 10), (1, 1));
  }

  // 场景：多逻辑行时视觉行号累加。
  // 预期：首行 12 字符（10 宽 → 2 行），次行光标 col=2 → 视觉 (2, 2)。
  #[test]
  fn maps_cursor_across_logical_lines() {
    let lines = ["a".repeat(12), "bcdef".to_string()];
    assert_eq!(map_cursor(&lines, (1, 2), 10), (2, 2));
  }
}
