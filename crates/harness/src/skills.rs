//! 技能扫描与加载器。
//!
//! 设计取舍：frontmatter 手写解析，不引入 `serde_yaml`；
//! 技能问题一律**不致命**——加载器返回可用集合 + 诊断，
//! 调用方把诊断写日志后继续装配。

use std::{
  ffi::OsStr,
  path::{Path, PathBuf},
};

/// 一个已加载的技能。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
  /// 技能名（frontmatter `name`，缺省回退文件名主干）。
  pub name: String,
  /// 一句话描述（frontmatter `description`，缺省为空串）。
  pub description: String,
  /// 技能正文（frontmatter 之后的全部内容）。
  pub content: String,
  /// 来源文件路径（绝对，装配时一次性解析）。
  pub location: PathBuf,
  /// 来源域标签：`project` / `global` / `plugin:<id>`。
  /// 单根旧构造器下恒为 `project`，保持零配置行为不变。
  pub origin: String,
}

/// 带来源域标签的扫描根。
///
/// `roots` 顺序即优先级：同名技能**首个 roots 胜出**。装配层按
/// Project → Global → Plugin[*]（声明序）排列，实现设计的优先级。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillRoot {
  /// 扫描根路径。
  pub path: PathBuf,
  /// 来源域标签（写入 [`Skill::origin`]）。
  pub origin: String,
}

impl SkillRoot {
  /// 创建带来源的扫描根。
  pub fn new(path: PathBuf, origin: impl Into<String>) -> Self {
    Self {
      path,
      origin: origin.into(),
    }
  }
}

/// 技能诊断码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillDiagnosticCode {
  /// 元信息读取失败（目录缺 SKILL.md / 文件类型不可判定）。
  FileInfoFailed,
  /// 扫描根 `read_dir` 失败（权限 / 不存在）。
  ListFailed,
  /// 文件读取失败（不存在 / 非 UTF-8 / 权限）。
  ReadFailed,
  /// frontmatter 解析失败（缺分隔符 / 缺键值结构）。
  ParseFailed,
  /// 元信息非法（空名 / 超长 / 重名）。
  InvalidMetadata,
}

/// 单条技能诊断（加载期产的告警，不中断整体加载）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDiagnostic {
  /// 相关路径；扫描根级失败时为 `None`。
  pub location: Option<PathBuf>,
  /// 诊断码。
  pub code: SkillDiagnosticCode,
  /// 面向开发者的英文说明（不记录正文）。
  pub message: String,
}

/// 技能扫描与加载器（同步 IO，面向装配期调用）。
pub struct SkillsLoader {
  roots: Vec<SkillRoot>,
  max_name_chars: usize,
  max_description_chars: usize,
}

impl SkillsLoader {
  /// 以扫描根创建加载器（默认上限：name 64 / description 1024）。
  ///
  /// 所有根的来源域标签为 `project`（兼容旧调用；多域场景用
  /// [`Self::with_origins`]）。
  pub fn new(roots: Vec<PathBuf>) -> Self {
    Self::with_origins(
      roots
        .into_iter()
        .map(|p| SkillRoot::new(p, "project"))
        .collect(),
    )
  }

  /// 以带来源域标签的扫描根创建加载器。
  pub fn with_origins(roots: Vec<SkillRoot>) -> Self {
    Self {
      roots,
      max_name_chars: 64,
      max_description_chars: 1024,
    }
  }

  /// 覆盖名字段上限。
  pub fn with_max_name_chars(mut self, n: usize) -> Self {
    self.max_name_chars = n;
    self
  }

  /// 覆盖描述字段上限。
  pub fn with_max_description_chars(mut self, n: usize) -> Self {
    self.max_description_chars = n;
    self
  }

  /// 扫描全部根目录。
  ///
  /// 任何单技能问题只产 [`SkillDiagnostic`]，不中断整体加载。
  /// 返回的技能按 `name` 升序、同名首个根目录优先。
  ///
  /// # 阻塞说明
  ///
  /// 本方法使用同步 IO，面向装配期调用（非热路径）；若在 async 上下文内调用，
  /// 建议由调用方置入 `tokio::task::spawn_blocking`。
  pub fn load(&self) -> (Vec<Skill>, Vec<SkillDiagnostic>) {
    let mut diagnostics = Vec::new();
    let mut skills = Vec::new();
    let mut names = std::collections::HashSet::new();

    for root in &self.roots {
      match std::fs::read_dir(&root.path) {
        Ok(entries) => {
          for entry in entries.flatten() {
            let (skill, diag) =
              self.load_entry(&entry, &mut names, &root.origin);
            if let Some(d) = diag {
              diagnostics.push(d);
            }
            if let Some(s) = skill {
              skills.push(s);
            }
          }
        }
        Err(err) => {
          diagnostics.push(SkillDiagnostic {
            location: Some(root.path.clone()),
            code: SkillDiagnosticCode::ListFailed,
            message: format!("read_dir failed: {err}"),
          });
        }
      }
    }

    // 按 name 升序排序（同名已在加载阶段去重，这里直接 sort）
    skills.sort_by(|a, b| a.name.cmp(&b.name));

    tracing::info!(
      skill_count = skills.len(),
      diagnostic_count = diagnostics.len(),
      "skills scan completed"
    );

    (skills, diagnostics)
  }

