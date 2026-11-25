//! JSONL 会话存储：追加写、回放、键编码与逐键写锁。

use std::{
  collections::hash_map::DefaultHasher,
  hash::{Hash, Hasher},
  path::PathBuf,
  sync::Arc,
};

use dashmap::DashMap;
use ra_agent::error::{context_build_error, session_create_failed};
use ra_core::{error::RiboResult, message::Message, utils::time::now_ms};
use serde::{Deserialize, Serialize};

use crate::compaction_state::CompactionCache;
use tokio::{
  fs::{self, OpenOptions},
  io::AsyncWriteExt,
  sync::Mutex,
};

/// JSONL 记录版本号。
const RECORD_VERSION: u32 = 1;

/// 编码后文件名的最大字符数（超过则截断并追加摘要后缀）。
const MAX_ENCODED_LEN: usize = 120;

/// JSONL 会话存储。
pub struct JsonlSessionStore {
  /// 会话文件目录（`data_dir/sessions`）。
  root: PathBuf,
  /// 逐键写锁：同键 append / delete 串行化。
  locks: DashMap<String, Arc<Mutex<()>>>,
}

/// 一次回放的结果。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionReplay {
  /// 按时间顺序排列的消息。
  pub messages: Vec<Message>,
  /// 破损行 / 未知版本 / 未知记录类型的诊断。
  pub diagnostics: Vec<ReplayDiagnostic>,
  /// 最后一条有效的摘要旁路记录：不进 `messages`，不决定读取起点。
  pub compaction: Option<CompactionCache>,
}

/// 单条回放诊断。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayDiagnostic {
  /// 从 1 起始的行号。
  pub line: usize,
  /// 诊断码。
  pub code: ReplayDiagnosticCode,
}

/// 回放诊断码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayDiagnosticCode {
  /// 文件末尾的半行（进程被杀 / 断电导致）。
  TornTail,
  /// 非末行的解析失败（手工编辑损坏，或与 append 并发读取时的跨批次半行）。
  CorruptLine,
  /// `type` 字段不在 `meta` / `message` / `compaction` 之中。
  UnknownRecordType,
  /// 版本号高于当前支持版本，按 v1 尽力回放。
  UnsupportedVersion,
  /// `compaction` 摘要记录损坏 / 字段缺失（忽略记录，不中断回放）。
  StaleCompaction,
}

impl JsonlSessionStore {
  /// 以会话文件目录创建存储（目录在首次写入时创建）。
  pub fn new(root: impl Into<PathBuf>) -> Self {
    Self {
      root: root.into(),
      locks: DashMap::new(),
    }
  }

  /// 追加一批消息（回合级原子写）。
  ///
  /// # Errors
  ///
  /// 目录创建 / 序列化 / 写入失败 → `agent.session_create_failed`。
  pub async fn append(
    &self,
    key: &str,
    messages: &[Message],
  ) -> RiboResult<()> {
    // 空批次不产生 IO，也不创建文件
    if messages.is_empty() {
      return Ok(());
    }

    // 键编码与路径解析
    let encoded = SessionKeyCodec::encode(key)?;
    let path = self.root.join(format!("{encoded}.jsonl"));

    // 逐键写锁（同键写入绝不并发）
    let lock = self.write_lock(&encoded);
    let _guard = lock.lock().await;

    // 一次组装、一次追加
    fs::create_dir_all(&self.root).await.map_err(|err| {
      session_create_failed(key, format!("create sessions dir: {err}"))
    })?;

    // 先打开再判断是否需要 meta 首行：以「文件当前长度」为准，
    // 不用 `try_exists` 预判（预判与写入之间存在竞态窗口）。
    let mut file = OpenOptions::new()
      .create(true)
      .append(true)
      .open(&path)
      .await
      .map_err(|err| {
        session_create_failed(key, format!("open session file: {err}"))
      })?;
    let created = file
      .metadata()
      .await
      .map(|meta| meta.len() == 0)
      .unwrap_or(false);

    // 单次 write_all 尽量压低「写入一半」的窗口，但不承诺单系统调用
    // （write_all 内部可能多次 write）；并发 load 读到跨批次半行时由
    // load 的 CorruptLine 诊断兜底，重新 load 可自愈。
    let mut buffer = String::new();
    if created {
      buffer.push_str(&self.render_meta(key)?);
    }
    let ts = now_ms();
    for message in messages {
      let record = WriteRecord {
        v: RECORD_VERSION,
        body: WriteBody::Message { ts, message },
      };
      let line = serde_json::to_string(&record).map_err(|err| {
        session_create_failed(key, format!("serialize message record: {err}"))
      })?;
      buffer.push_str(&line);
      buffer.push('\n');
    }

    file.write_all(buffer.as_bytes()).await.map_err(|err| {
      session_create_failed(key, format!("append session file: {err}"))
    })?;

    tracing::info!(
      session_key = %key,
      messages = messages.len(),
      created,
      "session transcript appended"
    );
    Ok(())
  }

