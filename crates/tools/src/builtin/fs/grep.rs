use std::{
  path::{Path, PathBuf},
  sync::Arc,
};

use ra_core::{
  error::RiboResult,
  message::{Schema, TextContent, ToolSchema},
};

use crate::{
  context::ToolContext,
  error::{exec_failed, invalid_arguments},
  traits::AgentTool,
  types::{ToolExecutionResult, ToolOutput},
};

use super::{
  FsRoot, check_cancel,
  constants::{GREP_MAX_DEPTH, GREP_MAX_FILE_BYTES},
};

/// grep 工具：正则搜索。
pub struct GrepTool {
  /// 构造期固化的工具声明。
  schema: ToolSchema,
  /// 沙箱根。
  root: FsRoot,
}

impl GrepTool {
  /// 构造并固化 schema 与沙箱根。
  pub(super) fn new(root: FsRoot) -> Self {
    Self {
      schema: ToolSchema {
        name: "grep".to_string(),
        description: "Search file contents with a Rust regex. Recurses \
directories, skipping large and binary files. Output: 'path:line: text'."
          .to_string(),
        input_schema: Schema(serde_json::json!({
          "type": "object",
          "properties": {
            "pattern": { "type": "string", "description": "Regex (Rust syntax)" },
            "path": { "type": "string", "description": "File or directory to search" },
            "ignore_case": { "type": "boolean", "description": "Case-insensitive match (default false)" },
            "max_results": { "type": "integer", "minimum": 1, "description": "Matched-line limit (default 100)" }
          },
          "required": ["pattern", "path"],
          "additionalProperties": false
        })),
        constrained_sampling: None,
      },
      root,
    }
  }
}

/// 递归搜索状态（聚合参数）。
struct GrepState {
  /// 已编译正则。
  re: regex::Regex,
  /// 命中行数上限。
  max_results: usize,
  /// 输出行（`{rel_path}:{line_no}: {line}`）。
  lines: Vec<String>,
  /// 已命中文件数。
  matched_files: usize,
  /// 是否达到上限截断。
  truncated: bool,
}

/// 判断文件是否二进制：头部 8KB 含 NUL 字节。
async fn is_binary(path: &Path) -> bool {
  use tokio::io::AsyncReadExt;
  let mut f = match tokio::fs::File::open(path).await {
    Ok(f) => f,
    Err(_) => return true,
  };
  let mut head = [0u8; 8192];
  match f.read(&mut head).await {
    Ok(n) => head[..n].contains(&0),
    Err(_) => true,
  }
}

/// 跳过判定：超过大小阈值或二进制文件。
async fn skip_reason(meta_len: u64, path: &Path) -> Option<String> {
  if meta_len > GREP_MAX_FILE_BYTES {
    return Some("file too large".to_string());
  }
  if is_binary(path).await {
    return Some("binary file".to_string());
  }
  None
}

/// 行内容展示：trim 后截 200 字符。
fn truncate_line(line: &str) -> String {
  let t = line.trim();
  if t.chars().count() > 200 {
    let s: String = t.chars().take(200).collect();
    format!("{s}...")
  } else {
    t.to_string()
  }
}

/// 搜索单文件：逐行匹配并追加输出行；读取失败跳过不整体失败。
async fn search_file(
  path: &Path,
  root: &Path,
  st: &mut GrepState,
  ctx: &ToolContext,
) -> RiboResult<()> {
  // ===== 跳过判定（元数据 / 大小 / 二进制） =====
  let meta = match tokio::fs::metadata(path).await {
    Ok(m) => m,
    Err(e) => {
      tracing::debug!(path = %path.display(), reason = %e, "grep file skipped");
      return Ok(());
    }
  };
  if let Some(reason) = skip_reason(meta.len(), path).await {
    tracing::debug!(path = %path.display(), reason = %reason, "grep file skipped");
    return Ok(());
  }
  check_cancel(ctx, "grep")?;
  // ===== 逐行匹配 =====
  let text = match tokio::fs::read_to_string(path).await {
    Ok(t) => t,
    // 非 UTF-8 / 权限等：跳过并记 debug，不整体失败
    Err(e) => {
      tracing::debug!(path = %path.display(), reason = %e, "grep file skipped");
      return Ok(());
    }
  };
  let rel = path
    .strip_prefix(root)
    .unwrap_or(path)
    .to_string_lossy()
    .into_owned();
  let mut hit = false;
  for (i, line) in text.lines().enumerate() {
    if st.re.is_match(line) {
      st.lines
        .push(format!("{rel}:{}: {}", i + 1, truncate_line(line)));
      hit = true;
      if st.lines.len() >= st.max_results {
        st.truncated = true;
        break;
      }
    }
  }
  if hit {
    st.matched_files += 1;
  }
  Ok(())
}

/// 递归搜索目录（async 递归经 Box::pin；深度受限；符号链接不跟随）。
async fn walk_grep(
  dir: &Path,
  root: &Path,
  depth: usize,
  st: &mut GrepState,
  ctx: &ToolContext,
) -> RiboResult<()> {
  // 深度上限或已截断：短路
  if st.truncated || depth > GREP_MAX_DEPTH {
    return Ok(());
  }
  check_cancel(ctx, "grep")?;
  let mut rd = tokio::fs::read_dir(dir)
    .await
    .map_err(|e| exec_failed("grep", format!("read_dir failed: {e}")))?;
  while let Some(entry) = rd
    .next_entry()
    .await
    .map_err(|e| exec_failed("grep", format!("read_dir failed: {e}")))?
  {
    let p: PathBuf = entry.path();
    let ft = entry
      .file_type()
      .await
      .map_err(|e| exec_failed("grep", format!("file_type failed: {e}")))?;
    if ft.is_dir() {
      // 目录下钻（深度受限）
      Box::pin(walk_grep(&p, root, depth + 1, st, ctx)).await?;
    } else if ft.is_file() {
      search_file(&p, root, st, ctx).await?;
      if st.lines.len() >= st.max_results {
        st.truncated = true;
        return Ok(());
      }
    }
  }
  Ok(())
}

