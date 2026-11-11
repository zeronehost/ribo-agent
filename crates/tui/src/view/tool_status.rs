//! 工具状态区视图：运行 / 完成 / 错误条目（16 §3.2）。

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::View;
use crate::state::{AppState, ToolPhase};

/// 工具状态区视图（无状态渲染；高度由根布局按 `state.tools.len()` 计算）。
pub struct ToolStatusView;

/// 工具区最大行数（超出后内部滚动，16 §6「上限封顶」）。
pub const MAX_TOOL_ROWS: usize = 8;

impl View for ToolStatusView {
  fn render(&self, frame: &mut Frame, area: Rect, state: &AppState) {
    if state.tools.is_empty() {
      return; // 无条目：根布局给 Length(0)，本区不渲染
    }
    let mut lines = Vec::new();
    for run in &state.tools {
      let (marker, style) = match run.phase {
        ToolPhase::Running => ("…", Style::default().fg(Color::Cyan)),
        ToolPhase::Done(false) => ("✓", Style::default().fg(Color::Green)),
        ToolPhase::Done(true) => ("✗", Style::default().fg(Color::Red)),
      };
      lines.push(Line::from(vec![
        Span::styled(format!("[{marker}] "), style),
        Span::raw(run.name.clone()),
      ]));
    }
    let paragraph = Paragraph::new(lines)
      .block(ratatui::widgets::Block::bordered().title("工具"));
    frame.render_widget(paragraph, area);
  }

  fn constraints(&self) -> ratatui::layout::Constraint {
    // 动态高度由根布局按 state 计算（View trait 约束签名不含状态）；
    // 此处返回兜底值（根布局不调用本条目）。
    ratatui::layout::Constraint::Length(0)
  }
}