  /// 追加一条摘要旁路记录。
  ///
  /// 与 `append` 共用逐键写锁；首轮（文件不存在）时先写 meta 首行。
  /// 摘要记录**不写回转录**：不截断、不删除任何 `message`。
  ///
  /// # Errors
  ///
  /// 键编码 / 目录创建 / 序列化 / 写入失败 → `agent.session_create_failed`。
  pub async fn append_compaction(
    &self,
    key: &str,
    cache: &CompactionCache,
  ) -> RiboResult<()> {
    let encoded = SessionKeyCodec::encode(key)?;
    let path = self.root.join(format!("{encoded}.jsonl"));
    let lock = self.write_lock(&encoded);
    let _guard = lock.lock().await;

    fs::create_dir_all(&self.root).await.map_err(|err| {
      session_create_failed(key, format!("create sessions dir: {err}"))
    })?;

    let record = WriteRecord {
      v: RECORD_VERSION,
      body: WriteBody::Compaction {
        summary: &cache.last_summary,
        checkpoint: cache.checkpoint_fingerprint.to_string(),
        model: &cache.summarizer_model,
        ts: cache.generated_at,
      },
    };
    let line = serde_json::to_string(&record).map_err(|err| {
      session_create_failed(key, format!("serialize compaction record: {err}"))
    })?;

    // 先打开再判断是否需要 meta 首行：以「文件当前长度」为准，
    // 不用 `try_exists` 预判（预判与写入之间存在竞态，且新建空文件
    // 与已存在文件都可能误判）。
    let mut file = OpenOptions::new()
      .create(true)
      .append(true)
      .open(&path)
      .await
      .map_err(|err| {
        session_create_failed(key, format!("open session file: {err}"))
      })?;
    let needs_meta = file
      .metadata()
      .await
      .map(|meta| meta.len() == 0)
      .unwrap_or(true);

    let mut buffer = String::new();
    if needs_meta {
      buffer.push_str(&self.render_meta(key)?);
    }
    buffer.push_str(&line);
    buffer.push('\n');

    file.write_all(buffer.as_bytes()).await.map_err(|err| {
      session_create_failed(key, format!("append compaction record: {err}"))
    })?;
    // 显式同步：确保随后的读（回放 / 冷启动统计）立即看到完整内容
    file.sync_all().await.map_err(|err| {
      session_create_failed(key, format!("sync session file: {err}"))
    })?;

    tracing::info!(
      session_key = %key,
      checkpoint = %cache.checkpoint_fingerprint,
      "compaction record appended"
    );
    Ok(())
  }

