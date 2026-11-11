//! Tab 补全：命令候选 + 模糊匹配（16 详细设计 §7.4）。
//!
//! 候选与 CLI `/help` 同源（`ra_cli::commands::COMMANDS` 元数据表）。

use ra_cli::commands::CommandMeta;

/// 一次补全结果。
pub struct Completion {
  /// 插入到编辑器的完整文本（命令名 + 尾随空格）。
  pub text: String,
  /// 是否存在多个候选（主循环据此展示候选列表）。
  pub ambiguous: bool,
  /// 全部候选（ambiguous 时非空）。
  pub alternatives: Vec<&'static CommandMeta>,
}

/// 对当前编辑器文本执行补全。
///
/// 仅支持「首行以 `/` 开头且无空格」的命令 token；返回 `None` 表示
/// 无可补全内容（普通文本 / 无候选）。
pub fn complete(text: &str) -> Option<Completion> {
  let first_line = text.lines().next()?;
  let token = first_line.strip_prefix('/')?;
  if token.contains(' ') {
    return None; // 已带参数：不再补全
  }
  let candidates = fuzzy_candidates(token);
  match candidates.as_slice() {
    [] => None,
    [(only, name)] => Some(Completion {
      text: format!("{name} "),
      ambiguous: false,
      alternatives: vec![*only],
    }),
    list => {
      // 多候选：取最长公共前缀补齐，其余列浮层
      let common = common_prefix(list.iter().map(|(_, name)| *name));
      Some(Completion {
        text: format!("{common} "),
        ambiguous: true,
        alternatives: list.iter().map(|(meta, _)| *meta).collect(),
      })
    }
  }
}

/// 模糊候选：前缀命中优先，其后按子序列模糊匹配（对齐 pi-tui
/// fuzzy_match 语义的简化版；别名一并参与）。
///
/// 返回 `(条目, 补全名)`——命中别名时补全名取**别名形式**（与用户
/// 输入一致），命中主名取主名。前缀命中存在时忽略模糊命中
///（避免低质量子序列候选污染公共前缀）。
fn fuzzy_candidates(token: &str) -> Vec<(&'static CommandMeta, &'static str)> {
  let token = token.to_lowercase();
  if token.is_empty() {
    return ra_cli::commands::COMMANDS
      .iter()
      .map(|m| (m, m.name))
      .collect();
  }
  let mut prefixed = Vec::new();
  let mut fuzzy = Vec::new();
  for meta in ra_cli::commands::COMMANDS {
    let names = std::iter::once(meta.name).chain(meta.aliases.iter().copied());
    for name in names {
      let bare = name.trim_start_matches('/').to_lowercase();
      if bare.starts_with(&token) {
        prefixed.push((meta, name));
        break;
      }
      if fuzzy_match(&bare, &token) {
        fuzzy.push((meta, name));
        break;
      }
    }
  }
  if !prefixed.is_empty() {
    prefixed
  } else {
    fuzzy
  }
}

/// 子序列模糊匹配（按序包含 token 全部字符）。
fn fuzzy_match(name: &str, token: &str) -> bool {
  let mut chars = name.chars();
  token.chars().all(|tc| chars.any(|nc| nc == tc))
}

/// 多候选的最长公共前缀（含 `/` 前缀）。
fn common_prefix<'a>(names: impl Iterator<Item = &'a str>) -> String {
  let mut names: Vec<&str> = names.collect();
  if names.is_empty() {
    return String::new();
  }
  names.sort_unstable();
  let first = names.first().expect("非空");
  let last = names.last().expect("非空");
  let mut prefix = String::new();
  for (a, b) in first.chars().zip(last.chars()) {
    if a != b {
      break;
    }
    prefix.push(a);
  }
  prefix
}

#[cfg(test)]
mod tests {
  use super::*;

  // 场景：前缀补全（唯一命中 → 命令名 + 尾随空格）。
  #[test]
  fn unique_prefix_completes_with_space() {
    let completion = complete("/se").expect("/se 应命中 /sessions");
    assert_eq!(completion.text, "/sessions ");
    assert!(!completion.ambiguous);
  }

  // 场景：多候选 → 最长公共前缀 + 候选列表。
  #[test]
  fn ambiguous_completion_reports_alternatives() {
    // /s 命中 /stop /skills /sessions /status
    let completion = complete("/s").expect("/s 应有多候选");
    assert!(completion.ambiguous);
    assert!(completion.alternatives.len() >= 3);
    assert!(completion.text.starts_with("/s"));
  }

  // 场景：别名参与模糊匹配（/c 命中 /cancel）。
  #[test]
  fn aliases_participate() {
    let completion = complete("/ca").expect("/ca 应命中 /cancel 别名");
    assert_eq!(completion.text, "/cancel ");
  }

  // 场景：带空格（已有参数）不补全；普通文本不补全。
  #[test]
  fn no_completion_for_args_or_plain_text() {
    assert!(complete("/new work").is_none());
    assert!(complete("hello").is_none());
    assert!(complete("/zzz").is_none());
  }

  // 场景：空 token（仅 "/"）列出全部命令。
  #[test]
  fn bare_slash_lists_all() {
    let completion = complete("/").expect("/ 应列出全部");
    assert!(completion.ambiguous);
    assert_eq!(
      completion.alternatives.len(),
      ra_cli::commands::COMMANDS.len()
    );
  }
}
