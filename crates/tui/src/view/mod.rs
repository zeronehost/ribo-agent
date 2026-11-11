//! 视图层：`View` trait（即时模式、无状态渲染）与五区根布局
//!（16 详细设计 §3）。

pub mod dialog;
pub mod help_overlay;
pub mod input;
pub mod status_line;
pub mod tool_status;

use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};

use crate::state::AppState;

/// 组件视图：每帧由布局分配区域后调用，从 `AppState` 只读绘制
///（15 §3.2 拍板——可变状态全部收敛在 AppState，组件不持有）。
///
/// 例外：`InputView` 经 `RefCell<TextArea>` 获取内部可变借用渲染
///（16 §3.1 R-B3）。
pub trait View {
  /// 在指定区域绘制自身当前状态。
  fn render(&self, frame: &mut Frame, area: Rect, state: &AppState);
  /// 垂直切分约束（占比 / 固定高度）。
  fn constraints(&self) -> Constraint;
}

/// 根布局：垂直切分「对话 / 工具 / 状态行 / 输入」四区（帮助浮层在
/// 四区绘制后按全屏叠加）。
pub struct RootLayout {
  /// 对话区（含粘底滚动 + Markdown 高亮）。
  pub dialog: dialog::DialogView,
  /// 工具状态区。
  pub tools: tool_status::ToolStatusView,
  /// 状态行。
  pub status: status_line::StatusLineView,
  /// 输入编辑器。
  pub input: input::InputView,
  /// 帮助浮层。
  pub help: help_overlay::HelpOverlayView,
}

impl RootLayout {
  /// 构造默认五区。
  pub fn new() -> Self {
    Self {
      dialog: dialog::DialogView,
      tools: tool_status::ToolStatusView,
      status: status_line::StatusLineView,
      input: input::InputView,
      help: help_overlay::HelpOverlayView,
    }
  }

  /// 渲染一帧（动态约束 → 布局切分 → 逐区绘制 → 浮层叠加）。
  ///
  /// 工具区高度按条目数动态（封顶 [`tool_status::MAX_TOOL_ROWS`]），
  /// 输入区高度按编辑器行数动态（封顶 [`input::MAX_INPUT_ROWS`]）——
  /// `View::constraints` 签名不含状态，动态约束由根布局就地计算
  ///（16 §6「`ToolLen`、`InputLen` 由 AppState 动态计算」）。
  pub fn render(&self, frame: &mut Frame, state: &AppState) {
    use tool_status::MAX_TOOL_ROWS;
    let tool_rows = if state.tools.is_empty() {
      0
    } else {
      state.tools.len().min(MAX_TOOL_ROWS) as u16 + 2
    };
    let editor_rows = state.editor.borrow().lines().len() as u16;
    let input_rows = (editor_rows + 1).min(input::MAX_INPUT_ROWS as u16) + 2;
    let chunks = ratatui::layout::Layout::vertical([
      self.dialog.constraints(),
      ratatui::layout::Constraint::Length(tool_rows),
      ratatui::layout::Constraint::Length(1),
      ratatui::layout::Constraint::Length(input_rows),
    ])
    .split(frame.area());
    self.dialog.render(frame, chunks[0], state);
    self.tools.render(frame, chunks[1], state);
    self.status.render(frame, chunks[2], state);
    self.input.render(frame, chunks[3], state);
    if state.help_open {
      self.help.render(frame, frame.area(), state);
    }
    if let Some(text) = &state.overlay_text {
      render_text_overlay(frame, frame.area(), "提示", text);
    }
  }
}

impl Default for RootLayout {
  fn default() -> Self {
    Self::new()
  }
}

/// 通用浮层渲染：`Clear` 清底 + 边框块展示文本行（`/sessions` 等）。
pub fn render_text_overlay(
  frame: &mut Frame,
  area: Rect,
  title: &str,
  text: &str,
) {
  use ratatui::widgets::{Clear, Paragraph, Wrap};
  let block = ratatui::widgets::Block::bordered().title(title);
  let inner_width = area.width.saturating_sub(2).max(1);
  // 按内宽预折行给出高度（clamp 到区域高度）
  let lines = crate::render_helpers::wrapped_text_lines(text, inner_width);
  let height = (lines as u16 + 2).min(area.height);
  let popup = ratatui::layout::Layout::vertical([
    Constraint::Fill(1),
    Constraint::Length(height),
    Constraint::Fill(1),
  ])
  .split(area);
  let popup_area = ratatui::layout::Layout::horizontal([
    Constraint::Fill(1),
    Constraint::Length(area.width.min(72)),
    Constraint::Fill(1),
  ])
  .split(popup[1])[1];
  frame.render_widget(Clear, popup_area);
  frame.render_widget(
    Paragraph::new(text).block(block).wrap(Wrap { trim: false }),
    popup_area,
  );
}
