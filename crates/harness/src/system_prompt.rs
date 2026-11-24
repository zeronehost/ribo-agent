//! 系统提示组装器（§9）。
//!
//! 两条格式化路径必须分离（pi-harness invariant）：
//! - 技能**清单**插入系统提示，属性值与文本一律 XML 转义；
//! - 技能**显式调用**（`format_skill_invocation`）逐字保留正文，不转义。

#[cfg(test)]
use std::path::Path;

use ra_core::message::{Content, TextContent};

use crate::instructions::Instructions;
use crate::skills::Skill;

/// 默认系统提示模板（英文：与模型指令语言一致，避免中英混排）。
const DEFAULT_TEMPLATE: &str = "\
You are a capable AI agent operating in a terminal workspace. \
Use the available tools to inspect files and run commands; \
prefer minimal, verifiable changes and report what you did.";

/// 系统提示组装器。
///
/// 段落顺序：模板 → 环境 → 技能清单 → 自定义段 → 会话摘要。
pub struct SystemPromptBuilder {
  template: String,
  include_env: bool,
  custom: Vec<String>,
  /// 域内 AGENTS.md 拼接内容（19_ Step 5）；空集不产段。
  instructions: Option<Instructions>,
}

impl Default for SystemPromptBuilder {
  fn default() -> Self {
    Self::new()
  }
}

impl SystemPromptBuilder {
  /// 创建默认组装器（默认模板 + 环境段开 + 无自定义段）。
  pub fn new() -> Self {
    Self {
      template: DEFAULT_TEMPLATE.to_string(),
      include_env: true,
      custom: Vec::new(),
      instructions: None,
    }
  }

  /// 覆盖模板。
  pub fn with_template(mut self, template: impl Into<String>) -> Self {
    self.template = template.into();
    self
  }

  /// 开关环境段。
  pub fn with_environment(mut self, include: bool) -> Self {
    self.include_env = include;
    self
  }

  /// 追加自定义段（按添加顺序拼接）。
  pub fn with_custom_section(mut self, section: impl Into<String>) -> Self {
    self.custom.push(section.into());
    self
  }

  /// 注入域内 `AGENTS.md` 拼接内容（19_ Step 5）。
  /// 空集时 build 不产 `<agents-instructions>` 段。
  pub fn with_instructions(mut self, instructions: Instructions) -> Self {
    self.instructions = Some(instructions);
    self
  }

  /// 组装系统提示（返回单段 `Content::Text`）。
  ///
  /// 段落顺序：模板 → 环境 → 技能清单 → 自定义段 → 会话摘要。
  /// 不追加压缩提示段（兼容旧调用方）；需要提示时用 [`Self::build_compaction`]。
  pub fn build(&self, skills: &[Skill], summary: Option<&str>) -> Vec<Content> {
    self.build_compaction(skills, summary, false, None)
  }

  /// 组装系统提示并追加压缩提示段（D6）。
  ///
  /// 段落顺序：模板 → 环境 → 技能清单 → 自定义段 → 会话摘要 →
  /// 过期提示 / 回退提示。
  ///
  /// - `fallback_hint == true` 且无摘要时追加 `<compaction-fallback>`；
  /// - `outdated_hint == Some(N)` 时追加 `<conversation-summary-outdated>`。
  ///
  /// 两个提示段只表达"前文缺失/摘要陈旧"，不暴露摘要文本或截断条数，
  /// 目的是避免模型"假装记得"导致幻觉。
  pub fn build_compaction(
    &self,
    skills: &[Skill],
    summary: Option<&str>,
    fallback_hint: bool,
    outdated_hint: Option<u32>,
  ) -> Vec<Content> {
    let mut sections: Vec<String> = Vec::new();

    // 1. 模板
    let t = self.template.trim();
    if !t.is_empty() {
      sections.push(t.to_string());
    }

    // 2. 环境段
    if self.include_env {
      let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "<unknown>".to_string());
      sections.push(format!(
        "<environment>\n<cwd>{cwd}</cwd>\n<platform>{}</platform>\n<arch>{}</arch>\n</environment>",
        escape_xml(std::env::consts::OS),
        escape_xml(std::env::consts::ARCH),
      ));
    }

    // 3. 技能清单
    if !skills.is_empty() {
      let mut list = "<available-skills>\n".to_string();
      for skill in skills {
        list.push_str(&format!(
          "  <skill name=\"{}\" description=\"{}\" origin=\"{}\"/>\n",
          escape_xml(&skill.name),
          escape_xml(&skill.description),
          escape_xml(&skill.origin),
        ));
      }
      list.push_str("</available-skills>");
      sections.push(list);
    }

