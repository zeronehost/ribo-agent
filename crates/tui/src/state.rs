//! `AppState` 状态树（16 详细设计 §4）。
//!
//! 集中式状态：所有键盘事件与 `AgentEvent` 的最终落点。`editor` 为
//! [`RefCell`]\[`TextArea`\]（非 `Sync`）——`AppState` 整体由**单一主循环
//! 任务独占持有**，绝不跨任务传递（16 §3.1 R-R4）；键盘 task 只投喂
//! `AppInput` 事件。

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Duration;

use ra_cli::config::ConfigMode;
use ra_cli::config::assemble::TurnHandle;
use tui_textarea::TextArea;

/// 全局可变状态树：所有事件与按键的最终落点（16 §4）。
pub struct AppState {
  /// 对话区状态（气泡队列 + 流式指针）。
  pub chat: ChatState,
  /// 当前会话全部工具调用（按发生序）。
  pub tools: Vec<ToolRun>,
  /// 状态行数据。
  pub status: StatusLine,
  /// 输入编辑器（`RefCell` 解决渲染需 `&mut self`，16 §3.1 R-B3）。
  pub editor: RefCell<TextArea<'static>>,
  /// `/help` 帮助浮层开关。
  pub help_open: bool,
  /// 通用浮层正文（`/skills` `/sessions` `/history` `/status` 输出）。
  pub overlay_text: Option<String>,
  /// 回合在途状态。
  pub turn: TurnState,
  /// 对话区粘底滚动状态机（含行数缓存）。
  pub scroll: ChatScrollState,
  /// Ctrl-C 超时（进入「再按一次即强退」子状态的等待时长）。
  pub ctrl_c_timeout: Duration,
  /// 第一次 Ctrl-C 已触发，进入退出等待态（16 §8）。
  pub exiting: bool,
  /// 超时后「再按一次即强退」子状态（16 §8 第 2 步 (b)）。
  pub force_ready: bool,
}

impl AppState {
  /// 构造初始状态（编辑器为空、跟随滚动、Idle）。
  pub fn new(
    mode: ConfigMode,
    warnings: Vec<String>,
    session_key: String,
  ) -> Self {
    Self {
      chat: ChatState::default(),
      tools: Vec::new(),
      status: StatusLine {
        mode,
        warnings,
        session_key,
        total_input: 0,
        total_output: 0,
        in_flight: false,
        context_pct: None,
      },
      editor: RefCell::new(TextArea::new(vec![String::new()])),
      help_open: false,
      overlay_text: None,
      turn: TurnState::Idle,
      scroll: ChatScrollState::default(),
      ctrl_c_timeout: Duration::from_secs(2),
      exiting: false,
      force_ready: false,
    }
  }
}

/// 对话区状态：气泡队列 + 当前流式气泡指针。
#[derive(Default)]
pub struct ChatState {
  /// 气泡队列（prompt / assistant 流式 / tool_result）。
  pub messages: Vec<ChatBubble>,
  /// 当前正在流式渲染的助手气泡 id（`MessageUpdate` 直写此气泡）。
  pub streaming_id: Option<u64>,
  /// 气泡 id 发生器（单调递增，作行数缓存键）。
  next_id: u64,
  /// 待失效气泡 id 暂存（`invalidate_cache` 写入，缓存刷新前消费）。
  invalidated: Vec<u64>,
}

impl ChatState {
  /// 追加气泡并返回其 id（行数缓存键）。
  pub fn push(&mut self, body: ChatBubbleBody) -> u64 {
    let id = self.next_id;
    self.next_id += 1;
    self.messages.push(ChatBubble { id, body });
    id
  }

  /// 按 id 取可变气泡（流式直写用）。
  pub fn bubble_mut(&mut self, id: u64) -> Option<&mut ChatBubble> {
    self.messages.iter_mut().find(|b| b.id == id)
  }

