//! 视图层：`View` trait（即时模式、无状态渲染）与五区根布局。
//!

pub mod dialog;
pub mod help_overlay;
pub mod input;
pub mod loading;
pub mod sidebar;

use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};

use crate::state::AppState;

/// 组件视图：每帧由布局分配区域后调用，从 `AppState` 只读绘制
///（可变状态全部收敛在 AppState，组件不持有）。
///
/// 例外：`InputView` 经 `RefCell<TextArea>` 获取内部可变借用渲染。
pub trait View {
  /// 在指定区域绘制自身当前状态。
  fn render(&self, frame: &mut Frame, area: Rect, state: &AppState);
  /// 垂直切分约束（占比 / 固定高度）。
  fn constraints(&self) -> Constraint;
}

/// 根布局：左侧状态面板 + 右侧「对话 / loading / 输入」三区
///（工具调用已内联到对话气泡流；帮助浮层在绘制后按全屏叠加）。
///
/// 窄终端（< 60 列）自动隐藏侧栏，退化为纯纵向布局——状态信息仍可
/// 由 `/status` 浮层查看。
pub struct RootLayout {
  /// 左侧状态面板。
  pub sidebar: sidebar::SidebarView,
  /// 对话区（含粘底滚动 + Markdown 高亮 + 内联工具调用气泡）。
  pub dialog: dialog::DialogView,
  /// 对话区底部固定 loading 行。
  pub loading: loading::LoadingView,
  /// 输入编辑器。
  pub input: input::InputView,
  /// 帮助浮层。
  pub help: help_overlay::HelpOverlayView,
}

impl RootLayout {
  /// 构造默认布局。
  pub fn new() -> Self {
    Self {
      sidebar: sidebar::SidebarView,
      dialog: dialog::DialogView,
      loading: loading::LoadingView,
      input: input::InputView,
      help: help_overlay::HelpOverlayView,
    }
  }

  /// 对话区**文本**有效宽度：终端宽减侧栏宽，再减 Block 边框 2 列。
  ///
  /// 必须与 `DialogView` 中 `Paragraph` 的 `text_area.width`（= area.width
  /// 减 Block 边框）一致——`bubble_line_count` 的行数缓存与 offset
  /// 均按此宽度计算，否则折行行数与 WordWrapper 实际渲染不符 →
  /// 内容显示不全、滚动偏移错位。
  pub fn dialog_width(terminal_width: u16) -> u16 {
    let show_sidebar = Self::sidebar_visible(terminal_width);
    let sidebar_w = if show_sidebar {
      sidebar::SIDEBAR_WIDTH.min(terminal_width / 3).max(24)
    } else {
      0
    };
    terminal_width
      .saturating_sub(sidebar_w)
      .saturating_sub(2) // Block 左右边框
      .max(4)
  }

  /// 侧栏是否显示（与 `dialog_width` 同一判定，供渲染复用）。
  pub fn sidebar_visible(terminal_width: u16) -> bool {
    terminal_width >= 60
  }

  /// 渲染一帧（动态约束 → 布局切分 → 逐区绘制 → 浮层叠加）。
  ///
  /// 输入区高度按编辑器行数动态（封顶 [`input::MAX_INPUT_ROWS`]）——
  /// `View::constraints` 签名不含状态，动态约束由根布局就地计算。
  pub fn render(&self, frame: &mut Frame, state: &AppState) {
    let editor_rows = state.editor.borrow().lines().len() as u16;
    let input_rows = (editor_rows + 1).min(input::MAX_INPUT_ROWS as u16) + 2;

    // 窄屏（< 60 列）隐藏侧栏：状态信息仍可由 /status 浮层查看
    let area = frame.area();
    let show_sidebar = Self::sidebar_visible(area.width);
    let sidebar_w = if show_sidebar {
      sidebar::SIDEBAR_WIDTH.min(area.width / 3).max(24)
    } else {
      0
    };
    let columns = ratatui::layout::Layout::horizontal([
      Constraint::Length(sidebar_w),
      Constraint::Min(0),
    ])
    .split(area);
    if show_sidebar {
      self.sidebar.render(frame, columns[0], state);
    }

    let rows = ratatui::layout::Layout::vertical([
      self.dialog.constraints(),
      self.loading.constraints(),
      Constraint::Length(input_rows),
    ])
    .split(columns[1]);
    self.dialog.render(frame, rows[0], state);
    self.loading.render(frame, rows[1], state);
    self.input.render(frame, rows[2], state);
    if state.help_open {
      self.help.render(frame, area, state);
    }
    if let Some(text) = &state.overlay_text {
      render_text_overlay(frame, area, "提示", text);
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
