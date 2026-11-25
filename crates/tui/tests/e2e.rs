//! ra-tui E2E：伪终端全链路。
//!
//! 用 `portable-pty` 起伪终端驱动真实 bin（`CARGO_BIN_EXE_ribo-tui`）：
//! 启动（mock 模式）→ 输入消息 → Ctrl-C 退出 → 断言**终端已恢复**
//! （备用屏退出序列出现）且退出码 0；中文 / Emoji 输入专项。
//!
//! 全部用例走真实 conPTY（Windows）/ OpenPTY（Unix）——mock 模式空脚本
//! 下回合以 INVALID_REQUEST 终结属预期，不影响退出。

use std::io::{Read, Write};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

/// 单条 PTY 会话收集结果。
struct SessionOutput {
  /// 进程退出码。
  exit_code: Option<u32>,
  /// 全量输出字节（含 ANSI 序列）。
  output: Vec<u8>,
}

/// 驱动一次 TUI 会话：注入输入序列 → 超时窗口后发 Ctrl-C → 收集退出码
/// 与输出。
fn drive_tui(
  root: &std::path::Path,
  inputs: &[&[u8]],
  total_timeout: Duration,
) -> SessionOutput {
  let pty_system = native_pty_system();
  let pair = pty_system
    .openpty(PtySize {
      rows: 24,
      cols: 80,
      pixel_width: 0,
      pixel_height: 0,
    })
    .expect("openpty 成功");

  let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_ribo-tui"));
  cmd.arg("--root");
  cmd.arg(root);
  cmd.cwd(root);
  let mut child = pair
    .slave
    .spawn_command(cmd)
    .expect("伪终端内启动 ribo-tui 成功");
  drop(pair.slave); // 子进程独占 TTY

  // writer 需在读取线程与主线程间共享：conPTY 没有终端仿真器，终端
  // 查询必须由 harness 代答（见下方 DSR 处理）。
  let writer =
    Arc::new(Mutex::new(pair.master.take_writer().expect("take writer")));
  let mut reader = pair.master.try_clone_reader().expect("clone reader");

  // 读取线程：逐块经 channel 转发，收集端不等待 EOF。
  // Windows conPTY 的 master 读句柄在子进程退出后未必返回 EOF——若在此
  // 攒满全部输出再 join()，线程可能永久阻塞（CI 挂死）。
  let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>();
  let reply_writer = Arc::clone(&writer);
  let reader_thread = std::thread::spawn(move || {
    let mut chunk = [0u8; 4096];
    loop {
      match reader.read(&mut chunk) {
        Ok(0) => break,
        Ok(n) => {
          let data = &chunk[..n];
          // 光标位置查询（DSR, ESC[6n）：真实终端会回复 ESC[row;colR，
          // conPTY 不会——若 harness 不代答，进程会同步阻塞在启动时，
          // 连备用屏都进不去。
          if data.windows(4).any(|w| w == b"\x1b[6n") {
            let mut guard = reply_writer.lock().expect("writer lock");
            let _ = guard.write_all(b"\x1b[1;1R");
            let _ = guard.flush();
          }
          if out_tx.send(data.to_vec()).is_err() {
            break; // 收集端已关闭
          }
        }
        Err(_) => break,
      }
    }
  });

  // 按序列注入输入（间隔给渲染循环让路）
  for input in inputs {
    std::thread::sleep(Duration::from_millis(400));
    let mut guard = writer.lock().expect("writer lock");
    guard.write_all(input).expect("写入输入");
    let _ = guard.flush();
  }

  // 总超时窗口：超时后发 Ctrl-C（raw mode 下字节 0x03）请求退出
  let deadline = Instant::now() + total_timeout;
  let mut exit_code: Option<u32> = None;
  while Instant::now() < deadline {
    if let Some(status) = child.try_wait().expect("try_wait") {
      exit_code = Some(status.exit_code());
      break;
    }
    std::thread::sleep(Duration::from_millis(100));
  }
  if exit_code.is_none() {
    // 超时兜底：Ctrl-C 触发优雅退出（模拟用户中断）
    let mut guard = writer.lock().expect("writer lock");
    let _ = guard.write_all(&[0x03]);
    let _ = guard.flush();
    drop(guard);
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
      if let Some(status) = child.try_wait().expect("try_wait") {
        exit_code = Some(status.exit_code());
        break;
      }
      std::thread::sleep(Duration::from_millis(100));
    }
  }

  // 兜底：进程仍存活则强制终止，杜绝孤儿进程与无限等待。用 try_wait
  // 轮询而非 wait()，避免 kill 失败时再次阻塞。
  if exit_code.is_none() {
    let _ = child.kill();
    for _ in 0..20 {
      if let Some(status) = child.try_wait().expect("try_wait") {
        exit_code = Some(status.exit_code());
        break;
      }
      std::thread::sleep(Duration::from_millis(100));
    }
  }

  // 限时抽干已收到的输出：不依赖 reader 线程结束（EOF 不保证）。
  let mut output = Vec::new();
  let collect_deadline = Instant::now() + Duration::from_secs(2);
  loop {
    match out_rx.recv_timeout(Duration::from_millis(100)) {
      Ok(chunk) => output.extend_from_slice(&chunk),
      Err(RecvTimeoutError::Disconnected) => break,
      Err(RecvTimeoutError::Timeout) => {
        if Instant::now() >= collect_deadline {
          break;
        }
      }
    }
  }

  // 关闭 pty 句柄促使 reader 线程退出；不 join（见上），由测试进程回收。
  drop(out_rx);
  drop(writer);
  drop(pair.master);
  drop(reader_thread); // detach，避免无限等待
  SessionOutput { exit_code, output }
}

