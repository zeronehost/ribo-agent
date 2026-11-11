//! 行命令解析：`Command` 枚举与纯函数解析（14 详细设计 §7）。
//!
//! 解析不持有 `Harness`——命令执行的控制流经 [`Effect`] 返回主循环，
//! 保持纯函数可完整单测。

/// 行命令（概设 §4.2 命令表；非 `/` 开头的整行为用户消息）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
  /// 用户消息（含空行——调用方丢弃空串）。
  Chat(String),
  /// `/help`：命令清单。
  Help,
  /// `/new [name]`：轮换会话键（name 缺省自动轮换）。
  New(Option<String>),
  /// `/stop`：取消在途回合。
  Stop,
  /// `/cancel`：`/stop` 同义（TODO_02 命令集）。
  Cancel,
  /// `/skills`：列出技能与诊断。
  Skills,
  /// `/sessions`：列出会话键。
  Sessions,
  /// `/history [n]`：最近 n 条转录（默认 10）。
  History(usize),
  /// `/exit` / `/quit`：退出。
  Exit,
}

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
    "/help" => Ok(Command::Help),
    "/new" => Ok(Command::New((!rest.is_empty()).then(|| rest.to_string()))),
    "/stop" => Ok(Command::Stop),
    "/cancel" => Ok(Command::Cancel),
    "/skills" => Ok(Command::Skills),
    "/sessions" => Ok(Command::Sessions),
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
  /// 预期：8 命令含参数形态、非命令行直通 Chat、未知命令报错、
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
    assert_eq!(parse_line("/sessions"), Ok(Command::Sessions));
    assert_eq!(parse_line("/quit"), Ok(Command::Exit));
    assert_eq!(parse_line("/exit"), Ok(Command::Exit));
    // /new：缺省轮换 vs 自定义键名
    assert_eq!(parse_line("/new"), Ok(Command::New(None)));
    assert_eq!(
      parse_line("/new work"),
      Ok(Command::New(Some("work".into())))
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
}
