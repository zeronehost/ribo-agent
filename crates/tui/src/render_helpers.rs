//! 渲染辅助：气泡折行（自折行真值）/ 行数缓存刷新 / Markdown 栅栏处理。

use crate::state::{AppState, ChatBubble, ChatBubbleBody};

/// 渲染区默认宽度兜底（首次 draw 前的估算口径）。
pub const FALLBACK_WIDTH: u16 = 80;

/// 按显示宽度把单行文本折成视觉行（字符级贪心，CJK 宽字符友好）。
///
/// **渲染与行数估算共用的唯一折行真值**——`DialogView` 渲染时用它把
/// 逻辑行折成视觉行（Paragraph 不再启用 WordWrapper），行数缓存、滚动
/// 偏移与实际渲染因此严格一致，根治「内容多时尾部渲染不出来」的
/// 口径漂移（此前渲染走 ratatui WordWrapper 按词折、估算走 div_ceil
/// 按字符硬折，两套逻辑永远存在漂移）。
pub fn hard_wrap(text: &str, width: usize) -> Vec<String> {
  let width = width.max(4);
  if text.is_empty() {
    return vec![String::new()];
  }
  let mut rows: Vec<String> = Vec::new();
  let mut current = String::new();
  let mut current_w = 0usize;
  for ch in text.chars() {
    let ch_w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
    if current_w + ch_w > width && !current.is_empty() {
      rows.push(std::mem::take(&mut current));
      current_w = 0;
    }
    current.push(ch);
    current_w += ch_w;
  }
  if !current.is_empty() {
    rows.push(current);
  }
  if rows.is_empty() {
    rows.push(String::new());
  }
  rows
}

/// 逐逻辑行（`\n` 切分）折行后的总视觉行数；空文本占 1 行。
fn logical_wrapped_rows(text: &str, width: usize) -> usize {
  if text.is_empty() {
    return 1;
  }
  text
    .split('\n')
    .map(|line| hard_wrap(line, width).len())
    .sum()
}

/// Assistant 正文段折行后的视觉行数（Markdown 栅栏拆分口径，与
/// `DialogView` 的 `markdown_lines` 渲染一一对应）。
///
/// 代码段 = ```lang 行 + 代码体逐行 + ``` 行；非代码段去掉栅栏分隔
/// 带来的前/后导换行（与渲染端 trim 一致）。
fn markdown_row_count(text: &str, width: usize) -> usize {
  let (rendered, _) = ensure_closed_fence(text);
  split_markdown_fences(&rendered)
    .iter()
    .map(|(lang, body, is_code)| {
      if *is_code {
        hard_wrap(&format!("```{lang}"), width).len()
          + logical_wrapped_rows(body.trim_end_matches('\n'), width)
          + hard_wrap("```", width).len()
      } else {
        logical_wrapped_rows(
          body.trim_end_matches('\n').trim_start_matches('\n'),
          width,
        )
      }
    })
    .sum()
}

/// 单个气泡在给定宽度下折成的视觉行数（含角色前缀行 + 分隔空行）。
///
/// 与 `DialogView` 的 `bubble_lines`（自折行渲染）**共用 `hard_wrap`**，
/// 行数与实际渲染严格一致——这是滚动偏移与粘底的唯一真值。
pub fn bubble_line_count(bubble: &ChatBubble, width: u16) -> usize {
  let width = usize::from(width.max(4));
  let mut count = 0usize;
  match &bubble.body {
    ChatBubbleBody::User(text) => {
      count += hard_wrap("you › ", width).len();
      count += logical_wrapped_rows(text, width);
    }
    ChatBubbleBody::Assistant {
      text,
      thinking,
      error,
    } => {
      if !thinking.is_empty() {
        count += hard_wrap("think › ", width).len();
        // 渲染端对 thinking 也做 ensure_closed_fence（口径一致）
        count += logical_wrapped_rows(&ensure_closed_fence(thinking).0, width);
      }
      count += hard_wrap("assistant › ", width).len();
      count += markdown_row_count(text, width);
      if let Some(err) = error {
        count += hard_wrap("error › ", width).len();
        count += logical_wrapped_rows(err, width);
      }
    }
    ChatBubbleBody::ToolResult {
      tool,
      text,
      is_error,
      running,
      ..
    } => {
      // 前缀与渲染端 bubble_lines 一致：✗ 失败 / … 执行中 / ✓ 完成
      let marker = if *is_error {
        "✗"
      } else if *running {
        "…"
      } else {
        "✓"
      };
      count += hard_wrap(&format!("tool {marker} {tool} › "), width).len();
      count += logical_wrapped_rows(text, width);
    }
    ChatBubbleBody::Compaction { failed } => {
      // 前缀与文案常量与渲染端 bubble_lines 共用（口径一致）
      count += hard_wrap(ChatBubbleBody::COMPACTION_PREFIX, width).len();
      let text = if *failed {
        ChatBubbleBody::COMPACTION_FAILED_TEXT
      } else {
        ChatBubbleBody::COMPACTION_TEXT
      };
      count += logical_wrapped_rows(text, width);
    }
  }
  // DialogView::render 每气泡后追加一个空行分隔
  count + 1
}