#[async_trait::async_trait]
impl AgentTool for GrepTool {
  fn schema(&self) -> &ToolSchema {
    &self.schema
  }

  fn label(&self) -> &str {
    "Grep"
  }

  async fn execute(
    &self,
    _tool_call_id: &str,
    args: serde_json::Value,
    ctx: &ToolContext,
  ) -> RiboResult<ToolExecutionResult> {
    // ===== 取参 =====
    let pattern = args["pattern"].as_str().ok_or_else(|| {
      invalid_arguments("grep", "field 'pattern' must be a string")
    })?;
    let path_raw = args["path"].as_str().ok_or_else(|| {
      invalid_arguments("grep", "field 'path' must be a string")
    })?;
    let ignore_case = args["ignore_case"].as_bool().unwrap_or(false);
    let max_results = args["max_results"].as_u64().unwrap_or(100) as usize;
    // ===== 正则编译（ignore_case 经内联标志 (?i)） =====
    // 正则非法属入参问题 → INVALID_ARGUMENTS
    let regex_src = if ignore_case {
      format!("(?i){pattern}")
    } else {
      pattern.to_string()
    };
    let re = regex::Regex::new(&regex_src)
      .map_err(|e| invalid_arguments("grep", format!("invalid regex: {e}")))?;
    let target = self.root.resolve(path_raw)?;
    check_cancel(ctx, "grep")?;
    // ===== 目标分发：单文件 / 目录递归 =====
    let mut st = GrepState {
      re,
      max_results,
      lines: Vec::new(),
      matched_files: 0,
      truncated: false,
    };
    if target.is_file() {
      search_file(&target, self.root.root(), &mut st, ctx).await?;
    } else {
      walk_grep(&target, self.root.root(), 0, &mut st, ctx).await?;
    }
    // ===== 输出（无命中不视为错误） =====
    let mut text = if st.lines.is_empty() {
      "(no matches)".to_string()
    } else {
      st.lines.join("\n")
    };
    if st.truncated {
      text.push_str(&format!("\n[stopped at max_results={}]", st.max_results));
    }
    Ok(ToolExecutionResult {
      content: vec![ToolOutput::Text(TextContent {
        content: text,
        signature: None,
      })],
      details: Some(
        serde_json::json!({ "matched_files": st.matched_files, "truncated": st.truncated }),
      ),
      ..Default::default()
    })
  }
}

/// 工厂函数。
pub fn create_grep_tool(root: &FsRoot) -> Arc<dyn AgentTool> {
  Arc::new(GrepTool::new(root.clone()))
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use serde_json::json;

  use super::*;
  use crate::{builtin::fs::test_support::TempRoot, error::INVALID_ARGUMENTS};

  /// 在唯一临时沙箱根上构造 grep 工具（返回句柄以维持目录存活）。
  fn grep_tool(tag: &str) -> (TempRoot, Arc<dyn AgentTool>) {
    let tmp = TempRoot::new(tag);
    let root = FsRoot::new(tmp.path()).expect("temp sandbox root exists");
    (tmp, create_grep_tool(&root))
  }

  /// 取结果首个文本块内容。
  fn first_text(result: &ToolExecutionResult) -> &str {
    match &result.content[0] {
      ToolOutput::Text(t) => &t.content,
      ToolOutput::Image(_) => panic!("expected text output"),
    }
  }

  /// 场景：目录递归搜索命中行。
  ///
  /// 预期：输出为 "code.txt:1: hello rust"（path:line: text 格式，
  /// 相对路径 + 1 起始行号 + trim 后的行文本）。
  #[tokio::test]
  async fn grep_reports_path_line_text() {
    let (tmp, tool) = grep_tool("grep-hit");
    std::fs::write(tmp.path().join("code.txt"), "hello rust\nsecond line\n")
      .expect("seed file");

    let result = tool
      .execute(
        "call-1",
        json!({ "pattern": "hello", "path": "." }),
        &ToolContext::detached(),
      )
      .await
      .expect("grep over sandbox should succeed");
    assert_eq!(first_text(&result), "code.txt:1: hello rust");
  }

  /// 场景：非法正则。
  ///
  /// 预期：未闭合分组 "(" → `tool.invalid_arguments`。
  #[tokio::test]
  async fn invalid_regex_maps_to_invalid_arguments() {
    let (_tmp, tool) = grep_tool("grep-bad");
    let err = tool
      .execute(
        "call-1",
        json!({ "pattern": "(", "path": "." }),
        &ToolContext::detached(),
      )
      .await
      .unwrap_err();
    assert_eq!(err.code(), INVALID_ARGUMENTS);
  }

  /// 场景：无命中。
  ///
  /// 预期：输出 "(no matches)"，不视为错误。
  #[tokio::test]
  async fn no_match_yields_placeholder() {
    let (tmp, tool) = grep_tool("grep-empty");
    std::fs::write(tmp.path().join("code.txt"), "hello rust\n")
      .expect("seed file");

    let result = tool
      .execute(
        "call-1",
        json!({ "pattern": "zebra", "path": "." }),
        &ToolContext::detached(),
      )
      .await
      .expect("no match is not an error");
    assert_eq!(first_text(&result), "(no matches)");
  }
}