    // 3.5 域内 AGENTS.md 指令段（文档级资源，默认加载）
    if let Some(instructions) = &self.instructions
      && !instructions.is_empty()
    {
      sections.push(format!(
        "<agents-instructions>\n{}\n</agents-instructions>",
        instructions.content.trim()
      ));
    }

    // 4. 自定义段
    for c in &self.custom {
      let c = c.trim();
      if !c.is_empty() {
        sections.push(c.to_string());
      }
    }

    // 5. 摘要（模型生成文本，不转义）
    if let Some(s) = summary {
      let s = s.trim();
      if !s.is_empty() {
        sections.push(format!(
          "<conversation-summary>\n{s}\n</conversation-summary>"
        ));
      }
    }

    // 6. 摘要过期提示（D6）：冷却期复用旧摘要且已存在多轮
    if let Some(age) = outdated_hint {
      sections.push(format!(
        "<conversation-summary-outdated>\nThis summary is from {age} turns ago; some recent context may be missing.\n</conversation-summary-outdated>"
      ));
    }

    // 7. 回退提示（D6）：无摘要且发生截断——告知模型前文缺失
    if fallback_hint && summary.is_none() {
      sections.push(
        "<compaction-fallback>\nEarlier conversation turns were omitted; no summary is available for this request.\n</compaction-fallback>"
          .to_string(),
      );
    }

    if sections.is_empty() {
      return Vec::new();
    }

    let content = sections.join("\n\n");
    vec![Content::Text(TextContent {
      content,
      signature: None,
    })]
  }
}

/// 技能显式调用格式：正文不转义，属性值转义。
pub fn format_skill_invocation(skill: &Skill, extra: Option<&str>) -> String {
  let mut out = format!(
    "<skill name=\"{}\" location=\"{}\">\n{}",
    escape_xml(&skill.name),
    escape_xml(skill.location.to_string_lossy().as_ref()),
    skill.content,
  );
  if let Some(extra) = extra {
    out.push('\n');
    out.push_str(extra);
  }
  out.push_str("\n</skill>");
  out
}