  // 内部辅助

  fn load_entry(
    &self,
    entry: &std::fs::DirEntry,
    names: &mut std::collections::HashSet<String>,
    origin: &str,
  ) -> (Option<Skill>, Option<SkillDiagnostic>) {
    let path = entry.path();
    let Some(file_name) = path.file_name() else {
      return (None, None);
    };
    let name_str = file_name.to_string_lossy();
    // 隐藏项跳过
    if name_str.starts_with('.') {
      return (None, None);
    }

    let ft = match entry.file_type() {
      Ok(ft) => ft,
      Err(err) => {
        return (
          None,
          Some(SkillDiagnostic {
            location: Some(path),
            code: SkillDiagnosticCode::FileInfoFailed,
            message: format!("file_type failed: {err}"),
          }),
        );
      }
    };

    if ft.is_dir() {
      // 目录：读 `<dir>/SKILL.md`
      let skill_md = path.join("SKILL.md");
      if !skill_md.exists() {
        return (
          None,
          Some(SkillDiagnostic {
            location: Some(path),
            code: SkillDiagnosticCode::FileInfoFailed,
            message: "directory missing SKILL.md".to_string(),
          }),
        );
      }
      self.load_skill_file(&skill_md, names, origin)
    } else if ft.is_file() {
      // 文件：非 SKILL.md 的 *.md 直接作为技能
      if name_str == "SKILL.md" {
        return (None, None); // SKILL.md 只允许在目录内，跳过独立文件
      }
      if path.extension() != Some(OsStr::new("md")) {
        return (None, None);
      }
      self.load_skill_file(&path, names, origin)
    } else {
      (None, None)
    }
  }

  fn load_skill_file(
    &self,
    path: &Path,
    names: &mut std::collections::HashSet<String>,
    origin: &str,
  ) -> (Option<Skill>, Option<SkillDiagnostic>) {
    let content = match std::fs::read_to_string(path) {
      Ok(c) => c,
      Err(err) => {
        return (
          None,
          Some(SkillDiagnostic {
            location: Some(path.to_path_buf()),
            code: SkillDiagnosticCode::ReadFailed,
            message: format!("read failed: {err}"),
          }),
        );
      }
    };

    let parsed = match parse_frontmatter(&content) {
      Ok(p) => p,
      Err(msg) => {
        return (
          None,
          Some(SkillDiagnostic {
            location: Some(path.to_path_buf()),
            code: SkillDiagnosticCode::ParseFailed,
            message: msg,
          }),
        );
      }
    };

    let file_stem = path
      .file_stem()
      .and_then(|s| s.to_str())
      .unwrap_or("skill")
      .to_string();
    let name = parsed.meta.name.unwrap_or(file_stem);

    let trimmed_name = name.trim();
    if trimmed_name.is_empty()
      || trimmed_name.chars().count() > self.max_name_chars
    {
      return (
        None,
        Some(SkillDiagnostic {
          location: Some(path.to_path_buf()),
          code: SkillDiagnosticCode::InvalidMetadata,
          message: format!(
            "invalid skill name: empty or >{} chars",
            self.max_name_chars
          ),
        }),
      );
    }

    if parsed.description.chars().count() > self.max_description_chars {
      return (
        None,
        Some(SkillDiagnostic {
          location: Some(path.to_path_buf()),
          code: SkillDiagnosticCode::InvalidMetadata,
          message: format!("description >{} chars", self.max_description_chars),
        }),
      );
    }

    let name = trimmed_name.to_string();
    if !names.insert(name.clone()) {
      return (
        None,
        Some(SkillDiagnostic {
          location: Some(path.to_path_buf()),
          code: SkillDiagnosticCode::InvalidMetadata,
          message: format!("duplicate skill name: {name}"),
        }),
      );
    }

    (
      Some(Skill {
        name,
        description: parsed.description,
        content: parsed.body,
        location: path.to_path_buf(),
        origin: origin.to_string(),
      }),
      None,
    )
  }
}

// frontmatter 手写解析

struct ParsedFm {
  meta: Frontmatter,
  description: String,
  body: String,
}

struct Frontmatter {
  name: Option<String>,
}

