# REPL 交互命令

启动 `ribo` 后进入 REPL。以 `/` 开头的行被解析为命令，其它行作为用户消息发送给模型。

## 命令清单

| 命令 | 说明 |
| :--- | :--- |
| `/help` | 显示命令清单 |
| `/new [name]` | 轮换会话键（`name` 缺省自动轮换） |
| `/skills` | 列出已加载技能及其来源 / 诊断 |
| `/sessions` | 列出全部会话键（内存 ∪ 磁盘） |
| `/status` | 状态行摘要（token 累计 / 在途 / 上下文占比） |
| `/history [n]` | 最近 n 条转录（默认取配置的 `history_lines`） |
| `/stop` | 取消在途回合 |
| `/cancel` | `/stop` 的同义命令 |
| `/exit` / `/quit` | 退出 |

> 注意：REPL 内的 `/status` 是**会话状态**命令，与一次性子命令 `ribo auth status`（查看凭证）无关。

## 技能来源显示

`/skills` 会显示每个技能的来源域，例如：

```text
skill: code-review（来自项目）
skill: company-guide（来自全局技能）
```

被覆盖的技能与加载诊断（解析失败、缺 SKILL.md、重名等）也会逐条显示。

## 会话与历史

- 会话按 key 持久化到 `harness.root/sessions/`（JSONL）；
- `/new` 轮换当前 key，实现"新会话"；
- `/history` 读取当前会话的转录；
- 上下文压缩（compression 层）在达到阈值时自动触发（摘要或滑窗）。

## 中断与退出

- 回合在途时按 Ctrl-C 取消当前回合（`/stop` 等价）；
- TUI 中连续两次 Ctrl-C 在超时窗口内会强制退出。
