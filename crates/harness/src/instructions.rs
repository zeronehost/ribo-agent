//! 域内 `AGENTS.md` 指令加载与拼接。
//!
//! **加载策略**：
//! - 只认精确文件名 `AGENTS.md`（`agents.md` / `CLAUDE.md` / `AGENTS.txt`
//!   一律不识别）；
//! - 只读**域根单文件**：不向上递归（`root/../AGENTS.md`），不向下递归
//!   （`root/sub/AGENTS.md`）——本产品与 pi-agent 的关键行为差异；
//! - 拼接顺序 Global → Project → Plugin[*]（声明序），每段带
//!   `<!-- source: ... -->` 标注；矛盾不自动裁决，全文保留；
//! - 单文件 32KB 上限，超长按 UTF-8 字符边界截断并附提示；
//! - 零文件 → [`Instructions::is_empty()`] 为 true，调用方不产系统提示段。
//!
//! `AGENTS.md` 属文档级资源，**默认加载**，与代码信任无关（设计）。

use std::path::PathBuf;

/// 单个 `AGENTS.md` 的大小上限（字节）。
pub const MAX_AGENTS_BYTES: usize = 32 * 1024;

/// 一段已加载的指令来源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionSource {
  /// 来源域标签（`global` / `project` / `plugin:<id>`）。
  pub origin: String,
  /// 文件路径（域根下的 `AGENTS.md`）。
  pub path: PathBuf,
  /// 是否因超限被截断。
  pub truncated: bool,
}

/// 拼接后的指令集合。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Instructions {
  /// 拼接后的正文（含 source 标注）；空集时为空串。
  pub content: String,
  /// 各来源（顺序与拼接一致）。
  pub sources: Vec<InstructionSource>,
}

impl Instructions {
  /// 是否没有任何指令文件。
  pub fn is_empty(&self) -> bool {
    self.sources.is_empty()
  }
}

/// 一个待加载的 `AGENTS.md` 声明：域根 + 来源标签。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionRoot {
  /// 域根目录（`AGENTS.md` 的直接父目录）。
  pub path: PathBuf,
  /// 来源域标签。
  pub origin: String,
}

impl InstructionRoot {
  /// 创建声明。
  pub fn new(path: PathBuf, origin: impl Into<String>) -> Self {
    Self {
      path,
      origin: origin.into(),
    }
  }
}

/// 按声明顺序加载并拼接所有域的 `AGENTS.md`。
///
/// 单文件读取失败（不存在 / 非 UTF-8）静默跳过——`AGENTS.md` 是文档级
/// 资源，问题只记 warning 日志，不中断装配（域故障隔离）。
/// 超限文件截断到 [`MAX_AGENTS_BYTES`] 内的 UTF-8 字符边界。
pub fn load_instructions(roots: &[InstructionRoot]) -> Instructions {
  let mut content = String::new();
  let mut sources = Vec::new();

  for root in roots {
    let path = root.path.join("AGENTS.md");
    // 精确单文件：不递归、不尝试文件名变体（设计「禁止」）
    if !path.is_file() {
      continue;
    }
    let raw = match std::fs::read_to_string(&path) {
      Ok(text) => text,
      Err(err) => {
        tracing::warn!(
          origin = %root.origin,
          error = %err,
          "AGENTS.md read failed, skipped"
        );
        continue;
      }
    };
    let (text, truncated) = truncate_utf8(&raw, MAX_AGENTS_BYTES);
    if truncated {
      tracing::warn!(
        origin = %root.origin,
        limit = MAX_AGENTS_BYTES,
        "AGENTS.md exceeds size limit, truncated"
      );
    }
    if !content.is_empty() {
      content.push_str("\n\n");
    }
    content.push_str(&format!(
      "<!-- source: {} ({}) -->\n{}",
      root.origin,
      path.display(),
      text
    ));
    sources.push(InstructionSource {
      origin: root.origin.clone(),
      path,
      truncated,
    });
  }

  Instructions { content, sources }
}

/// 按 UTF-8 字符边界截断到 `max_bytes`，返回（文本, 是否截断）。
///
/// 复用 `plugins/error.rs:truncate_error` 的边界回退思路，但不加
/// `... [truncated]` 后缀——此处由调用方在 source 标注里记录截断状态，
/// 避免注入模型的正文出现非原文标记。
fn truncate_utf8(text: &str, max_bytes: usize) -> (String, bool) {
  if text.len() <= max_bytes {
    return (text.to_string(), false);
  }
  let mut end = max_bytes.min(text.len());
  while end > 0 && !text.is_char_boundary(end) {
    end -= 1;
  }
  (text[..end].to_string(), true)
}

