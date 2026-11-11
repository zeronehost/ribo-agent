//! 渲染辅助：气泡折行行数（含宽字符）/ 行数缓存刷新 / Markdown 栅栏处理
//!（16 详细设计 §6 / §7.5 / §15 C2）。

use unicode_width::UnicodeWidthStr;

use crate::state::{AppState, ChatBubble, ChatBubbleBody};

/// 渲染区默认宽度兜底（首次 draw 前的估算口径）。
pub const FALLBACK_WIDTH: u16 = 80;

/// 单个气泡在给定宽度下折成的视觉行数（含角色前缀行）。
///
/// 口径：按逻辑行（`\n` 切分）逐行以显示宽度上取整折行；与 ratatui
/// `Paragraph::wrap` 的行为对齐到「行数正确」粒度（长单词硬折）。
pub fn bubble_line_count(bubble: &ChatBubble, width: u16) -> usize {
  let width = usize::from(width.max(4)); // 防御：极窄区域按 4 列估算
  let logical = match &bubble.body {
    ChatBubbleBody::User(text) => vec![("you › ".to_string(), text.clone())],
    ChatBubbleBody::Assistant {
      text,
      thinking,
      error,
    } => {
      let mut rows = Vec::new();
      if !thinking.is_empty() {
        rows.push(("think › ".to_string(), thinking.clone()));
      }
      rows.push(("assistant › ".to_string(), text.clone()));
      if let Some(err) = error {
        rows.push(("error › ".to_string(), err.clone()));
      }
      rows
    }
    ChatBubbleBody::ToolResult { tool, text, .. } => {
      vec![(format!("tool {tool} › "), text.clone())]
    }
  };
  let mut count = 0usize;
  for (prefix, body) in logical {
    // 前缀行与正文行分别计宽（正文空行也占 1 行）
    count += wrapped_lines(&prefix, width);
    count += wrapped_lines(&body, width).max(1);
  }
  count
}

/// 单段文本按显示宽度折行后的行数（`\n` 逐逻辑行，上取整）。
fn wrapped_lines(text: &str, width: usize) -> usize {
  text
    .split('\n')
    .map(|line| {
      let w = UnicodeWidthStr::width(line);
      if w == 0 { 1 } else { w.div_ceil(width) }
    })
    .sum()
}

/// 公开包装：文本在给定显示宽度下的折行行数（浮层高度估算用）。
pub fn wrapped_text_lines(text: &str, width: u16) -> usize {
  wrapped_lines(text, usize::from(width.max(1)))
}

/// 文本块聚合：Text 块按序拼接（与 CLI render 口径一致的最小实现）。
pub fn join_text(content: &[ra_core::message::Content]) -> String {
  content
    .iter()
    .filter_map(|b| match b {
      ra_core::message::Content::Text(t) => Some(t.content.clone()),
      _ => None,
    })
    .collect::<Vec<_>>()
    .join("\n")
}

/// 转录消息单行摘要：角色 + 首个文本块（`/history` 浮层输出，
/// 与 CLI `summarize_message` 口径一致的最小实现）。
pub fn history_line(message: &ra_core::message::Message) -> String {
  match message {
    ra_core::message::Message::User(user) => {
      let text = match &user.content {
        ra_core::message::UserContent::Text(t) => t.clone(),
        ra_core::message::UserContent::Blocks(blocks) => join_text(blocks),
      };
      format!("user: {text}")
    }
    ra_core::message::Message::Assistant(assistant) => {
      let text = join_text(&assistant.content);
      if text.is_empty() {
        format!("assistant: （{:?}）", assistant.stop_reason)
      } else {
        format!("assistant: {text}")
      }
    }
    ra_core::message::Message::ToolResult(result) => {
      format!(
        "tool_result: {} ({})",
        result.tool_call_id,
        if result.is_error { "error" } else { "ok" }
      )
    }
  }
}

/// 刷新对话区行数缓存：仅宽度变化或显式失效的气泡才重算（R-R5）。
///
/// 返回全部气泡在当前宽度下的总视觉行数（粘底滚动上限依据）。
pub fn refresh_line_cache(state: &mut AppState, width: u16) -> usize {
  // 流式直写过的气泡先失效（本轮已更新内容）
  for id in state.chat.take_invalidated() {
    state.scroll.line_cache.remove(&id);
  }
  let mut total = 0usize;
  for bubble in &state.chat.messages {
    let cached = state.scroll.cached_lines(bubble.id, width);
    let count = match cached {
      Some(count) => count,
      None => {
        let count = bubble_line_count(bubble, width);
        state.scroll.store_lines(bubble.id, width, count);
        count
      }
    };
    total += count;
  }
  total
}

