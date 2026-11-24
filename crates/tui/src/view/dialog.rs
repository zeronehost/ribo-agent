//! 对话区视图：气泡渲染（自折行）+ Markdown 栅栏高亮 + 粘底滚动
//!（16 §3.2 / §6；2026-11-22 渲染架构重设计）。
//!
//! **渲染与滚动共用同一折行真值**：所有文本经 [`hard_wrap`] 折成视觉行
//! 后交给 `Paragraph`（不再启用 WordWrapper），行数缓存 / 滚动偏移与
//! 实际渲染严格一致；同时按视口裁剪——只折可见气泡，渲染成本与
//! 会话总内容量解耦（内容再多也不卡、尾部始终可达）。

use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::View;
use crate::render_helpers::{
  bubble_line_count, ensure_closed_fence, hard_wrap, split_markdown_fences,
};
use crate::state::{AppState, ChatBubbleBody};

/// 对话区视图（`Constraint::Min(0)` 占据剩余空间）。
pub struct DialogView;

impl DialogView {
  /// 单气泡 → **已折行**的视觉行序列（Markdown 栅栏高亮 + 角色前缀着色）。
  ///
  /// 行数（+1 分隔空行）与 [`bubble_line_count`] 严格一致——两者共用
  /// [`hard_wrap`] 与栅栏口径。
  fn bubble_lines(
    bubble: &crate::state::ChatBubble,
    width: u16,
  ) -> Vec<Line<'static>> {
    let w = usize::from(width);
    let mut lines: Vec<Line<'static>> = Vec::new();
    match &bubble.body {
      ChatBubbleBody::User(text) => {
        push_prefix(&mut lines, "you › ", Color::Cyan, w);
        push_text(&mut lines, text, Style::default(), w);
      }
      ChatBubbleBody::Assistant {
        text,
        thinking,
        error,
      } => {
        if !thinking.is_empty() {
          push_prefix(&mut lines, "think › ", Color::DarkGray, w);
          let (rendered, _) = ensure_closed_fence(thinking);
          push_text(
            &mut lines,
            &rendered,
            Style::default().fg(Color::DarkGray),
            w,
          );
        }
        push_prefix(&mut lines, "assistant › ", Color::Green, w);
        let (rendered, _) = ensure_closed_fence(text);
        push_markdown(&mut lines, &rendered, w);
        if let Some(err) = error {
          push_prefix(&mut lines, "error › ", Color::Red, w);
          push_text(&mut lines, err, Style::default().fg(Color::Red), w);
        }
      }
      ChatBubbleBody::ToolResult {
        tool,
        text,
        is_error,
        running,
        ..
      } => {
        let color = if *is_error {
          Color::Red
        } else if *running {
          Color::Cyan
        } else {
          Color::Yellow
        };
        let marker = if *is_error {
          "✗"
        } else if *running {
          "…"
        } else {
          "✓"
        };
        push_prefix(&mut lines, &format!("tool {marker} {tool} › "), color, w);
        push_text(&mut lines, text, Style::default().fg(color), w);
      }
      // D8：压缩痕迹——持久系统行，不显示摘要内容 / 截断条数；
      // 失败回退用黄色（降级告警）与成功暗色区分
      ChatBubbleBody::Compaction { failed } => {
        let (text, color) = if *failed {
          (ChatBubbleBody::COMPACTION_FAILED_TEXT, Color::Yellow)
        } else {
          (ChatBubbleBody::COMPACTION_TEXT, Color::DarkGray)
        };
        push_prefix(&mut lines, ChatBubbleBody::COMPACTION_PREFIX, color, w);
        push_text(&mut lines, text, Style::default().fg(color), w);
      }
    }
    lines
  }
}

/// 视口可显示的文本行数（area 高度减 Block 上下边框 2）。
fn visible_rows(area: Rect) -> usize {
  usize::from(area.height.saturating_sub(2).max(1))
}

/// 前缀行（折行后着色加粗）。
fn push_prefix(
  lines: &mut Vec<Line<'static>>,
  prefix: &str,
  color: Color,
  w: usize,
) {
  for row in hard_wrap(prefix, w) {
    lines.push(Line::from(Span::styled(
      row,
      Style::default().fg(color).add_modifier(Modifier::BOLD),
    )));
  }
}

/// 普通文本：逐逻辑行折行，统一着色。
fn push_text(
  lines: &mut Vec<Line<'static>>,
  text: &str,
  style: Style,
  w: usize,
) {
  for logical in text.split('\n') {
    for row in hard_wrap(logical, w) {
      lines.push(Line::from(Span::styled(row, style)));
    }
  }
}