  /// 读取最后一条有效的摘要旁路记录。
  ///
  /// 损坏 / 字段非法 / 指纹解析失败的记录记 `StaleCompaction` 诊断并跳过，
  /// 返回最后一条有效记录；不存在返回 `Ok(None)`。
  ///
  /// # Errors
  ///
  /// 文件读取失败 → `agent.context_build_error`。
  pub async fn load_compaction(
    &self,
    key: &str,
  ) -> RiboResult<Option<CompactionCache>> {
    let encoded = SessionKeyCodec::encode(key)?;
    let path = self.root.join(format!("{encoded}.jsonl"));
    if !fs::try_exists(&path).await.unwrap_or(false) {
      return Ok(None);
    }
    let content = fs::read_to_string(&path).await.map_err(|err| {
      context_build_error(key, format!("read session file: {err}"))
    })?;

    let mut latest = None;
    for raw in content.lines() {
      let trimmed = raw.trim();
      if trimmed.is_empty() {
        continue;
      }
      let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        tracing::debug!(session_key = %key, "non-JSON line skipped in compaction scan");
        continue;
      };
      if value.get("type").and_then(serde_json::Value::as_str)
        != Some("compaction")
      {
        continue;
      }
      match serde_json::from_value::<CompactionRecord>(value) {
        Ok(record) => match record.checkpoint.parse::<u64>() {
          Ok(checkpoint) => {
            latest = Some(CompactionCache {
              last_summary: record.summary,
              checkpoint_fingerprint: checkpoint,
              summarizer_model: record.model,
              generated_at: record.ts,
            });
          }
          Err(_) => tracing::warn!(
            session_key = %key,
            "compaction record has invalid checkpoint fingerprint"
          ),
        },
        Err(err) => tracing::warn!(
          session_key = %key,
          error = %err,
          "compaction record corrupt; ignored"
        ),
      }
    }
    Ok(latest)
  }

  /// 回放会话。
  ///
  /// # Errors
  ///
  /// 打开 / 读取文件失败 → `agent.context_build_error`
  /// （回放失败即历史上下文不可恢复，属上下文装配路径）。
  pub async fn load(&self, key: &str) -> RiboResult<SessionReplay> {
    let encoded = SessionKeyCodec::encode(key)?;
    let path = self.root.join(format!("{encoded}.jsonl"));

    // 文件不存在 = 空历史（首次会话的正常路径，不报错）
    if !fs::try_exists(&path).await.unwrap_or(false) {
      return Ok(SessionReplay::default());
    }

    let content = fs::read_to_string(&path).await.map_err(|err| {
      context_build_error(key, format!("read session file: {err}"))
    })?;

    let mut replay = SessionReplay::default();
    let lines: Vec<&str> = content.lines().collect();
    let last = lines.len().saturating_sub(1);

    // 逐行解析：任何单行问题都只记诊断，不中断回放
    for (index, raw) in lines.iter().enumerate() {
      let trimmed = raw.trim();
      if trimmed.is_empty() {
        continue;
      }

      let value: serde_json::Value = match serde_json::from_str(trimmed) {
        Ok(value) => value,
        Err(_) => {
          // 末行损坏 = torn write；中间行损坏 = 手工编辑/写坏/并发读半行
          let code = if index == last {
            ReplayDiagnosticCode::TornTail
          } else {
            ReplayDiagnosticCode::CorruptLine
          };
          replay.diagnostics.push(ReplayDiagnostic {
            line: index + 1,
            code,
          });
          continue;
        }
      };

      // 版本：高版本按 v1 尽力回放
      let version = value
        .get("v")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(u64::from(RECORD_VERSION));
      if version != u64::from(RECORD_VERSION) {
        replay.diagnostics.push(ReplayDiagnostic {
          line: index + 1,
          code: ReplayDiagnosticCode::UnsupportedVersion,
        });
      }

      match value.get("type").and_then(serde_json::Value::as_str) {
        Some("meta") => { /* 首行元信息，回放不消费 */ }
        Some("message") => {
          let Some(message) = value.get("message") else {
            continue;
          };
          match serde_json::from_value::<Message>(message.clone()) {
            Ok(message) => replay.messages.push(message),
            Err(_) => replay.diagnostics.push(ReplayDiagnostic {
              line: index + 1,
              code: ReplayDiagnosticCode::CorruptLine,
            }),
          }
        }
        // 摘要旁路记录——不进 messages、不决定读取起点，只存最后一条。
        Some("compaction") => {
          match serde_json::from_value::<CompactionRecord>(value.clone()) {
            Ok(record) => match record.checkpoint.parse::<u64>() {
              Ok(checkpoint) => {
                replay.compaction = Some(CompactionCache {
                  last_summary: record.summary,
                  checkpoint_fingerprint: checkpoint,
                  summarizer_model: record.model,
                  generated_at: record.ts,
                });
              }
              Err(err) => {
                tracing::warn!(
                  session_key = %key,
                  line = index + 1,
                  error = %err,
                  "stale compaction checkpoint ignored during replay"
                );
                replay.diagnostics.push(ReplayDiagnostic {
                  line: index + 1,
                  code: ReplayDiagnosticCode::StaleCompaction,
                });
              }
            },
            Err(err) => {
              tracing::warn!(
                session_key = %key,
                line = index + 1,
                error = %err,
                "stale compaction record ignored during replay"
              );
              replay.diagnostics.push(ReplayDiagnostic {
                line: index + 1,
                code: ReplayDiagnosticCode::StaleCompaction,
              });
            }
          }
        }
        _ => replay.diagnostics.push(ReplayDiagnostic {
          line: index + 1,
          code: ReplayDiagnosticCode::UnknownRecordType,
        }),
      }
    }

    if !replay.diagnostics.is_empty() {
      tracing::warn!(
        session_key = %key,
        messages = replay.messages.len(),
        diagnostics = replay.diagnostics.len(),
        "session replay completed with diagnostics"
      );
    } else {
      tracing::info!(
        session_key = %key,
        messages = replay.messages.len(),
        "session replay loaded"
      );
    }
    Ok(replay)
  }

  /// 会话文件是否存在（IO 错误按不存在处理并告警）。
  pub async fn exists(&self, key: &str) -> bool {
    let Ok(encoded) = SessionKeyCodec::encode(key) else {
      return false;
    };
    let path = self.root.join(format!("{encoded}.jsonl"));
    match fs::try_exists(&path).await {
      Ok(exists) => exists,
      Err(err) => {
        tracing::warn!(session_key = %key, error = %err, "session exists check failed");
        false
      }
    }
  }

  /// 删除会话文件；文件不存在视为成功（幂等）。
  ///
  /// 与 `append` 使用同一把逐键写锁，避免「写入进行中删除」产生幽灵文件。
  pub async fn delete(&self, key: &str) -> RiboResult<()> {
    let encoded = SessionKeyCodec::encode(key)?;
    let path = self.root.join(format!("{encoded}.jsonl"));

    let lock = self.write_lock(&encoded);
    let _guard = lock.lock().await;

    match fs::remove_file(&path).await {
      Ok(()) => {
        tracing::info!(session_key = %key, "session file removed");
        Ok(())
      }
      Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
      Err(err) => Err(session_create_failed(
        key,
        format!("delete session file: {err}"),
      )),
    }
  }

  /// 列出全部会话键（无稳定顺序，调用方排序）。
  ///
  /// 目录不存在 → 空列表；单个文件读取失败只告警并跳过。
  pub async fn list_keys(&self) -> RiboResult<Vec<String>> {
    if !fs::try_exists(&self.root).await.unwrap_or(false) {
      return Ok(Vec::new());
    }

    let mut keys = Vec::new();
    let mut entries = fs::read_dir(&self.root).await.map_err(|err| {
      session_create_failed("", format!("list sessions dir: {err}"))
    })?;

    while let Some(entry) = entries.next_entry().await.map_err(|err| {
      session_create_failed("", format!("read sessions dir entry: {err}"))
    })? {
      let path = entry.path();
      let is_jsonl = path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"));
      if !is_jsonl {
        continue;
      }

      let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();

      // 优先读首行 meta.key；缺失或损坏时回退文件名解码
      match Self::read_meta_key(&path).await {
        Some(key) => keys.push(key),
        None => {
          if let Some(key) = SessionKeyCodec::decode(stem) {
            keys.push(key);
          } else {
            tracing::warn!(file = %path.display(), "skip session file with undecodable name and missing meta");
          }
        }
      }
    }

    Ok(keys)
  }

  /// 从首行 `meta` 记录中取出原始会话键。
  async fn read_meta_key(path: &std::path::Path) -> Option<String> {
    let content = fs::read_to_string(path).await.ok()?;
    let first = content.lines().next()?;
    let value: serde_json::Value = serde_json::from_str(first).ok()?;
    if value.get("type").and_then(serde_json::Value::as_str) != Some("meta") {
      return None;
    }
    value
      .get("key")
      .and_then(serde_json::Value::as_str)
      .map(|s| s.to_string())
  }

  /// 取（必要时创建）某键的写锁，返回克隆的 `Arc` 以便跨 await 使用。
  fn write_lock(&self, encoded: &str) -> Arc<Mutex<()>> {
    self
      .locks
      .entry(encoded.to_string())
      .or_insert_with(|| Arc::new(Mutex::new(())))
      .clone()
  }

  /// 渲染首行 `meta` 记录。
  fn render_meta(&self, key: &str) -> RiboResult<String> {
    let record = WriteRecord {
      v: RECORD_VERSION,
      body: WriteBody::Meta {
        key,
        created_at: now_ms(),
      },
    };
    let line = serde_json::to_string(&record).map_err(|err| {
      session_create_failed(key, format!("serialize meta record: {err}"))
    })?;
    Ok(format!("{line}\n"))
  }
}