/// 检测流式文本中的开放栅栏并临时补闭合（16 §15 C2 / R-H2）。
///
/// 返回 `(用于渲染的文本, 原文是否本就闭合)`——补闭合仅作用于渲染，
/// 不污染 `AppState`。
pub fn ensure_closed_fence(text: &str) -> (String, bool) {
  let open = !text.matches("```").count().is_multiple_of(2);
  if open {
    (format!("{text}\n```"), false)
  } else {
    (text.to_string(), true)
  }
}

/// 切分 Markdown 代码栅段：返回 `(语言, 文本, 是否栅栏内)` 序列。
///
/// 栅栏外文本 `is_code=false`、语言为空；输入须先经
/// [`ensure_closed_fence`] 保证栅栏成对。
pub fn split_markdown_fences(text: &str) -> Vec<(&str, &str, bool)> {
  const FENCE: &str = "```";
  let mut segments = Vec::new();
  let mut rest = text;
  loop {
    match rest.find(FENCE) {
      None => {
        if !rest.is_empty() {
          segments.push(("", rest, false));
        }
        break;
      }
      Some(start) => {
        if start > 0 {
          segments.push(("", &rest[..start], false));
        }
        let after = &rest[start + FENCE.len()..];
        let (body, tail) = match after.find(FENCE) {
          Some(end) => (&after[..end], &after[end + FENCE.len()..]),
          None => (after, ""), // 输入未闭合：剩余全部视为代码体
        };
        let (lang, code) = match body.split_once('\n') {
          Some((lang, code))
            if lang
              .trim()
              .chars()
              .all(|c| c.is_alphanumeric() || c == '-' || c == '+') =>
          {
            (lang.trim(), code)
          }
          _ => ("", body),
        };
        segments.push((lang, code, true));
        rest = tail;
      }
    }
  }
  segments
}

#[cfg(test)]
mod tests {
  use super::*;
  use ra_cli::config::ConfigMode;

  fn bubble(body: ChatBubbleBody) -> ChatBubble {
    ChatBubble { id: 0, body }
  }

  // 场景：ASCII 短文本按前缀 + 正文计行。
  // 预期：前缀 1 行（64 宽内）+ 正文 1 行 = 2 行。
  #[test]
  fn ascii_short_text_counts_two_lines() {
    let b = bubble(ChatBubbleBody::User("ok".to_string()));
    assert_eq!(bubble_line_count(&b, 80), 2);
  }

  // 场景：CJK 宽字符按显示宽度折行（宽度 2/字符）。
  // 预期：40 汉字 = 80 列 → 正文 1 行（80 宽），总行 = 前缀 + 1 = 2；
  // 81 汉字 = 162 列 → 正文 3 行，总行 = 4。
  #[test]
  fn cjk_counts_by_display_width() {
    let b = bubble(ChatBubbleBody::User("汉".repeat(40)));
    assert_eq!(bubble_line_count(&b, 80), 2); // 前缀行 + 正文 1 行
    let b = bubble(ChatBubbleBody::User("汉".repeat(81)));
    assert_eq!(bubble_line_count(&b, 80), 4); // 前缀行 + 正文 3 行
  }

  // 场景：空正文占 1 行。
  #[test]
  fn empty_body_counts_one_line() {
    let b = bubble(ChatBubbleBody::User(String::new()));
    assert_eq!(bubble_line_count(&b, 80), 2);
  }

  // 场景：ensure_closed_fence 补闭合不影响原文。
  #[test]
  fn ensure_closed_fence_only_for_render() {
    let (fixed, closed) = ensure_closed_fence("a\n```rust\ncode");
    assert!(!closed);
    assert!(fixed.ends_with("```"));
    let (same, closed) = ensure_closed_fence("no fence");
    assert!(closed);
    assert_eq!(same, "no fence");
  }

  // 场景：栅栏切分（语言标记 + 栅栏外文本，含 is_code 标志）。
  #[test]
  fn split_markdown_fences_splits_segments() {
    let segments = split_markdown_fences("pre\n```rust\ncode\n```\npost");
    assert_eq!(segments.len(), 3);
    assert_eq!(segments[0], ("", "pre\n", false));
    assert_eq!(segments[1], ("rust", "code\n", true));
    assert_eq!(segments[2], ("", "\npost", false));
  }

  // 场景：无栅栏文本单段且非代码。
  #[test]
  fn split_markdown_fences_plain_text() {
    let segments = split_markdown_fences("just text");
    assert_eq!(segments.len(), 1);
    assert_eq!(segments[0], ("", "just text", false));
  }

  // 场景：refresh_line_cache 宽度变化才重算（缓存命中）。
  #[test]
  fn refresh_line_cache_respects_width() {
    let mut state =
      AppState::new(ConfigMode::Mock, Vec::new(), "default".into());
    state.chat.push(ChatBubbleBody::User("hi".into()));
    let total = refresh_line_cache(&mut state, 80);
    assert_eq!(total, 2);
    assert!(state.scroll.cached_lines(0, 80).is_some());
    assert!(state.scroll.cached_lines(0, 40).is_none(), "宽度变化未命中");
  }
}
