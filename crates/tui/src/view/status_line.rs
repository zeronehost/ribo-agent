//! 状态行视图：mode / 会话键 / token 累计 / 在途 / 上下文占比（16 §3.2）。

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::View;
use crate::state::AppState;

/// 状态行视图（单行，`Constraint::Length(1)`）。
pub struct StatusLineView;

impl View for StatusLineView {
  fn render(&self, frame: &mut Frame, area: Rect, state: &AppState) {
    let status = &state.status;
    let mode = match status.mode {
      ra_cli::config::ConfigMode::Real => "real",
      ra_cli::config::ConfigMode::Mock => "mock",
    };
    let context = match status.context_pct {
      Some(pct) => format!("{pct:.0}%"),
      None => "—".to_string(),
    };
    let in_flight = if status.in_flight {
      "回合中"
    } else {
      "空闲"
    };
    let warn = if status.warnings.is_empty() {
      String::new()
    } else {
      format!(" | ⚠ {}", status.warnings.len())
    };
    let line = Line::from(vec![
      Span::styled(
        format!("[{mode}]"),
        Style::default().fg(if mode == "mock" {
          Color::Yellow
        } else {
          Color::Green
        }),
      ),
      Span::raw(format!(
        " {} | tok in:{} out:{} | ctx:{context} | {in_flight}{warn}",
        status.session_key, status.total_input, status.total_output
      )),
    ]);
    frame.render_widget(Paragraph::new(line), area);
  }

  fn constraints(&self) -> ratatui::layout::Constraint {
    ratatui::layout::Constraint::Length(1)
  }
}