  /// 按 id 使行数缓存失效（流式直写后调用）。
  pub fn invalidate_cache(&mut self, id: u64) {
    self.invalidated.push(id);
  }

  /// 本轮被直写、行数缓存待失效的气泡 id（主循环刷新缓存前消费）。
  pub(crate) fn take_invalidated(&mut self) -> Vec<u64> {
    std::mem::take(&mut self.invalidated)
  }
}

/// 对话气泡。
pub struct ChatBubble {
  /// 气泡 id（行数缓存键，单调递增）。
  pub id: u64,
  /// 气泡内容。
  pub body: ChatBubbleBody,
}

/// 气泡内容三型（用户 / 助手流式 / 工具结果）。
pub enum ChatBubbleBody {
  /// 用户消息。
  User(String),
  /// 助手消息（流式直写 text/thinking；终态填 error）。
  Assistant {
    /// 可见文本（Text 块聚合）。
    text: String,
    /// 推理文本（Thinking 块聚合，暗色展示）。
    thinking: String,
    /// 终端错误原因（`error_message`）。
    error: Option<String>,
  },
  /// 工具结果气泡。
  ToolResult {
    /// 工具名。
    tool: String,
    /// 结果文本（content Text 聚合）。
    text: String,
    /// 是否失败。
    is_error: bool,
  },
}

/// 工具运行条目（工具状态区）。
pub struct ToolRun {
  /// 工具名。
  pub name: String,
  /// 执行阶段。
  pub phase: ToolPhase,
}

/// 工具执行阶段。
pub enum ToolPhase {
  /// 执行中（`ToolExecutionStart`）。
  Running,
  /// 已结束（`is_error` 区分成败；`ToolExecutionEnd`）。
  Done(bool),
}

/// 状态行数据。
pub struct StatusLine {
  /// 运行模式。
  pub mode: ConfigMode,
  /// 装配期降级原因（横幅曾经内容，压缩为计数 + 悬浮展示）。
  pub warnings: Vec<String>,
  /// 当前会话键。
  pub session_key: String,
  /// 累计输入 token（真实 `Usage` 累加）。
  pub total_input: u64,
  /// 累计输出 token。
  pub total_output: u64,
  /// 是否有在途回合。
  pub in_flight: bool,
  /// 上下文占比占位（`None` 显示「—」；依赖 root 转录估算，本期未填）。
  pub context_pct: Option<f32>,
}

/// 回合在途状态。
pub enum TurnState {
  /// 空闲。
  Idle,
  /// 在途（`key` 为在途会话键；`handle` 为回合句柄）。
  Running { key: String, handle: TurnHandle },
}

/// 对话区粘底滚动状态机（pi-tui ScrollViewState 语义移植）。
///
/// `line_cache` 为「气泡 id → (上次计算宽度, 折行行数)」缓存：Resize 时
/// 仅宽度变化的气泡才重算（16 §7.5 第 4 步，避免 O(n) 全量重排）。
#[derive(Default)]
pub struct ChatScrollState {
  /// 视口顶部偏移（视觉行数）。
  pub offset: u16,
  /// 跟随底部：新消息到达时锁底；上滚置 false，触底恢复 true。
  pub follow_end: bool,
  /// 气泡折行行数缓存（id → (last_width, line_count)）。
  pub line_cache: HashMap<u64, (u16, usize)>,
}

impl ChatScrollState {
  /// 取缓存行数（宽度一致才命中）。
  pub fn cached_lines(&self, id: u64, width: u16) -> Option<usize> {
    self
      .line_cache
      .get(&id)
      .filter(|(w, _)| *w == width)
      .map(|(_, count)| *count)
  }

  /// 写入缓存。
  pub fn store_lines(&mut self, id: u64, width: u16, count: usize) {
    self.line_cache.insert(id, (width, count));
  }

  /// 丢弃全部缓存（宽度变化 / Lagged 重置）。
  pub fn clear_cache(&mut self) {
    self.line_cache.clear();
  }
}