/// 会话键 ↔ 文件名的双向编解码。
///
/// 规则：
/// 1. 只保留 `[A-Za-z0-9._-]`，其余字节按 `%XX`（大写十六进制）编码；
/// 2. 拒绝空键；编码结果天然不含 `/`、`\`（防目录穿越）；
/// 3. 超过 [`MAX_ENCODED_LEN`] 时截断并追加 64 位摘要后缀。
pub struct SessionKeyCodec;

impl SessionKeyCodec {
  /// 编码会话键为文件名主干。
  ///
  /// # Errors
  ///
  /// 空键返回 `agent.session_create_failed`（reason = `empty session key`）。
  pub fn encode(key: &str) -> RiboResult<String> {
    if key.is_empty() {
      return Err(session_create_failed("", "empty session key"));
    }

    let mut encoded = String::with_capacity(key.len());
    for byte in key.bytes() {
      match byte {
        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b'_' => {
          encoded.push(byte as char);
        }
        other => {
          encoded.push_str(&format!("%{other:02X}"));
        }
      }
    }

    // 截断并追加摘要后缀，保证文件名长度可控且可追溯
    if encoded.chars().count() <= MAX_ENCODED_LEN {
      return Ok(encoded);
    }

    let mut prefix: String = encoded.chars().take(96).collect();
    // 回退到完整转义边界：若末尾是 '%' 或 '%X'，再向前回退，避免切断转义序列
    while prefix.ends_with('%')
      || (prefix.len() >= 2
        && prefix.as_bytes()[prefix.len() - 2] == b'%'
        && prefix.as_bytes()[prefix.len() - 1].is_ascii_hexdigit())
    {
      prefix.pop();
    }

    let mut hasher = DefaultHasher::new();
    encoded.hash(&mut hasher);
    let suffix = hasher.finish();
    Ok(format!("{prefix}-{suffix:016x}"))
  }