/// 单段文本按显示宽度折行后的行数（`\n` 逐逻辑行，字符级硬折）。
///
/// 供浮层高度估算使用；对话区行数请用 [`bubble_line_count`]（含栅栏
/// 口径），两者共用 [`hard_wrap`]。
fn wrapped_lines(text: &str, width: usize) -> usize {
  logical_wrapped_rows(text, width)
}

/// 公开包装：文本在给定显示宽度下的折行行数（浮层高度估算用）。
pub fn wrapped_text_lines(text: &str, width: u16) -> usize {
  wrapped_lines(text, usize::from(width.max(1)))
}

/// token 数量的紧凑显示（K / M）。
///
/// 规则：`< 1000` 原样；`< 1_000_000` 用 K（一位小数）；其余用 M（一位小数）。
/// 四舍五入后进位到下一单位（如 999_999 → `1.0M`），避免出现 `1000.0K`。
///
/// # Examples
///
/// ```
/// use ra_tui::render_helpers::format_compact_tokens;
///
/// assert_eq!(format_compact_tokens(758), "758");
/// assert_eq!(format_compact_tokens(7586), "7.6K");
/// assert_eq!(format_compact_tokens(999_999), "1.0M");
/// assert_eq!(format_compact_tokens(885_000), "13.9M");
/// ```
pub fn format_compact_tokens(tokens: usize) -> String {
  const THOUSAND: f64 = 1000.0;
  const MILLION: f64 = 1_000_000.0;
  let value = tokens as f64;
  if value >= MILLION {
    format!("{:.1}M", value / MILLION)
  } else if value >= THOUSAND {
    let scaled = value / THOUSAND;
    // 四舍五入到一位小数后若进位到 1000，改用 M（如 999_999 → 1.0M）
    if (scaled * 10.0).round() >= 10_000.0 {
      format!("{:.1}M", scaled / THOUSAND)
    } else {
      format!("{scaled:.1}K")
    }
  } else {
    tokens.to_string()
  }
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

/// 检测流式文本中的开放栅栏并临时补闭合。
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

  // 场景：token 大数紧凑显示。
  // 预期：<1000 原样；K/M 一位小数；999_999 进位为 1.0M。
  #[test]
  fn format_compact_tokens_uses_k_and_m() {
    assert_eq!(format_compact_tokens(0), "0");
    assert_eq!(format_compact_tokens(758), "758");
    assert_eq!(format_compact_tokens(999), "999");
    assert_eq!(format_compact_tokens(1000), "1.0K");
    assert_eq!(format_compact_tokens(1502), "1.5K");
    assert_eq!(format_compact_tokens(7586), "7.6K");
    assert_eq!(format_compact_tokens(13_885), "13.9K");
    assert_eq!(format_compact_tokens(999_000), "999.0K");
    assert_eq!(format_compact_tokens(999_999), "1.0M");
    assert_eq!(format_compact_tokens(1_000_000), "1.0M");
    assert_eq!(format_compact_tokens(13_885_000), "13.9M");
  }

  // 场景：ASCII 短文本按前缀 + 正文 + 分隔空行计行。
  // 预期：前缀 1 + 正文 1 + 分隔 1 = 3 行。
  #[test]
  fn ascii_short_text_counts_two_lines() {
    let b = bubble(ChatBubbleBody::User("ok".to_string()));
    assert_eq!(bubble_line_count(&b, 80), 3);
  }

  // 场景：CJK 宽字符按显示宽度折行（宽度 2/字符）。
  // 预期：40 汉字 = 80 列 → 正文 1 行，总行 = 前缀 1 + 正文 1 + 分隔 1 = 3；
  // 81 汉字 = 162 列 → 正文 3 行，总行 = 1 + 3 + 1 = 5。
  #[test]
  fn cjk_counts_by_display_width() {
    let b = bubble(ChatBubbleBody::User("汉".repeat(40)));
    assert_eq!(bubble_line_count(&b, 80), 3);
    let b = bubble(ChatBubbleBody::User("汉".repeat(81)));
    assert_eq!(bubble_line_count(&b, 80), 5);
  }

  // 场景：空正文占 1 行 + 前缀 + 分隔 = 3 行。
  #[test]
  fn empty_body_counts_one_line() {
    let b = bubble(ChatBubbleBody::User(String::new()));
    assert_eq!(bubble_line_count(&b, 80), 3);
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
    let mut state = AppState::new(
      ConfigMode::Mock,
      "mock".into(),
      "mock-model".into(),
      Vec::new(),
      "default".into(),
    );
    state.chat.push(ChatBubbleBody::User("hi".into()));
    let total = refresh_line_cache(&mut state, 80);
    assert_eq!(total, 3); // 前缀 + 正文 + 分隔空行
    assert!(state.scroll.cached_lines(0, 80).is_some());
    assert!(state.scroll.cached_lines(0, 40).is_none(), "宽度变化未命中");
  }
}