/// 手写 frontmatter 解析器（不引入 serde_yaml）。
///
/// # Errors
///
/// 返回错误信息字符串，调用方映射到 `ParseFailed` 诊断。
fn parse_frontmatter(raw: &str) -> Result<ParsedFm, String> {
  let trimmed = raw.trim_start_matches('\u{feff}');
  let mut lines = trimmed.lines();

  // 1. 首行必须是 "---"
  let first = lines.next().ok_or_else(|| "empty file".to_string())?;
  if first.trim_end() != "---" {
    return Err("missing frontmatter opening delimiter".to_string());
  }

  // 2. 向下寻找结束分隔符
  let mut fm_lines = Vec::new();
  let mut body_start: Option<usize> = None;
  let mut scan = lines.enumerate();
  for (i, line) in scan.by_ref() {
    if line.trim_end() == "---" {
      // enumerate 索引从 0 开始，但我们已消费 1 行（opening 分隔符），
      // 所以 closing 的绝对索引 = i + 1，body 起点再加 1 = i + 2
      body_start = Some(i + 2);
      break;
    }
    fm_lines.push(line);
  }
  let body_start = body_start
    .ok_or_else(|| "missing frontmatter closing delimiter".to_string())?;

  // 3. 解析键值对
  //    fm_lines 已收集完全部 frontmatter 行（不含 opening / closing 分隔符），
  //    用可消费迭代器遇到 description 块标量时继续往后取接续行。
  let mut meta = Frontmatter { name: None };
  let mut description = String::new();
  let mut fm_iter = fm_lines.iter();

  while let Some(line) = fm_iter.next() {
    if line.trim().is_empty() || line.trim_start().starts_with('#') {
      continue;
    }

    let Some(colon) = line.find(':') else {
      continue;
    };
    let key = line[..colon].trim();
    let value = line[colon + 1..].trim();

    match key.to_ascii_lowercase().as_str() {
      "name" => {
        if meta.name.is_none() {
          meta.name = Some(strip_quotes(value).to_string());
        }
      }
      "description" => {
        if value == "|" || value == "|-" {
          // 块标量：从 fm_iter 继续取接续行直到缩进不足
          let mut indent_min: Option<usize> = None;
          description.clear();
          for cont in fm_iter.by_ref() {
            let c = cont.trim();
            if c.is_empty() {
              if !description.is_empty() {
                description.push('\n');
              }
              continue;
            }
            let indent = cont.len() - cont.trim_start().len();
            match indent_min {
              None => indent_min = Some(indent),
              Some(min) if indent < min => break,
              _ => {}
            }
            if !description.is_empty() {
              description.push('\n');
            }
            description.push_str(c);
          }
          break; // description 块消费完，frontmatter 其余忽略
        } else {
          description = strip_quotes(value).to_string();
        }
      }
      _ => { /* 未知 key，忽略 */ }
    }
  }

  // 4. 正文
  let body_lines: Vec<_> = trimmed.lines().skip(body_start).collect();
  let body_text = body_lines.join("\n").trim().to_string();
  if body_text.is_empty() {
    return Err("empty skill body".to_string());
  }

  Ok(ParsedFm {
    meta,
    description: description.trim().to_string(),
    body: body_text,
  })
}

/// 去除成对包裹的引号。
fn strip_quotes(s: &str) -> &str {
  let trimmed = s.trim();
  if (trimmed.starts_with('"') && trimmed.ends_with('"'))
    || (trimmed.starts_with('\'') && trimmed.ends_with('\''))
  {
    &trimmed[1..trimmed.len() - 1]
  } else {
    trimmed
  }
}

// 测试

#[cfg(test)]
mod tests {
  use std::path::PathBuf;

  use super::*;

  fn td(name: &str) -> PathBuf {
    crate::test_util::test_dir(name)
  }

  /// dir/SKILL.md + flat.md 两种布局都能加载，name 升序，location 正确。
  #[test]
  fn load_scans_dir_and_file_layouts() {
    let root = td("load_scans_dir_and_file_layouts");

    // 目录型技能
    let d = root.join("coding-skill");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
      d.join("SKILL.md"),
      "---\nname: coding\ndescription: Write clean code\n---\nSkill body here",
    )
    .unwrap();

    // 扁平型技能
    std::fs::write(
      root.join("flat.md"),
      "---\nname: flat\ndescription: Flat file skill\n---\nFlat body",
    )
    .unwrap();

    let loader = SkillsLoader::new(vec![root.clone()]);
    let (skills, diagnostics) = loader.load();

    assert!(
      diagnostics.is_empty(),
      "no diagnostics expected: {diagnostics:?}"
    );
    assert_eq!(skills.len(), 2);

    // name 升序
    assert_eq!(skills[0].name, "coding");
    assert_eq!(skills[1].name, "flat");

    // location
    assert!(skills[0].location.ends_with("SKILL.md"));
    assert!(skills[1].location.ends_with("flat.md"));

