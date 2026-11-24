//! `AppState` 状态树。
//!
//! 集中式状态：所有键盘事件与 `AgentEvent` 的最终落点。`editor` 为
//! [`RefCell`]\[`TextArea`\]（非 `Sync`）——`AppState` 整体由**单一主循环
//! 任务独占持有**，绝不跨任务传递；键盘 task 只投喂
//! `AppInput` 事件。

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Duration;

use ra_cli::config::ConfigMode;
use ra_cli::config::assemble::TurnHandle;
use tui_textarea::TextArea;

/// 全局可变状态树：所有事件与按键的最终落点。
pub struct AppState {
  /// 对话区状态（气泡队列 + 流式指针）。
  pub chat: ChatState,
  /// 状态行数据。
  pub status: StatusLine,
  /// 输入编辑器（`RefCell` 解决渲染需 `&mut self`，R-B3）。
  pub editor: RefCell<TextArea<'static>>,
  /// `/help` 帮助浮层开关。
  pub help_open: bool,
  /// 通用浮层正文（`/skills` `/sessions` `/history` `/status` 输出）。
  pub overlay_text: Option<String>,
  /// 回合在途状态。
  pub turn: TurnState,
  /// loading 动画帧计数（主循环 tick 驱动；空闲时不递增）。
  pub spinner_tick: u64,
  /// 对话区有效宽度（终端宽 - 侧栏宽；行数缓存/滚动上限共用）。
  pub dialog_width: u16,
  /// 对话区粘底滚动状态机（含行数缓存）。
  pub scroll: ChatScrollState,
  /// Ctrl-C 超时（进入「再按一次即强退」子状态的等待时长）。
  pub ctrl_c_timeout: Duration,
  /// 第一次 Ctrl-C 已触发，进入退出等待态。
  pub exiting: bool,
  /// 超时后「再按一次即强退」子状态。
  pub force_ready: bool,
}

impl AppState {
  /// 构造初始状态（编辑器为空、跟随滚动、Idle）。
  pub fn new(
    mode: ConfigMode,
    provider: String,
    model: String,
    warnings: Vec<String>,
    session_key: String,
  ) -> Self {
    Self {
      chat: ChatState::default(),
      status: StatusLine {
        mode,
        provider,
        model,
        warnings,
        session_key,
        total_input: 0,
        total_output: 0,
        in_flight: false,
        compaction: None,
        context_window: 0,
      },
      editor: RefCell::new(TextArea::new(vec![String::new()])),
      help_open: false,
      overlay_text: None,
      turn: TurnState::Idle,
      spinner_tick: 0,
      dialog_width: crate::render_helpers::FALLBACK_WIDTH,
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

  /// 清空气泡队列（切换会话时重建历史用）；保留 `next_id` 递增
  /// 以免与行数缓存的旧条目 id 冲突。
  pub fn clear(&mut self) {
    self.messages.clear();
    self.streaming_id = None;
    self.invalidated.clear();
  }

  /// 按 id 取可变气泡（流式直写用）。
  pub fn bubble_mut(&mut self, id: u64) -> Option<&mut ChatBubble> {
    self.messages.iter_mut().find(|b| b.id == id)
  }

  /// 按 id 使行数缓存失效（流式直写后调用）。
  pub fn invalidate_cache(&mut self, id: u64) {
    self.invalidated.push(id);
  }

  /// 按工具调用 id 找气泡位置（Start/End/回灌去重关联；id 为空返回 None）。
  pub fn tool_bubble_index(&self, call_id: &str) -> Option<usize> {
    if call_id.is_empty() {
      return None;
    }
    self.messages.iter().position(|b| {
      matches!(&b.body, ChatBubbleBody::ToolResult { call_id: Some(id), .. } if id == call_id)
    })
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

/// 气泡内容四型（用户 / 助手流式 / 工具结果 / 系统操作痕迹）。
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
  /// 工具调用气泡（在对话流中按发生序内联显示）。
  ToolResult {
    /// 工具调用 id（`ToolExecutionStart` 携带；用于 Start/End/回灌去重关联）。
    call_id: Option<String>,
    /// 工具名。
    tool: String,
    /// 结果文本（content Text 聚合；执行中为占位说明）。
    text: String,
    /// 是否失败。
    is_error: bool,
    /// 是否仍在执行中（Start 后、End 前为真）。
    running: bool,
  },
  /// 上下文压缩痕迹：对话流中的持久提示，不显示摘要内容与条数。
  /// `failed == true` 表示摘要失败、本轮已回退滑窗。
  Compaction {
    /// 是否失败回退。
    failed: bool,
  },
}

impl ChatBubbleBody {
  /// 压缩气泡前缀（渲染与行数缓存共用，防漂移）。
  pub const COMPACTION_PREFIX: &'static str = "sys › ";
  /// 压缩成功气泡正文（渲染与行数缓存共用，防漂移）。
  pub const COMPACTION_TEXT: &'static str = "已压缩上下文（早期对话已摘要）";
  /// 压缩失败气泡正文（渲染与行数缓存共用，防漂移）。
  pub const COMPACTION_FAILED_TEXT: &'static str =
    "上下文压缩失败，本轮已回退滑窗";
}

/// 状态面板数据（左侧栏 + `/status` 共用）。
pub struct StatusLine {
  /// 运行模式。
  pub mode: ConfigMode,
  /// 当前主模型归属的 provider（装配期确定）。
  pub provider: String,
  /// 当前主模型名。
  pub model: String,
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
  /// 上下文压缩三态统计（总量 / 已压缩 / 当前）。
  /// `None` 仅在未绑定估算器 / 未回放时出现；冷启动回放后由
  /// `SessionStatsEstimator` 写入三态（当前量为 0，尚未发起请求），
  /// 侧栏显示数值而非「—」。
  pub compaction: Option<ra_agent::session::CompactionStats>,
  /// 主模型上下文窗口（token；0 = 未知，占比显示「—」）。
  pub context_window: u32,
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
/// 仅宽度变化的气泡才重算。
pub struct ChatScrollState {
  /// 视口顶部偏移（视觉行数）。
  pub offset: u16,
  /// 跟随底部：新消息到达时锁底；上滚置 false，触底恢复 true。
  pub follow_end: bool,
  /// 气泡折行行数缓存（id → (last_width, line_count)）。
  pub line_cache: HashMap<u64, (u16, usize)>,
}

impl Default for ChatScrollState {
  fn default() -> Self {
    Self {
      offset: 0,
      follow_end: true, // 默认粘底：新消息自动可见（上滚才解除）
      line_cache: HashMap::new(),
    }
  }
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
