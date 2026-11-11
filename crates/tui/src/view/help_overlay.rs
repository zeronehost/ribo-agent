//! 帮助浮层视图：分组命令表（与 CLI `/help` 同源 `COMMANDS` 表，16 §10）。

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use super::View;
use crate::state::AppState;

/// 帮助浮层视图（全屏 `Clear` 后重绘分组命令表）。
pub struct HelpOverlayView;

impl View for HelpOverlayView {
  fn render(&self, frame: &mut Frame, area: Rect, _state: &AppState) {
    let mut lines = Vec::new();
    lines.push(Line::from(Span::styled(
      "ribo-tui 命令",
      ratatui::style::Style::default()
        .fg(ratatui::style::Color::Green)
        .add_modifier(ratatui::style::Modifier::BOLD),
    )));
    lines.push(Line::from(""));
    for meta in ra_cli::commands::COMMANDS {
      let usage = if meta.args.is_empty() {
        meta.name.to_string()
      } else {
        format!("{} {}", meta.name, meta.args)
      };
      let alias = if meta.aliases.is_empty() {
        String::new()
      } else {
        format!("  （{}）", meta.aliases.join(" "))
      };
      lines.push(Line::from(vec![
        Span::styled(
          format!("{usage:<20}"),
          ratatui::style::Style::default().fg(ratatui::style::Color::Cyan),
        ),
        Span::raw(meta.description),
        Span::styled(
          alias,
          ratatui::style::Style::default().fg(ratatui::style::Color::DarkGray),
        ),
      ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
      "其余输入作为消息发送给模型。Ctrl-C 退出（在途回合会先取消落盘）。",
      ratatui::style::Style::default().fg(ratatui::style::Color::DarkGray),
    )));
    let block = ratatui::widgets::Block::bordered().title("帮助 (Esc 关闭)");
    frame.render_widget(Clear, area);
    frame.render_widget(
      Paragraph::new(lines)
        .block(block)
        .wrap(ratatui::widgets::Wrap { trim: false }),
      area,
    );
  }

  fn constraints(&self) -> ratatui::layout::Constraint {
    // 浮层不参与根布局切分（全屏叠加绘制）
    ratatui::layout::Constraint::Length(0)
  }
}