    // body
    assert_eq!(skills[0].content, "Skill body here");
    assert_eq!(skills[1].content, "Flat body");
  }

  /// md 无 frontmatter → 该技能被跳过，ParseFailed 诊断，其余继续加载。
  #[test]
  fn missing_frontmatter_yields_diagnostic() {
    let root = td("missing_frontmatter_yields_diagnostic");

    std::fs::write(root.join("a.md"), "no frontmatter here\njust body\n")
      .unwrap();
    std::fs::write(
      root.join("b.md"),
      "---\nname: b\ndescription: valid\n---\nValid body",
    )
    .unwrap();

    let loader = SkillsLoader::new(vec![root]);
    let (skills, diagnostics) = loader.load();

    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "b");
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].code, SkillDiagnosticCode::ParseFailed);
    assert!(diagnostics[0].message.contains("missing frontmatter"));
  }

  /// description 使用块标量语法（`|`）+ 无 name 回退文件主干。
  #[test]
  fn multiline_description_and_name_fallback() {
    let root = td("multiline_description_and_name_fallback");

    std::fs::write(
      root.join("auto-named.md"),
      "---\ndescription: |\n  Line one\n  Line two\n---\nBody",
    )
    .unwrap();

    let loader = SkillsLoader::new(vec![root]);
    let (skills, diagnostics) = loader.load();

    assert!(diagnostics.is_empty());
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "auto-named"); // 回退文件名主干
    assert_eq!(skills[0].description, "Line one\nLine two");
    assert_eq!(skills[0].content, "Body");
  }

  /// 补充：扫描根不存在 → ListFailed 诊断；其余技能仍加载（多个根）。
  #[test]
  fn missing_root_yields_list_failed_and_continues() {
    let good_root = td("missing_root_yields_list_failed");
    let bad_root = good_root.join("does-not-exist");
    std::fs::write(good_root.join("ok.md"), "---\nname: ok\n---\nBody")
      .unwrap();

    let loader = SkillsLoader::new(vec![bad_root, good_root]);
    let (skills, diagnostics) = loader.load();

    assert_eq!(skills.len(), 1);
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].code, SkillDiagnosticCode::ListFailed);
  }

  /// 多域来源标签 + 同名优先级（首个 roots 胜出）。
  ///
  /// 场景：global / project 两域都有同名技能 `dup`，按 project 优先顺序
  /// 排列；预期加载 project 版本，origin 标记正确。
  #[test]
  fn multi_domain_origin_and_priority() {
    let g = td("multi_domain_origin-global");
    let p = td("multi_domain_origin-project");
    std::fs::write(g.join("dup.md"), "---\nname: dup\n---\nGlobal body")
      .unwrap();
    std::fs::write(p.join("dup.md"), "---\nname: dup\n---\nProject body")
      .unwrap();
    std::fs::write(p.join("only.md"), "---\nname: only\n---\nOnly body")
      .unwrap();

    // project 先 → 优先胜出
    let loader = SkillsLoader::with_origins(vec![
      SkillRoot::new(p.clone(), "project"),
      SkillRoot::new(g.clone(), "global"),
    ]);
    let (skills, _) = loader.load();
    assert_eq!(skills.len(), 2);
    let dup = skills.iter().find(|s| s.name == "dup").expect("dup skill");
    assert_eq!(dup.content, "Project body");
    assert_eq!(dup.origin, "project");
    let only = skills
      .iter()
      .find(|s| s.name == "only")
      .expect("only skill");
    assert_eq!(only.origin, "project");
  }

  /// 旧 new() 构造器的 origin 恒为 project（零配置兼容）。
  #[test]
  fn legacy_new_marks_origin_as_project() {
    let root = td("legacy_new_marks_origin_as_project");
    std::fs::write(root.join("a.md"), "---\nname: a\n---\nBody").unwrap();
    let (skills, _) = SkillsLoader::new(vec![root]).load();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].origin, "project");
  }

  /// 补充：同名技能（两个根目录） → 首个胜出，后来者 InvalidMetadata。
  #[test]
  fn duplicate_name_is_diagnosed() {
    let r1 = td("duplicate_name_is_diagnosed-r1");
    let r2 = td("duplicate_name_is_diagnosed-r2");
    std::fs::write(r1.join("first.md"), "---\nname: dup\n---\nBody first")
      .unwrap();
    std::fs::write(r2.join("second.md"), "---\nname: dup\n---\nBody second")
      .unwrap();

    let loader = SkillsLoader::new(vec![r1, r2]);
    let (skills, diagnostics) = loader.load();

    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].content, "Body first");
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].code, SkillDiagnosticCode::InvalidMetadata);
    assert!(diagnostics[0].message.contains("duplicate"));
  }
}
