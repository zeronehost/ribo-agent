//! 对话区底部固定 loading 行：在途时逐帧动画，空闲时静默提示。
//!
//! 动画由主循环的 tick 源递增 `AppState.spinner_tick` 驱动，视图只读
//! 帧计数——保持即时模式渲染纪律（16 §3）。

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::View;
use crate::state::AppState;

/// Braille 转轮帧（8 帧，约 100ms/帧 → 0.8s 一周）。
const FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

/// loading 行视图（固定高度 1）。
pub struct LoadingView;

impl View for LoadingView {
  fn render(&self, frame: &mut Frame, area: Rect, state: &AppState) {
    // D8 压缩痕迹走对话区 Compaction 气泡（Compaction 与 AgentStart 同批
    // 到达，瞬时状态位在即时模式渲染下不可见——气泡才是用户可见载体）
    let line = if state.status.in_flight {
      let frame = FRAMES[(state.spinner_tick as usize) % FRAMES.len()];
      Line::from(vec![
        Span::styled(
          format!(" {frame} "),
          Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
          "处理中…（Ctrl-C 取消）",
          Style::default().fg(Color::DarkGray),
        ),
      ])
    } else {
      Line::from(Span::styled(" 就绪", Style::default().fg(Color::DarkGray)))
    };
    frame.render_widget(Paragraph::new(line), area);
  }

  fn constraints(&self) -> ratatui::layout::Constraint {
    ratatui::layout::Constraint::Length(1)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn frames_cycle_over_ticks() {
    assert_eq!(FRAMES.len(), 8);
    assert_ne!(FRAMES[0], FRAMES[1]);
    // 帧索引按 tick 取模，8 步后回到起点
    let first = FRAMES[0usize % FRAMES.len()];
    let after_cycle = FRAMES[8usize % FRAMES.len()];
    assert_eq!(first, after_cycle);
  }
}