/// XML 文本 / 属性转义：`&` `<` `>` `"` `'` 分别替换为实体。
pub fn escape_xml(raw: &str) -> String {
  raw
    .replace('&', "&amp;")
    .replace('<', "&lt;")
    .replace('>', "&gt;")
    .replace('"', "&quot;")
    .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::skills::Skill;

  fn skill(name: &str, desc: &str, content: &str) -> Skill {
    Skill {
      name: name.to_string(),
      description: desc.to_string(),
      content: content.to_string(),
      location: Path::new("/skills").join(format!("{name}.md")),
      origin: "project".to_string(),
    }
  }

  /// SY-01：build 含环境段 + 2 技能 → 输出含 `<environment>` 与两条 `<skill .../>`。
  #[test]
  fn build_includes_env_and_skills() {
    let skills = vec![
      skill("bash", "Run shell", "# bash\nrun things"),
      skill("write-file", "Write file", "# write\nwrite things"),
    ];

    let prompt = SystemPromptBuilder::new()
      .with_template("TPL")
      .with_environment(true)
      .build(&skills, None);

    assert_eq!(prompt.len(), 1);
    let text = match &prompt[0] {
      Content::Text(t) => &t.content,
      _ => panic!("expected Text"),
    };

    assert!(text.contains("<environment>"));
    assert!(text.contains("<platform>"));
    assert!(text.contains("<arch>"));
    assert!(text.contains("<available-skills>"));
    assert!(text.contains("name=\"bash\""));
    assert!(text.contains("name=\"write-file\""));
  }

  /// SY-02：技能正文含 `<xml>`；清单条目转义为 `&lt;`；
  /// format_skill_invocation 正文保持 `<xml>` 原文。
  #[test]
  fn invocation_keeps_body_and_escapes_listing() {
    let s = skill("xml-skill", "Desc with <xml>", "Body with <xml> & &amp;");

    let listing = SystemPromptBuilder::new()
      .with_environment(false)
      .build(std::slice::from_ref(&s), None);
    let listing_text = match &listing[0] {
      Content::Text(t) => &t.content,
      _ => panic!("expected Text"),
    };

    // 清单中属性值被转义（name 不含特殊字符所以原样；description 被转义）
    assert!(listing_text.contains("&lt;xml&gt;"), "description 应转义");
    // 清单不应含原始 '<' 作为技能属性值（正文根本不会出现在清单里）
    assert!(!listing_text.contains("description=\"Desc with <xml>\""));

    // format_skill_invocation：正文保留原文；属性转义
    let invocation = format_skill_invocation(&s, None);
    assert!(
      invocation.contains("Body with <xml> & &amp;"),
      "正文应保留原文"
    );
    assert!(invocation.contains("name=\"xml-skill\""), "属性存在");
    assert!(invocation.contains("<skill name="), "标签存在");
  }

  /// 补充：无 template 无 skills 无 env → 返回空 Vec。
  #[test]
  fn empty_build_returns_empty_vec() {
    let prompt = SystemPromptBuilder::new()
      .with_template("")
      .with_environment(false)
      .build(&[], None);
    assert!(prompt.is_empty());
  }

  /// 补充：summary 非空 → 追加 `<conversation-summary>` 段。
  #[test]
  fn summary_append_as_xml_block() {
    let prompt = SystemPromptBuilder::new()
      .with_template("TPL")
      .with_environment(false)
      .build(&[], Some("This is summary"));
    let text = match &prompt[0] {
      Content::Text(t) => &t.content,
      _ => panic!("expected Text"),
    };
    assert!(text.contains("<conversation-summary>"));
    assert!(text.contains("This is summary"));
  }

  /// D6：无摘要 + fallback_hint → 追加 `<compaction-fallback>` 段。
  #[test]
  fn fallback_hint_appends_compaction_fallback() {
    let prompt = SystemPromptBuilder::new()
      .with_template("TPL")
      .with_environment(false)
      .build_compaction(&[], None, true, None);
    let text = match &prompt[0] {
      Content::Text(t) => &t.content,
      _ => panic!("expected Text"),
    };
    assert!(text.contains("<compaction-fallback>"));
    assert!(text.contains("no summary is available"));
  }

  /// D6：有摘要时即使 fallback_hint=true 也不追加回退段。
  #[test]
  fn fallback_hint_ignored_when_summary_present() {
    let prompt = SystemPromptBuilder::new()
      .with_template("TPL")
      .with_environment(false)
      .build_compaction(&[], Some("kept"), true, None);
    let text = match &prompt[0] {
      Content::Text(t) => &t.content,
      _ => panic!("expected Text"),
    };
    assert!(!text.contains("<compaction-fallback>"));
  }

  /// D6：outdated_hint=Some(N) → 追加过期提示段且含 N。
  #[test]
  fn outdated_hint_appends_outdated_section_with_age() {
    let prompt = SystemPromptBuilder::new()
      .with_template("TPL")
      .with_environment(false)
      .build_compaction(&[], Some("kept"), false, Some(5));
    let text = match &prompt[0] {
      Content::Text(t) => &t.content,
      _ => panic!("expected Text"),
    };
    assert!(text.contains("<conversation-summary-outdated>"));
    assert!(text.contains("from 5 turns ago"));
  }

  /// Step 5-C：技能清单带 origin 属性（来源域标签）。
  #[test]
  fn skill_list_includes_origin_attribute() {
    let mut s = skill("review", "desc", "body");
    s.origin = "plugin:company".to_string();
    let prompt = SystemPromptBuilder::new()
      .with_template("TPL")
      .with_environment(false)
      .build(&[s], None);
    let text = match &prompt[0] {
      Content::Text(t) => &t.content,
      _ => panic!("expected Text"),
    };
    assert!(text.contains("origin=\"plugin:company\""));
  }

  /// Step 5-C：Instructions 非空 → 产 <agents-instructions> 段；空 → 不产。
  #[test]
  fn agents_instructions_section_conditional() {
    let instr = Instructions {
      content: "<!-- source: project (x) -->\nBe concise".to_string(),
      sources: vec![crate::instructions::InstructionSource {
        origin: "project".to_string(),
        path: std::path::PathBuf::from("x/AGENTS.md"),
        truncated: false,
      }],
    };
    let with = SystemPromptBuilder::new()
      .with_template("TPL")
      .with_environment(false)
      .with_instructions(instr)
      .build(&[], None);
    let text = match &with[0] {
      Content::Text(t) => &t.content,
      _ => panic!("expected Text"),
    };
    assert!(text.contains("<agents-instructions>"));
    assert!(text.contains("Be concise"));

    let without = SystemPromptBuilder::new()
      .with_template("TPL")
      .with_environment(false)
      .build(&[], None);
    let text2 = match &without[0] {
      Content::Text(t) => &t.content,
      _ => panic!("expected Text"),
    };
    assert!(!text2.contains("<agents-instructions>"));
  }

  /// 补充：custom section 非空 → 追加。
  #[test]
  fn custom_section_appended_in_order() {
    let prompt = SystemPromptBuilder::new()
      .with_template("TPL")
      .with_environment(false)
      .with_custom_section("C1")
      .with_custom_section("C2")
      .build(&[], None);
    let text = match &prompt[0] {
      Content::Text(t) => &t.content,
      _ => panic!("expected Text"),
    };
    assert!(text.contains("TPL\n\nC1\n\nC2"));
  }
}
