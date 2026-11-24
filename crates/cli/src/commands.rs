//! 行命令解析：`Command` 枚举与纯函数解析。
//!
//! 解析不持有 `Harness`——命令执行的控制流经 [`Effect`] 返回主循环，
//! 保持纯函数可完整单测。

/// 行命令（命令表；非 `/` 开头的整行为用户消息）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
  /// 用户消息（含空行——调用方丢弃空串）。
  Chat(String),
  /// `/help`：命令清单。
  Help,
  /// `/stop`：取消在途回合。
  Stop,
  /// `/cancel`：`/stop` 同义（兼容命令集）。
  Cancel,
  /// `/skills`：列出已有技能与诊断。
  Skills,
  /// `/sessions [name]`：无参列出会话键；带参切换到指定会话
  ///（不存在则新建空白会话）。
  Sessions(Option<String>),
  /// `/status`：状态行摘要（token 累计 / 在途 / 上下文占比）。
  Status,
  /// `/history [n]`：最近 n 条历史记录（默认 10）。
  History(usize),
  /// `/exit` / `/quit`：退出。
  Exit,
}

/// 命令适用端：TUI 与 CLI 命令面不完全一致——CLI 终端滚动缓冲不可控，
/// 依赖 `/history` 回看历史记录、`/status` 看状态；TUI 对话区即完整历史、
/// 侧栏常驻状态，二者冗余。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
  /// 双端可用。
  Both,
  /// 仅 CLI（TUI 的 `/help` 浮层与 Tab 补全不展示）。
  CliOnly,
}

/// 命令元数据表条目：CLI `/help` 展示与 TUI Tab 补全的同源候选。
pub struct CommandMeta {
  /// 主名（含 `/` 前缀）。
  pub name: &'static str,
  /// 别名（含 `/` 前缀；无别名为空切片）。
  pub aliases: &'static [&'static str],
  /// 参数占位文本（无参数为空串）。
  pub args: &'static str,
  /// 一句话说明。
  pub description: &'static str,
  /// 适用端（TUI 侧按此过滤浮层与补全）。
  pub target: Target,
}

/// 全部命令的元数据表（与 `parse_line` 支持集一一对应；新增命令须同步）。
pub const COMMANDS: &[CommandMeta] = &[
  CommandMeta {
    name: "/help",
    aliases: &["/?"],
    args: "",
    description: "显示本清单",
    target: Target::Both,
  },
  CommandMeta {
    name: "/stop",
    aliases: &["/cancel"],
    args: "",
    description: "取消在途回合",
    target: Target::Both,
  },
  CommandMeta {
    name: "/skills",
    aliases: &[],
    args: "",
    description: "列出已有技能与诊断",
    target: Target::Both,
  },
  CommandMeta {
    name: "/sessions",
    aliases: &[],
    args: "[name]",
    description: "列出会话；带参切换或新建会话",
    target: Target::Both,
  },
  CommandMeta {
    name: "/status",
    aliases: &[],
    args: "",
    description: "状态摘要（会话/模式/在途）",
    target: Target::CliOnly,
  },
  CommandMeta {
    name: "/history",
    aliases: &[],
    args: "[n]",
    description: "最近 n 条历史记录（默认 10）",
    target: Target::CliOnly,
  },
  CommandMeta {
    name: "/exit",
    aliases: &["/quit"],
    args: "",
    description: "退出",
    target: Target::Both,
  },
];

/// 解析失败（未知命令 / 非法参数）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
  /// 未识别的命令名。
  UnknownCommand(String),
  /// 命令参数非法。
  BadArgs(String),
}

impl std::fmt::Display for ParseError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::UnknownCommand(name) => {
        write!(f, "未知命令 {name}（/help 查看可用命令）")
      }
      Self::BadArgs(msg) => write!(f, "参数错误：{msg}"),
    }
  }
}

