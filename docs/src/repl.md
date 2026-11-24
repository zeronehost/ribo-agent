# REPL 交互命令

启动 `ribo` 后进入 REPL。以 `/` 开头的行被解析为命令，其它行作为用户消息发送给模型。

## 命令清单

| 命令 | 适用端 | 说明 |
| :--- | :--- | :--- |
| `/help`（`/?`） | CLI + TUI | 显示命令清单 |
| `/skills` | CLI + TUI | 列出已加载技能及其来源 / 诊断 |
| `/sessions [name]` | CLI + TUI | 无参列出全部会话键（内存 ∪ 磁盘；TUI 浮层以 → 标记当前，CLI 逗号分隔列出）；带参切换到指定会话，不存在则新建 |
| `/stop`（`/cancel`） | CLI + TUI | 取消在途回合 |
| `/status` | **仅 CLI** | 状态摘要（会话 / 模式 / 在途 / 警告数 / 上下文三态：总量 / 已压缩 / 当前）；TUI 侧栏常驻展示，无需命令。输出示例：`ctx: total=… \| compressed=…% \| current=… (…)`；未装配时为 `ctx: —` |
| `/history [n]` | **仅 CLI** | 最近 n 条历史记录（默认固定 10，不读取配置）；TUI 对话区即完整历史，PgUp/PgDn 翻页 |
| `/exit`（`/quit`） | CLI + TUI | 退出 |

> 注意：REPL 内的 `/status` 是**会话状态**命令，与一次性子命令 `ribo auth status`（查看凭证）无关。

TUI 输入 CLI-only 命令时会显示指引浮层（例如「对话区即完整历史，PgUp/PgDn 翻页查看」），不会静默忽略。

## 技能来源显示

`/skills` 会显示每个技能的来源域，例如：

```text
skill: code-review（来自项目）
skill: company-guide（来自全局技能）
```

被覆盖的技能与加载诊断（解析失败、缺 SKILL.md、重名等）也会逐条显示。

## 会话与历史

- 会话按 key 持久化到 `<harness.root>/.ribo/sessions/`（JSONL）；
- `/sessions <名称>` 切换到指定会话（旧 `/new` 命令已合并至此），不存在则新建空白会话；
- `/history [n]` 读取当前会话的历史记录（仅 CLI，默认固定 10 条）；
- 上下文压缩（compression 层）在达到阈值时自动触发（摘要或滑窗）。

## 中断与退出

- 回合在途时按 Ctrl-C 取消当前回合（`/stop` 等价）；
- TUI 中连续两次 Ctrl-C 在超时窗口内会强制退出。