  /// 由文件名主干反解会话键。
  ///
  /// 仅对未截断的键成立（截断键以文件首行 `meta.key` 为准）；
  /// 百分号转义非法时返回 `None`。
  pub fn decode(stem: &str) -> Option<String> {
    let bytes = stem.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
      match bytes[i] {
        b'%' => {
          if i + 2 >= bytes.len() {
            return None;
          }
          let hi = (bytes[i + 1] as char).to_digit(16)?;
          let lo = (bytes[i + 2] as char).to_digit(16)?;
          out.push(((hi << 4) | lo) as u8);
          i += 3;
        }
        other => {
          out.push(other);
          i += 1;
        }
      }
    }
    String::from_utf8(out).ok()
  }
}

/// 写侧记录：版本在前，载荷经 `flatten` 平铺。
#[derive(Serialize)]
struct WriteRecord<'a> {
  v: u32,
  #[serde(flatten)]
  body: WriteBody<'a>,
}

/// 写侧载荷：`type` 为内部标签。
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WriteBody<'a> {
  Meta {
    key: &'a str,
    created_at: i64,
  },
  Message {
    ts: i64,
    message: &'a Message,
  },
  /// 摘要旁路记录：纯缓存，不截断 / 不删除任何 message，
  /// 也不决定模型读取起点。`v` 保持 1（硬切换不靠版本号区分）。
  Compaction {
    summary: &'a str,
    /// 指纹以十进制字符串写入，避免 JSON 数字精度问题。
    checkpoint: String,
    model: &'a str,
    ts: i64,
  },
}