/// 行首 `/` 为命令，其余整行为用户消息；未知命令报错不进入回合。
pub fn parse_line(line: &str) -> Result<Command, ParseError> {
  let trimmed = line.trim();
  if !trimmed.starts_with('/') {
    return Ok(Command::Chat(trimmed.to_string()));
  }
  let (head, rest) = trimmed.split_once(' ').unwrap_or((trimmed, ""));
  let rest = rest.trim();
  match head {
    // `/?` 为 COMMANDS 中声明的 `/help` 别名
    "/help" | "/?" => Ok(Command::Help),
    "/stop" => Ok(Command::Stop),
    "/cancel" => Ok(Command::Cancel),
    "/skills" => Ok(Command::Skills),
    "/sessions" => Ok(Command::Sessions(
      (!rest.is_empty()).then(|| rest.to_string()),
    )),
    "/status" => Ok(Command::Status),
    "/history" => match rest.parse::<usize>() {
      Ok(n) => Ok(Command::History(n)),
      Err(_) if rest.is_empty() => Ok(Command::History(10)),
      Err(_) => Err(ParseError::BadArgs(format!("`{rest}` 不是非负整数"))),
    },
    "/exit" | "/quit" => Ok(Command::Exit),
    other => Err(ParseError::UnknownCommand(other.to_string())),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// 场景：全部命令解析（CMD-01）。
  ///
  /// 预期：9 命令含参数形态、非命令行直通 Chat、未知命令报错、
  /// /history 缺省与非法参数分支。
  #[test]
  fn parse_line_parses_all_commands() {
    // 非命令行 → Chat（trim）
    assert_eq!(
      parse_line("hello world"),
      Ok(Command::Chat("hello world".into()))
    );
    assert_eq!(parse_line("  "), Ok(Command::Chat(String::new())));
    // 无参命令
    assert_eq!(parse_line("/help"), Ok(Command::Help));
    assert_eq!(parse_line("/stop"), Ok(Command::Stop));
    assert_eq!(parse_line("/cancel"), Ok(Command::Cancel));
    assert_eq!(parse_line("/skills"), Ok(Command::Skills));
    assert_eq!(parse_line("/sessions"), Ok(Command::Sessions(None)));
    assert_eq!(parse_line("/status"), Ok(Command::Status));
    assert_eq!(parse_line("/quit"), Ok(Command::Exit));
    assert_eq!(parse_line("/exit"), Ok(Command::Exit));
    // /sessions：带参切换/新建（合并原 /new）
    assert_eq!(
      parse_line("/sessions work"),
      Ok(Command::Sessions(Some("work".into())))
    );
    // /new 已移除：报未知命令
    assert_eq!(
      parse_line("/new work"),
      Err(ParseError::UnknownCommand("/new".into()))
    );
    // /history：缺省 10、显式数字、非法参数
    assert_eq!(parse_line("/history"), Ok(Command::History(10)));
    assert_eq!(parse_line("/history 3"), Ok(Command::History(3)));
    assert!(matches!(
      parse_line("/history abc"),
      Err(ParseError::BadArgs(_))
    ));
    // 未知命令不进入回合
    assert_eq!(
      parse_line("/nope"),
      Err(ParseError::UnknownCommand("/nope".into()))
    );
  }

  /// 场景：元数据表声明的命令名与别名均可解析（CMD-02，防漂移）。
  ///
  /// 预期：遍历 `COMMANDS` 的 `name` 与 `aliases`，`parse_line` 均不返回
  /// `UnknownCommand`；`/help` 的 `/?` 别名明确映射到 `Help`。
  #[test]
  fn all_declared_commands_and_aliases_are_parsable() {
    for meta in COMMANDS {
      assert!(
        !matches!(parse_line(meta.name), Err(ParseError::UnknownCommand(_))),
        "主名 {} 未在 parse_line 中实现",
        meta.name
      );
      for alias in meta.aliases {
        assert!(
          !matches!(parse_line(alias), Err(ParseError::UnknownCommand(_))),
          "别名 {alias} 未在 parse_line 中实现"
        );
      }
    }
    assert_eq!(parse_line("/?"), Ok(Command::Help));
  }
}