/// 剥离 ANSI CSI 序列与控制字符，重建可读文本供断言使用。
///
/// ratatui 在真 PTY（Linux）下是 diff 渲染——每个字符用 `[row;colH` 单独
/// 定位，中文字符在原始字节流中被 ANSI 序列隔开；Windows conPTY 则整行
/// 重绘、字符连续。断言必须在剥离后的纯文本上进行，两个平台才等价。
fn plain(output: &[u8]) -> String {
  let s = String::from_utf8_lossy(output);
  let mut out = String::with_capacity(s.len());
  let mut chars = s.chars().peekable();
  while let Some(c) = chars.next() {
    match c {
      '\x1b' => {
        if chars.peek() == Some(&'[') {
          chars.next();
          for c2 in chars.by_ref() {
            if c2.is_ascii_alphabetic() {
              break; // CSI 终止符（H / m / J 等）
            }
          }
        } else {
          chars.next(); // 两字符转义序列
        }
      }
      c if c.is_control() => {}
      c => out.push(c),
    }
  }
  out
}

// 场景：启动 → 输入英文消息 → Ctrl-C 退出。
// 预期：退出码 0（优雅退出）；输出含备用屏进入/退出序列（终端已恢复）
// 与对话区标题（界面已渲染）。
#[test]
fn pty_exit_restores_terminal() {
  let dir = tempfile::tempdir().expect("temp dir");
  let session = drive_tui(dir.path(), &[b"hello\r"], Duration::from_secs(6));

  let output = String::from_utf8_lossy(&session.output);
  assert_eq!(
    session.exit_code,
    Some(0),
    "Ctrl-C 应优雅退出（退出码 0）；输出预览：{:?}",
    output.chars().take(600).collect::<String>()
  );
  assert!(
    output.contains("\x1b[?1049h"),
    "应进入备用屏（TUI 启动），实际输出前 400 字节：{}",
    output.chars().take(400).collect::<String>()
  );
  assert!(
    output.contains("\x1b[?1049l"),
    "退出应恢复终端（LeaveAlternateScreen）"
  );
  assert!(
    plain(&session.output).contains("对话"),
    "对话区应渲染（标题出现）"
  );
}

// 场景：中文 / Emoji 输入经 PTY 全链路。
// 预期：退出码 0；用户气泡渲染出原始中文（MessageStart 进入转录）。
#[test]
fn pty_cjk_and_emoji_input() {
  let dir = tempfile::tempdir().expect("temp dir");
  // 字节字符串不支持 unicode 转义：用 str 字面量取 UTF-8 字节
  let input = "你好 🌍\r";
  let session =
    drive_tui(dir.path(), &[input.as_bytes()], Duration::from_secs(6));

  let output = String::from_utf8_lossy(&session.output);
  assert_eq!(
    session.exit_code,
    Some(0),
    "中文输入后 Ctrl-C 应优雅退出；输出预览：{:?}",
    output.chars().take(600).collect::<String>()
  );
  assert!(
    plain(&session.output).contains("你好"),
    "中文应经编辑器进入对话气泡（mock 回合 MessageStart 回显），实际含：{}",
    plain(&session.output).chars().take(300).collect::<String>()
  );
}