/// 读侧摘要记录载荷：与 `WriteBody::Compaction` 字段一一对应。
#[derive(Deserialize)]
struct CompactionRecord {
  summary: String,
  checkpoint: String,
  model: String,
  ts: i64,
}

#[cfg(test)]
mod tests {
  use super::*;
  use ra_core::message::{
    AssistantMessage, Content, Message, TextContent, UserContent,
  };

  /// 构造一条用户文本消息。
  fn user_msg(text: &str) -> Message {
    Message::User(ra_core::message::UserMessage {
      content: UserContent::Text(text.to_string()),
      timestamp: 1_000,
    })
  }

  /// 构造一条助手文本消息。
  fn assistant_msg(text: &str) -> Message {
    let mut msg = AssistantMessage::empty("test-provider", "test-model", 1_001);
    msg.content = vec![Content::Text(TextContent {
      content: text.to_string(),
      signature: None,
    })];
    Message::Assistant(msg)
  }

  /// append 后 load 能完整回放，不重复、首行为 meta。
  #[tokio::test]
  async fn append_then_load_roundtrips_messages() {
    let dir =
      crate::test_util::test_dir("append_then_load_roundtrips_messages");
    let store = JsonlSessionStore::new(dir.clone());
    let msgs = vec![user_msg("hi"), assistant_msg("hello")];

    store.append("s1", &msgs).await.unwrap();

    let replay = store.load("s1").await.unwrap();
    assert_eq!(replay.messages, msgs);
    assert!(replay.diagnostics.is_empty());

    // 首行必须是 meta
    let file = dir.join("s1.jsonl");
    let content = std::fs::read_to_string(file).unwrap();
    let first_line = content.lines().next().unwrap();
    assert!(first_line.contains("\"type\":\"meta\""));
  }

  /// 破损尾行 + 未知 type 行被跳过并记诊断，不报错。
  #[tokio::test]
  async fn load_tolerates_torn_tail_and_unknown_records() {
    let dir = crate::test_util::test_dir(
      "load_tolerates_torn_tail_and_unknown_records",
    );
    let store = JsonlSessionStore::new(dir.clone());

    // 直接构造完整文件内容（meta + 合法消息 + 未知 type + 截断尾行），
    // 一次性写入，避免「append 后再 std::fs 覆盖」与 tokio 异步关句柄的竞态。
    let valid = user_msg("hi");
    let mut content = String::new();
    content.push_str(
      &serde_json::to_string(&WriteRecord {
        v: RECORD_VERSION,
        body: WriteBody::Meta {
          key: "s1",
          created_at: 1,
        },
      })
      .unwrap(),
    );
    content.push('\n');
    content.push_str(
      &serde_json::to_string(&WriteRecord {
        v: RECORD_VERSION,
        body: WriteBody::Message {
          ts: 2,
          message: &valid,
        },
      })
      .unwrap(),
    );
    content.push('\n');
    content.push_str("{\"v\":1,\"type\":\"weird\",\"data\":{}}\n");
    content.push_str("{\"v\":1,\"type\":\"message\",\"ts\":1,\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"x\"}");

    let file = dir.join("s1.jsonl");
    std::fs::write(&file, content).unwrap();

    let replay = store.load("s1").await.unwrap();
    // 合法消息仍回放
    assert_eq!(replay.messages.len(), 1);
    assert_eq!(replay.messages[0], valid);
    // 至少包含 TornTail 与 UnknownRecordType
    let codes: Vec<_> = replay.diagnostics.iter().map(|d| d.code).collect();
    assert!(codes.contains(&ReplayDiagnosticCode::UnknownRecordType));
    assert!(codes.contains(&ReplayDiagnosticCode::TornTail));
  }