#[cfg(test)]
mod tests {
  use crate::test_util::test_dir;

  use super::*;

  /// 场景：零文件 → Instructions 为空，不产任何内容。
  #[test]
  fn no_agents_file_yields_empty_instructions() {
    let root = test_dir("no_agents_file_yields_empty");
    let out = load_instructions(&[InstructionRoot::new(root, "project")]);
    assert!(out.is_empty());
    assert!(out.content.is_empty());
  }

  /// 场景：关键——不向上递归（`../AGENTS.md` 不加载）。
  #[test]
  fn no_upward_recursion() {
    let parent = test_dir("no_upward_recursion-parent");
    let child = parent.join("child");
    std::fs::create_dir_all(&child).expect("child dir");
    std::fs::write(parent.join("AGENTS.md"), "PARENT CONTENT").expect("seed");

    let out = load_instructions(&[InstructionRoot::new(child, "project")]);
    assert!(out.is_empty(), "parent AGENTS.md must NOT be loaded (P2)");
    assert!(!out.content.contains("PARENT CONTENT"));
  }

  /// 场景：关键——不向下递归（`sub/AGENTS.md` 不加载）。
  #[test]
  fn no_downward_recursion() {
    let root = test_dir("no_downward_recursion");
    let sub = root.join("sub");
    std::fs::create_dir_all(&sub).expect("sub dir");
    std::fs::write(sub.join("AGENTS.md"), "SUB CONTENT").expect("seed");

    let out = load_instructions(&[InstructionRoot::new(root, "project")]);
    assert!(out.is_empty(), "sub AGENTS.md must NOT be loaded (P2)");
  }

  /// 场景：文件名变体不识别。
  #[test]
  fn only_exact_filename_is_recognized() {
    let root = test_dir("only_exact_filename");
    // 注：大小写变体（agents.md）在 Windows 大小写不敏感文件系统上
    // 与 AGENTS.md 是同一文件，无法区分；这里只测真正不同的文件名。
    std::fs::write(root.join("CLAUDE.md"), "claude").expect("seed");
    std::fs::write(root.join("AGENTS.txt"), "txt").expect("seed");
    let out = load_instructions(&[InstructionRoot::new(root, "project")]);
    assert!(out.is_empty(), "only exact AGENTS.md is recognized");
  }

  /// 场景：Global → Project 顺序拼接，每段带 source 标注。
  #[test]
  fn concatenates_in_order_with_source_annotations() {
    let g = test_dir("concat-global");
    let p = test_dir("concat-project");
    std::fs::write(g.join("AGENTS.md"), "GLOBAL CONTENT").expect("seed");
    std::fs::write(p.join("AGENTS.md"), "PROJECT CONTENT").expect("seed");

    let out = load_instructions(&[
      InstructionRoot::new(g, "global"),
      InstructionRoot::new(p, "project"),
    ]);
    assert!(!out.is_empty());
    assert_eq!(out.sources.len(), 2);
    assert_eq!(out.sources[0].origin, "global");
    assert_eq!(out.sources[1].origin, "project");
    let g_pos = out.content.find("GLOBAL CONTENT").expect("global text");
    let p_pos = out.content.find("PROJECT CONTENT").expect("project text");
    assert!(g_pos < p_pos, "global must precede project");
    assert!(out.content.contains("<!-- source: global ("));
    assert!(out.content.contains("<!-- source: project ("));
  }

  /// 场景：超限按 UTF-8 字符边界截断（不切坏汉字）。
  #[test]
  fn truncates_on_utf8_boundary() {
    let root = test_dir("truncate_utf8_boundary");
    // 3 字节汉字重复，确保 max_bytes 落在字符中间
    let text = "汉".repeat(MAX_AGENTS_BYTES / 3 + 10);
    std::fs::write(root.join("AGENTS.md"), &text).expect("seed");
    let out = load_instructions(&[InstructionRoot::new(root, "project")]);
    assert_eq!(out.sources.len(), 1);
    assert!(out.sources[0].truncated);
    // 正文必须是合法 UTF-8（String 保证），且不含 source 标注之外的截断标记
    assert!(!out.content.contains("... [truncated]"));
  }

  /// 场景：纯 `truncate_utf8` 的边界正确性。
  #[test]
  fn truncate_utf8_respects_char_boundary() {
    let text = "abc汉def";
    let (head, truncated) = truncate_utf8(text, 4); // 切在汉字中间
    assert!(truncated);
    assert_eq!(head, "abc"); // 回退到边界，不切坏汉字
    let (full, t2) = truncate_utf8(text, 100);
    assert!(!t2);
    assert_eq!(full, text);
  }
}
