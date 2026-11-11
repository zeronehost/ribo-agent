//! 对话区视图：气泡渲染 + Markdown 栅栏高亮 + 粘底滚动（16 §3.2 / §6）。

use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use super::View;
use crate::render_helpers::{
  bubble_line_count, ensure_closed_fence, split_markdown_fences,
};
use crate::state::{AppState, ChatBubbleBody};

/// 对话区视图（`Constraint::Min(0)` 占据剩余空间）。
pub struct DialogView;

impl DialogView {
  /// 单气泡 → 渲染行序列（Markdown 栅栏高亮 + 角色前缀着色）。
  fn bubble_lines(bubble: &crate::state::ChatBubble) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    match &bubble.body {
      ChatBubbleBody::User(text) => {
        lines.push(prefix_line("you › ", Color::Cyan));
        lines.extend(text_lines(text, Style::default()));
      }
      ChatBubbleBody::Assistant {
        text,
        thinking,
        error,
      } => {
        if !thinking.is_empty() {
          lines.push(prefix_line("think › ", Color::DarkGray));
          let (rendered, _) = ensure_closed_fence(thinking);
          lines.extend(text_lines(
            &rendered,
            Style::default().fg(Color::DarkGray),
          ));
        }
        lines.push(prefix_line("assistant › ", Color::Green));
        let (rendered, _) = ensure_closed_fence(text);
        lines.extend(markdown_lines(&rendered));
        if let Some(err) = error {
          lines.push(prefix_line("error › ", Color::Red));
          lines.extend(text_lines(err, Style::default().fg(Color::Red)));
        }
      }
      ChatBubbleBody::ToolResult {
        tool,
        text,
        is_error,
      } => {
        let color = if *is_error { Color::Red } else { Color::Yellow };
        lines.push(prefix_line(&format!("tool {tool} › "), color));
        lines.extend(text_lines(text, Style::default().fg(color)));
      }
    }
    lines
  }

  /// 粘底滚动偏移：follow_end 时锁底，否则 clamp 防越界（16 §7.5）。
  fn scroll_offset(&self, state: &AppState, area: Rect, total: usize) -> u16 {
    let visible = usize::from(area.height.max(1));
    let max = total.saturating_sub(visible);
    if state.scroll.follow_end {
      max as u16
    } else {
      state.scroll.offset.min(max as u16)
    }
  }
}

impl View for DialogView {
  fn render(&self, frame: &mut Frame, area: Rect, state: &AppState) {
    // 行数缓存已在主循环按当前宽度刷新；此处只读求和
    let width = area.width.max(4);
    let mut total = 0usize;
    for bubble in &state.chat.messages {
      total += state
        .scroll
        .cached_lines(bubble.id, width)
        .unwrap_or_else(|| bubble_line_count(bubble, width));
    }
    let offset = self.scroll_offset(state, area, total);

    let mut lines: Vec<Line<'static>> = Vec::new();
    for bubble in &state.chat.messages {
      lines.extend(Self::bubble_lines(bubble));
      lines.push(Line::from(""));
    }
    let paragraph = Paragraph::new(lines)
      .block(ratatui::widgets::Block::bordered().title("对话"))
      .wrap(Wrap { trim: false })
      .scroll((offset, 0));
    frame.render_widget(paragraph, area);
  }

  fn constraints(&self) -> Constraint {
    Constraint::Min(0)
  }
}

/// 角色前缀行。
fn prefix_line(prefix: &str, color: Color) -> Line<'static> {
  Line::from(Span::styled(
    prefix.to_string(),
    Style::default().fg(color).add_modifier(Modifier::BOLD),
  ))
}

/// 普通文本行（多逻辑行逐条映射）。
fn text_lines(text: &str, style: Style) -> Vec<Line<'static>> {
  text
    .split('\n')
    .map(|l| Line::from(Span::styled(l.to_string(), style)))
    .collect()
}

/// 最简 Markdown：代码栅栏体着黄色、语言标记着暗色（16 §15 C2 一期口径）。
fn markdown_lines(text: &str) -> Vec<Line<'static>> {
  let mut lines = Vec::new();
  let (rendered, _) = ensure_closed_fence(text);
  for (lang, body, is_code) in split_markdown_fences(&rendered) {
    if is_code {
      lines.push(prefix_line(&format!("```{lang}"), Color::DarkGray));
      for code_line in body.trim_end_matches('\n').split('\n') {
        lines.push(Line::from(Span::styled(
          code_line.to_string(),
          Style::default().fg(Color::Yellow),
        )));
      }
      lines.push(prefix_line("```", Color::DarkGray));
    } else {
      lines.extend(text_lines(body, Style::default()));
    }
  }
  lines
}