  /// 键编码不含分隔符、短键可往返、长键被截断且稳定。
  #[test]
  fn key_codec_roundtrip_and_length_cap() {
    // 短键：纯 ASCII 可往返
    assert_eq!(
      SessionKeyCodec::decode(&SessionKeyCodec::encode("abc-123").unwrap())
        .unwrap(),
      "abc-123"
    );

    // 含路径分隔符与空格：编码后不出现 '/' '\\'
    let encoded = SessionKeyCodec::encode("a/b c").unwrap();
    assert!(!encoded.contains('/'));
    assert!(!encoded.contains('\\'));
    assert!(!encoded.contains(' '));

    // 超长键：截断后长度 ≤ MAX_ENCODED_LEN，且多次编码稳定
    let long = "a".repeat(500);
    let enc1 = SessionKeyCodec::encode(&long).unwrap();
    let enc2 = SessionKeyCodec::encode(&long).unwrap();
    assert_eq!(enc1, enc2);
    assert!(enc1.chars().count() <= MAX_ENCODED_LEN);

    // 空键报错
    assert!(SessionKeyCodec::encode("").is_err());
  }

  /// list_keys 列出键、delete 幂等。
  #[tokio::test]
  async fn delete_is_idempotent_and_list_keys_roundtrip() {
    let dir = crate::test_util::test_dir(
      "delete_is_idempotent_and_list_keys_roundtrip",
    );
    let store = JsonlSessionStore::new(dir.clone());

    store.append("s1", &[user_msg("a")]).await.unwrap();
    store.append("s2", &[user_msg("b")]).await.unwrap();

    let mut keys = store.list_keys().await.unwrap();
    keys.sort();
    assert_eq!(keys, vec!["s1".to_string(), "s2".to_string()]);

    assert!(store.exists("s1").await);
    store.delete("s1").await.unwrap();
    assert!(!store.exists("s1").await);

    // 二次删除仍成功
    store.delete("s1").await.unwrap();

    let mut keys = store.list_keys().await.unwrap();
    keys.sort();
    assert_eq!(keys, vec!["s2".to_string()]);
  }

  /// 补充：缺失文件返回空回放且无诊断。
  #[tokio::test]
  async fn load_missing_file_returns_empty() {
    let dir = crate::test_util::test_dir("load_missing_file_returns_empty");
    let store = JsonlSessionStore::new(dir.clone());
    let replay = store.load("ghost").await.unwrap();
    assert!(replay.messages.is_empty());
    assert!(replay.diagnostics.is_empty());
  }

  /// 补充：空批次 append 不创建文件。
  #[tokio::test]
  async fn append_empty_batch_creates_no_file() {
    let dir = crate::test_util::test_dir("append_empty_batch_creates_no_file");
    let store = JsonlSessionStore::new(dir.clone());
    store.append("empty", &[]).await.unwrap();
    assert!(!store.exists("empty").await);
  }

