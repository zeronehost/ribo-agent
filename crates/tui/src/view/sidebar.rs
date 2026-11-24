//! 左侧状态面板：会话 / provider / 模型 / token / 上下文占用 / 警告。
//!
//! 由根布局横向切分后占据左侧固定宽度；内容均为 `AppState` 只读快照，
//! 与主循环事件解耦（16 §3 即时模式渲染纪律）。

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use super::View;
use crate::render_helpers::format_compact_tokens;
use crate::state::AppState;

/// 侧栏推荐宽度（终端过窄时由根布局压缩）。
pub const SIDEBAR_WIDTH: u16 = 34;

/// 左侧状态面板视图。
pub struct SidebarView;

/// 上下文占用条（10 格）：按占比填充，超阈值（>75%）转红。
fn ctx_bar(pct: Option<f32>) -> (String, Style) {
  const CELLS: usize = 10;
  let Some(pct) = pct else {
    return ("·".repeat(CELLS), Style::default().fg(Color::DarkGray));
  };
  let filled = ((pct / 100.0) * CELLS as f32).round() as usize;
  let filled = filled.min(CELLS);
  let style = if pct >= 75.0 {
    Style::default().fg(Color::Red)
  } else if pct >= 50.0 {
    Style::default().fg(Color::Yellow)
  } else {
    Style::default().fg(Color::Green)
  };
  (
    format!("{}{}", "█".repeat(filled), "░".repeat(CELLS - filled)),
    style,
  )
}

impl View for SidebarView {
  fn render(&self, frame: &mut Frame, area: Rect, state: &AppState) {
    let s = &state.status;
    let mode = match s.mode {
      ra_cli::config::ConfigMode::Real => "real",
      ra_cli::config::ConfigMode::Mock => "mock",
    };
    let mode_color = if mode == "mock" {
      Color::Yellow
    } else {
      Color::Green
    };
    // 上下文三态（v1.6 用户需求）：总量 token / 已压缩占总量百分比 /
    // 当前使用 token + 占窗口百分比；未装配过统一显示「—」。
    let ctx_stats = s.compaction;
    let total_text = match ctx_stats {
      Some(stats) => format_compact_tokens(stats.total_tokens),
      None => "—".into(),
    };
    // 已压缩量 ÷ 使用总量（反映"多大比例的历史已被摘要替代"）
    let compressed_text = match ctx_stats {
      Some(stats) if stats.total_tokens > 0 => {
        format!(
          "{:.0}%",
          stats.compressed_tokens as f32 / stats.total_tokens as f32 * 100.0
        )
      }
      _ => "—".into(),
    };
    let current_pct = ctx_stats.and_then(|stats| {
      (s.context_window > 0).then(|| {
        (stats.current_tokens as f32 / s.context_window as f32 * 100.0)
          .clamp(0.0, 999.0)
      })
    });
    let current_text = match (ctx_stats, current_pct) {
      (Some(stats), Some(pct)) if s.context_window > 0 => format!(
        "{}/{} ({pct:.0}%)",
        format_compact_tokens(stats.current_tokens),
        format_compact_tokens(s.context_window as usize)
      ),
      (Some(stats), _) => format_compact_tokens(stats.current_tokens),
      _ => "—".into(),
    };
    let (bar, bar_style) = ctx_bar(current_pct);

    let bold = Style::default().add_modifier(Modifier::BOLD);
    let mut lines = vec![
      Line::from(vec![
        Span::styled("模式  ", Style::default().fg(Color::DarkGray)),
        Span::styled(mode, Style::default().fg(mode_color)),
      ]),
      Line::from(vec![
        Span::styled("会话  ", Style::default().fg(Color::DarkGray)),
        Span::raw(s.session_key.clone()),
      ]),
      Line::from(""),
      Line::from(Span::styled("模型", bold)),
      Line::from(vec![
        Span::styled("prov  ", Style::default().fg(Color::DarkGray)),
        Span::raw(s.provider.clone()),
      ]),
      Line::from(vec![
        Span::styled("model ", Style::default().fg(Color::DarkGray)),
        Span::raw(s.model.clone()),
      ]),
      Line::from(""),
      Line::from(Span::styled("Token", bold)),
      Line::from(vec![
        Span::styled("in    ", Style::default().fg(Color::DarkGray)),
        Span::raw(format_compact_tokens(s.total_input as usize)),
      ]),
      Line::from(vec![
        Span::styled("out   ", Style::default().fg(Color::DarkGray)),
        Span::raw(format_compact_tokens(s.total_output as usize)),
      ]),
      Line::from(""),
      Line::from(Span::styled("上下文", bold)),
      Line::from(vec![
        Span::styled("总量  ", Style::default().fg(Color::DarkGray)),
        Span::raw(total_text),
      ]),
      Line::from(vec![
        Span::styled("已压缩 ", Style::default().fg(Color::DarkGray)),
        Span::raw(compressed_text),
      ]),
      Line::from(vec![
        Span::styled("当前  ", Style::default().fg(Color::DarkGray)),
        Span::raw(current_text),
      ]),
      Line::from(Span::styled(bar, bar_style)),
    ];
    if !s.warnings.is_empty() {
      lines.push(Line::from(""));
      lines.push(Line::from(vec![
        Span::styled("警告  ", Style::default().fg(Color::DarkGray)),
        Span::styled(
          s.warnings.len().to_string(),
          Style::default().fg(Color::Yellow),
        ),
      ]));
    }
    let block = Block::bordered().title("状态");
    frame.render_widget(Paragraph::new(lines).block(block), area);
  }

