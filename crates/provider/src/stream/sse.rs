//! SSE 传输层：字节 → 行 → 帧。
//!
//! 与具体服务商无关：只实现 text/event-stream 的传输层惯例——
//! 行切分（`\r\n` / `\n`）、注释跳过（`:` 前缀）、字段提取、帧边界（空行）。
//! `[DONE]` 哨兵与 JSON 语义属方言层，由各适配器的 `dispatch_frame` 处理。

/// 一帧完整的 SSE 数据：事件名（可选）+ data 载荷。
#[derive(Debug)]
pub(crate) struct SseFrame {
  /// `event:` 行内容；多数服务商不发，缺省 `None`。
  pub(crate) event: Option<String>,
  /// `data:` 载荷（多行 data 以 `\n` 连接为一帧，SSE 规范）。
  pub(crate) data: String,
}

/// SSE 传输层解析：字节流 → 行 → 帧。
///
/// 批次式接口：每次 `feed` 返回本轮完成的帧，EOF 时 `finish` 冲刷残余。
#[derive(Debug, Default)]
pub(crate) struct SseParser {
  /// 未成行的字节缓冲。
  buf: Vec<u8>,
  /// 当前帧已收集的 data 载荷。
  data: String,
  /// 当前帧的事件名。
  event: Option<String>,
}

impl SseParser {
  /// 喂入新字节，返回随之完成的帧（空行触发，一次喂入可能完成多帧）。
  ///
  /// # 参数
  ///
  /// - `bytes`：本轮 `response.chunk()` 读到的原始字节。
  ///
  /// # 返回
  ///
  /// 本轮喂入期间完成的帧，按到达顺序排列。
  pub(crate) fn feed(&mut self, bytes: &[u8]) -> Vec<SseFrame> {
    // ===== 第一步：并入缓冲，逐行消费 =====
    self.buf.extend_from_slice(bytes);
    let mut frames = Vec::new();
    // 以 '\n' 为行界反复取行；'\r\n' 的 '\r' 在行处理时剥离
    while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
      // drain(..=pos) 连 '\n' 一并取出，剩余字节自动前移
      let line_bytes: Vec<u8> = self.buf.drain(..=pos).collect();
      // 行内必为完整 UTF-8（续字节不含 '\n'）；lossy 仅防提供方发送非法序列
      let line = String::from_utf8_lossy(&line_bytes[..pos]);
      self.push_line(line.trim_end_matches('\r'), &mut frames);
    }
    frames
  }

  /// 流结束时冲刷残余字节成帧；部分服务商末帧不带空行（宽松解析）。
  ///
  /// # 返回
  ///
  /// 冲刷产生的帧（可能为空：流以空行正常收尾时无残余）。
  pub(crate) fn finish(&mut self) -> Vec<SseFrame> {
    let mut frames = Vec::new();

    // ===== 第一步：残余未换行字节补作最后一行 =====
    if !self.buf.is_empty() {
      // mem::take 取出缓冲：Cow 借用 bytes 局部变量，避免与 push_line
      // 的可变借用（&mut self）冲突
      let bytes = std::mem::take(&mut self.buf);
      let line = String::from_utf8_lossy(&bytes);
      self.push_line(line.trim_end_matches('\r'), &mut frames);
    }

    // ===== 第二步：data 非空则无条件成帧 =====
    // 末帧无空行触发的宽松兜底
    if !self.data.is_empty() {
      frames.push(SseFrame {
        event: self.event.take(),
        data: std::mem::take(&mut self.data),
      });
    }
    frames
  }

  /// 处理单行：帧边界 / 注释 / 字段提取（`feed` 与 `finish` 共用）。
  fn push_line(&mut self, line: &str, frames: &mut Vec<SseFrame>) {
    if line.is_empty() {
      // ===== 帧边界：仅当收集到 data 才成帧 =====
      // 纯 event 无 data 的帧对补全协议无意义（dispatch 只认 JSON 载荷），
      // 直接丢弃——否则心跳帧（连续空行、": ping" 后的空行）会被
      // 误判为 INVALID_RESPONSE 终结整条流。
      if !self.data.is_empty() {
        frames.push(SseFrame {
          event: self.event.take(),
          data: std::mem::take(&mut self.data),
        });
      }
      // 无 data 的 event 一并复位，避免泄漏到下一帧
      self.event = None;
    } else if line.starts_with(':') {
      // 注释行：SSE 规范忽略（服务商心跳 keep-alive 常用 ": ping"）
    } else {
      // 字段提取：冒号切分 + value 剥离单个前导空格（SSE 规范）
      let (field, value) = match line.split_once(':') {
        Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
        // 只有字段名没有冒号：规范规定视作空值
        None => (line, ""),
      };
      match field {
        "data" => {
          // 多行 data 以 '\n' 连接为单帧载荷（SSE 规范）
          if !self.data.is_empty() {
            self.data.push('\n');
          }
          self.data.push_str(value);
        }
        "event" => {
          // 规范语义为覆盖；重复 event 行属提供方缺陷，取最新值（宽松）
          self.event = Some(value.to_string());
        }
        // id: / retry: 及未知字段与帧语义无关，一律忽略
        _ => {}
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// 场景：标准帧 + 多行 data + 注释行 + 无空行末帧。
  ///
  /// 预期：注释被忽略，多行 data 以换行连接，EOF 冲刷出末帧。
  #[test]
  fn parser_handles_multiline_comments_and_final_frame() {
    let mut parser = SseParser::default();
    let frames = parser.feed(
      b": ping\r\nevent: delta\r\ndata: {\"a\":1}\r\ndata: {\"b\":2}\r\n\r\n",
    );
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].event.as_deref(), Some("delta"));
    assert_eq!(frames[0].data, "{\"a\":1}\n{\"b\":2}");

    // 末帧无空行：finish 兜底成帧
    parser.feed(b"event: stop\ndata: {}");
    let tail = parser.finish();
    assert_eq!(tail.len(), 1);
    assert_eq!(tail[0].event.as_deref(), Some("stop"));
    assert_eq!(tail[0].data, "{}");
  }

  /// 场景：纯 event 无 data 的心跳帧。
  ///
  /// 预期：不成帧，避免被当作空载荷误判。
  #[test]
  fn parser_drops_event_without_data() {
    let mut parser = SseParser::default();
    assert!(parser.feed(b"event: ping\n\n").is_empty());
    assert!(parser.finish().is_empty());
  }
}