  /// append_compaction 写入旁路记录，load_compaction 读回四字段，
  /// 且不污染 message 转录（不截断 / 不删除）。
  #[tokio::test]
  async fn compaction_record_roundtrip_does_not_touch_messages() {
    let dir = crate::test_util::test_dir(
      "compaction_record_roundtrip_does_not_touch_messages",
    );
    let store = JsonlSessionStore::new(dir.clone());
    store
      .append("s1", &[user_msg("q1"), assistant_msg("a1")])
      .await
      .unwrap();

    let cache = CompactionCache {
      last_summary: "kept decisions".to_string(),
      checkpoint_fingerprint: 12_345,
      summarizer_model: "glm-4.5-flash".to_string(),
      generated_at: 999,
    };
    store.append_compaction("s1", &cache).await.unwrap();

    // 记录在原消息之后
    let file = dir.join("s1.jsonl");
    let content = std::fs::read_to_string(file).unwrap();
    assert!(content.contains("\"type\":\"compaction\""));
    assert!(content.contains("\"checkpoint\":\"12345\""));

    // 回放：messages 不含摘要记录
    let replay = store.load("s1").await.unwrap();
    assert_eq!(replay.messages.len(), 2);
    assert_eq!(replay.compaction, Some(cache.clone()));
    assert!(replay.diagnostics.is_empty());

    // 专用读取最后一条
    assert_eq!(store.load_compaction("s1").await.unwrap(), Some(cache));
  }

  /// 首轮（无 message）直接写摘要记录时自动补 meta 首行。
  #[tokio::test]
  async fn compaction_record_first_creates_meta_line() {
    let dir =
      crate::test_util::test_dir("compaction_record_first_creates_meta_line");
    let store = JsonlSessionStore::new(dir.clone());
    let cache = CompactionCache {
      last_summary: "s".to_string(),
      checkpoint_fingerprint: 1,
      summarizer_model: "m".to_string(),
      generated_at: 2,
    };
    store.append_compaction("solo", &cache).await.unwrap();

    let content = std::fs::read_to_string(dir.join("solo.jsonl")).unwrap();
    let meta_count = content
      .lines()
      .filter(|l| l.contains("\"type\":\"meta\""))
      .count();
    assert_eq!(meta_count, 1);
    assert_eq!(store.load_compaction("solo").await.unwrap(), Some(cache));
  }

  /// 损坏的 compaction 记录记 StaleCompaction 诊断，不中断回放。
  #[tokio::test]
  async fn load_marks_stale_compaction_record() {
    let dir = crate::test_util::test_dir("load_marks_stale_compaction_record");
    let store = JsonlSessionStore::new(dir.clone());
    let mut content = String::new();
    content.push_str(
      "{\"v\":1,\"type\":\"meta\",\"key\":\"s1\",\"created_at\":1}\n",
    );
    content.push_str("{\"v\":1,\"type\":\"compaction\",\"summary\":\"s\",\"checkpoint\":\"not-a-number\",\"model\":\"m\",\"ts\":2}\n");
    content.push_str("{\"v\":1,\"type\":\"message\",\"ts\":3,\"message\":{\"role\":\"user\",\"content\":\"hi\",\"timestamp\":0}}\n");
    std::fs::write(dir.join("s1.jsonl"), content).unwrap();

    let replay = store.load("s1").await.unwrap();
    assert_eq!(replay.messages.len(), 1);
    assert!(replay.compaction.is_none());
    assert!(
      replay
        .diagnostics
        .iter()
        .any(|d| d.code == ReplayDiagnosticCode::StaleCompaction)
    );
  }

  /// 补充：多次 append 只产生一行 meta。
  #[tokio::test]
  async fn append_uses_single_meta_line() {
    let dir = crate::test_util::test_dir("append_uses_single_meta_line");
    let store = JsonlSessionStore::new(dir.clone());
    store.append("s1", &[user_msg("a")]).await.unwrap();
    store.append("s1", &[assistant_msg("b")]).await.unwrap();

    let file = dir.join("s1.jsonl");
    let content = std::fs::read_to_string(file).unwrap();
    let meta_count = content
      .lines()
      .filter(|l| l.contains("\"type\":\"meta\""))
      .count();
    assert_eq!(meta_count, 1);

    // 回放两条消息
    let replay = store.load("s1").await.unwrap();
    assert_eq!(replay.messages.len(), 2);
  }
}