  fn constraints(&self) -> ratatui::layout::Constraint {
    ratatui::layout::Constraint::Length(SIDEBAR_WIDTH)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn ctx_bar_reflects_pct_and_color() {
    let (bar, _) = ctx_bar(Some(0.0));
    assert_eq!(bar.chars().count(), 10);
    assert_eq!(bar, "░░░░░░░░░░");
    let (bar, _) = ctx_bar(Some(50.0));
    assert_eq!(bar, "█████░░░░░");
    let (bar, _) = ctx_bar(Some(100.0));
    assert_eq!(bar, "██████████");
    let (bar, _) = ctx_bar(None);
    assert_eq!(bar, "··········");
  }

  // 场景：上下文「当前」行显示 `<当前量>/<窗口>(nn%)`（v1.6 用户需求）。
  // 预期：1502 / 2048 → `1.5K/2.0K (73%)`；窗口未知时只显示当前量。
  #[test]
  fn current_line_shows_current_over_window_ratio() {
    use ratatui::backend::TestBackend;

    let mut state = AppState::new(
      ra_cli::config::ConfigMode::Mock,
      "p".into(),
      "m".into(),
      Vec::new(),
      "s1".into(),
    );
    state.status.compaction = Some(ra_agent::session::CompactionStats {
      total_tokens: 7586,
      compressed_tokens: 6296,
      current_tokens: 1502,
    });
    state.status.context_window = 2048;

    let mut terminal =
      ratatui::Terminal::new(TestBackend::new(40, 20)).expect("backend");
    terminal
      .draw(|frame| SidebarView.render(frame, frame.area(), &state))
      .expect("draw");
    let text: String = terminal
      .backend()
      .buffer()
      .content()
      .iter()
      .map(|c| c.symbol())
      .collect();
    assert!(
      text.contains("1.5K/2.0K"),
      "current/window ratio missing: {text:?}"
    );
    assert!(text.contains("73%"), "current pct missing: {text:?}");
    // 总量同样紧凑显示
    assert!(text.contains("7.6K"), "total not compact: {text:?}");

    // 窗口未知（0）→ 只显示当前量，不显示比值
    state.status.context_window = 0;
    terminal
      .draw(|frame| SidebarView.render(frame, frame.area(), &state))
      .expect("draw again");
    let text: String = terminal
      .backend()
      .buffer()
      .content()
      .iter()
      .map(|c| c.symbol())
      .collect();
    assert!(text.contains("1.5K"), "current missing: {text:?}");
    assert!(!text.contains("2.0K"), "window must be omitted: {text:?}");
  }
}