/// Markdown 渲染：代码栅栏体着黄色、语言标记着暗色（16 §15 C2 一期口径），
/// 全部经折行。与估算端 `markdown_row_count` 口径严格对应。
fn push_markdown(lines: &mut Vec<Line<'static>>, text: &str, w: usize) {
  let (rendered, _) = ensure_closed_fence(text);
  for (lang, body, is_code) in split_markdown_fences(&rendered) {
    if is_code {
      for row in hard_wrap(&format!("```{lang}"), w) {
        lines.push(prefix_span(row, Color::DarkGray));
      }
      for logical in body.trim_end_matches('\n').split('\n') {
        for row in hard_wrap(logical, w) {
          lines.push(Line::from(Span::styled(
            row,
            Style::default().fg(Color::Yellow),
          )));
        }
      }
      for row in hard_wrap("```", w) {
        lines.push(prefix_span(row, Color::DarkGray));
      }
    } else {
      push_text(
        lines,
        body.trim_end_matches('\n').trim_start_matches('\n'),
        Style::default(),
        w,
      );
    }
  }
}

/// 栅栏标记行（暗色）。
fn prefix_span(row: String, color: Color) -> Line<'static> {
  Line::from(Span::styled(
    row,
    Style::default().fg(color).add_modifier(Modifier::BOLD),
  ))
}

impl View for DialogView {
  fn render(&self, frame: &mut Frame, area: Rect, state: &AppState) {
    // 行数缓存按文本宽度（area.width - Block 边框 2）计算（与 App::draw
    // 的 RootLayout::dialog_width 口径一致）
    let width = area.width.saturating_sub(2).max(4);
    let visible = visible_rows(area);

    // 1) 总行数与每气泡行数（缓存优先，miss 时估算——两者共用 hard_wrap）
    let mut total = 0usize;
    let counts: Vec<usize> = state
      .chat
      .messages
      .iter()
      .map(|bubble| {
        let count = state
          .scroll
          .cached_lines(bubble.id, width)
          .unwrap_or_else(|| bubble_line_count(bubble, width));
        total += count;
        count
      })
      .collect();

    // 2) 滚动偏移：follow_end 锁底，否则 clamp 防越界
    let max_offset = total.saturating_sub(visible);
    let offset = if state.scroll.follow_end {
      max_offset
    } else {
      (state.scroll.offset as usize).min(max_offset)
    };

    // 3) 可见窗口裁剪：跳过视口上方的整气泡（只数数不折行），从起始
    //    气泡折行，填满视口即停——渲染成本与总内容量解耦
    let mut skip = offset;
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(visible);
    'outer: for (bubble, &count) in state.chat.messages.iter().zip(&counts) {
      if skip >= count {
        skip -= count;
        continue; // 整气泡在视口上方
      }
      let mut rows = Self::bubble_lines(bubble, width);
      rows.push(Line::from("")); // 气泡间分隔空行（计入行数缓存）
      for line in rows {
        if skip > 0 {
          skip -= 1;
          continue;
        }
        lines.push(line);
        if lines.len() >= visible {
          break 'outer;
        }
      }
    }

    // 4) 已折好、已裁剪：Paragraph 无 wrap 无 scroll，所见即行数真值
    let paragraph = Paragraph::new(lines)
      .block(ratatui::widgets::Block::bordered().title("对话"));
    frame.render_widget(paragraph, area);
  }

  fn constraints(&self) -> Constraint {
    Constraint::Min(0)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::state::ChatBubble;

  /// 行数估算必须与渲染行数严格一致（否则滚动偏移错位、内容截断）。
  /// 覆盖：纯文本 / 代码块（闭合 / 未闭合）/ thinking / 工具结果。
  #[test]
  fn line_count_matches_rendered_lines() {
    const W: u16 = 60;
    let cases = vec![
      ChatBubbleBody::User("plain text".into()),
      ChatBubbleBody::User("汉".repeat(80)),
      ChatBubbleBody::Assistant {
        text: "前置\n```rust\nfn main() {}\n```\n后置".into(),
        thinking: String::new(),
        error: None,
      },
      // 未闭合栅栏：ensure_closed_fence 补一行
      ChatBubbleBody::Assistant {
        text: "```rust\nfn main() {".into(),
        thinking: String::new(),
        error: None,
      },
      ChatBubbleBody::Assistant {
        text: "answer".into(),
        thinking: "thinking...".into(),
        error: Some("boom".into()),
      },
      ChatBubbleBody::ToolResult {
        call_id: Some("c1".into()),
        tool: "read".into(),
        text: "file content".into(),
        is_error: false,
        running: false,
      },
      // 执行中的工具气泡
      ChatBubbleBody::ToolResult {
        call_id: Some("c2".into()),
        tool: "write".into(),
        text: "执行中…".into(),
        is_error: false,
        running: true,
      },
    ];
    for (i, body) in cases.into_iter().enumerate() {
      let bubble = ChatBubble { id: i as u64, body };
      // 渲染行数 = bubble_lines（自折行后的视觉行）+ 分隔空行；
      // 与 bubble_line_count 共用 hard_wrap，必须严格相等
      let rendered = DialogView::bubble_lines(&bubble, W).len() + 1;
      let estimated = bubble_line_count(&bubble, W);
      assert_eq!(
        estimated, rendered,
        "case {i}: line count mismatch: estimated={estimated} rendered={rendered}"
      );
    }
  }
}
