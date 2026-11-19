# Ribo Agent

> 一个模块化的 Rust 工作空间，用于构建支持**工具调用**、**多模型**和**事件驱动工作流**的 AI 代理（agent）。

Ribo Agent 以 `ribo` / `ribo-tui` 两个二进制提供命令行与全屏终端两种交互界面，底层由一组职责单一的 crate 组成：模型提供方抽象、工具系统、事件驱动的 agent 循环、会话容器与配置装配。

- 许可证：[木兰宽松许可证，第 2 版（MulanPSL-2.0）](LICENSE)
- 语言要求：Rust **1.85.0+**，`edition = 2024`

---

## 特性

- **双交互界面**：`ribo` 是流式渲染的 REPL 命令行，`ribo-tui` 是五区全屏 TUI（输入、输出、状态行、工具状态、帮助叠加）。
- **多模型与多提供方**：内置 `openai-chat`、`openai-responses`、`anthropic` 三种适配器与 `mock` 模拟器，支持跨提供方降级链。
- **工具调用**：内置 9 个工具（echo、计算器、当前时间、文件系统六件套），文件系统工具带沙箱根限制。
- **网关韧性**：请求重试（指数退避 + 抖动）、首 token 超时、跨提供方降级、熔断与准入控制。
- **会话与上下文**：会话持久化、可配置的上下文压缩（摘要 / 滑窗）、系统提示与环境注入。
- **技能系统**：从扫描根加载 `SKILL.md` 技能，加载诊断不致命。
- **分层配置**：provider / retry / compression / session / tools / harness 六层相互独立，未配置时整体回退 Mock 模式；另有 Project / Global / Plugin 三层信任域与两档信任（文档默认信任、代码须显式授权）。

---

## 架构

| Crate | 职责 |
| :--- | :--- |
| `ra-core` | 统一错误类型与对话消息模型（用户 / 助手 / 工具 / 用量）。 |
| `ra-provider` | Provider 抽象、模型目录、网关（降级 / 熔断 / 准入）与 OpenAI 兼容适配器。 |
| `ra-tools` | `AgentTool` 声明、注册表、Schema 校验、熔断编排与内置工具。 |
| `ra-agent` | 事件驱动的 agent loop、会话、abort / queue。 |
| `ra-harness` | 会话容器：持久化、上下文压缩、系统提示与技能加载。 |
| `ra-cli` | 命令行交互层：REPL、流式渲染与分层配置（二进制 `ribo`）。 |
| `ra-tui` | 终端 TUI：五区全屏交互（二进制 `ribo-tui`）。 |

工作空间在 `Cargo.toml` 以 `resolver = "3"` 组织，`crates/*` 与 `internals/git-hook` 为成员。

---

## 环境要求

- Rust 工具链 **1.85.0** 或更高（`edition 2024`）。
- 应用形态为可执行程序，建议提交 `Cargo.lock`。

---

## 快速开始

```bash
# 1. 克隆
git clone https://gitee.com/zeronehost/ribo-agent.git
cd ribo-agent

# 2. 构建全部 crate（生成 ribo 与 ribo-tui 两个二进制）
cargo build --workspace

# 3. 直接以 Mock 模式运行（无需任何密钥）
cargo run -p ra-cli        # 命令行 REPL
cargo run -p ra-tui        # 全屏终端 TUI
```

> **Mock 模式**：未指定 `--config` 且当前目录不存在 `ribo.toml` 时，整个请求链路回退为本地模拟器（`mock-model`），可零配置体验交互与工具流程。

---

## 命令行参数

两个二进制共用同一套参数（`clap` 解析）：

| 参数 | 说明 |
| :--- | :--- |
| `-c, --config <PATH>` | 配置文件路径。未指定时自动探测当前目录的 `ribo.toml`，缺失则进入 Mock 模式。 |
| `--root <PATH>` | 覆盖配置中的工作空间根目录（`harness` 层 `root`，默认进程 cwd）。 |
| `--global-dir <PATH>` | 覆盖全局域目录（默认平台配置目录 + `/ribo`）。 |
| `--allow-duplicate-plugin-id` | 允许跨源重复的 plugin_id（首声明胜出）；默认拒绝并启动致命。 |

一次性子命令（`auth` / `trust` / `config` / `plugins list`）见下方「一次性子命令」小节。

---

## 交互命令

进入 REPL / TUI 后，以 `/` 开头的行被视为命令，其余整行作为用户消息发送给模型。

| 命令 | 别名 | 说明 |
| :--- | :--- | :--- |
| `/help` | `/?` | 显示命令清单。 |
| `/new [name]` | — | 轮换会话键；省略 `name` 自动轮换。 |
| `/stop` | `/cancel` | 取消在途回合。 |
| `/skills` | — | 列出已加载技能与诊断信息。 |
| `/sessions` | — | 列出会话键。 |
| `/status` | — | 状态摘要（会话 / 模式 / 在途）。 |
| `/history [n]` | — | 显示最近 `n` 条转录，默认 10。 |
| `/exit` | `/quit` | 退出。 |

---

## 配置

配置采用**六层 schema**，各层相互独立、层间零交叉；根级字段 `query_model` 为网关查询入口模型。

| 层 | 关键字段 | 默认值 |
| :--- | :--- | :--- |
| `provider` | `query_model`、`[[providers]]`（adapter / base_url / api_key / model）、`[routing.cross_provider]` | `query_model = "mock-model"`、无 provider |
| `retry` | `max_retries`、`initial_backoff_ms`、`jitter_ratio` | 3 / 500ms / 0.25 |
| `compression` | `enabled`、`trigger_ratio`、`keep_recent_turns`、`summarizer_model`、`max_summary_chars` | 关闭 / 0.75 / 4 / None / 2048 |
| `session` | `default_key`、`history_lines` | `"default"` / 10 |
| `tools` | `builtin_enabled`、`fs_root` | true / 沙箱 = harness.root |
| `harness` | `root`、`skills_roots`、`include_environment` | `"."` / `["skills"]` / true |

- **密钥不落盘**：`api_key` 一律写 `"$ENV_NAME"` 占位，运行时读取环境变量；未设置则该 provider 跳过注册。
- **降级语义**：
  - 模板保持占位状态（`your-*` / `example.com`）→ 整体回退 Mock；
  - 某 `$ENV` 密钥未设置 → 局部降级，引用它的路由候选一并剔除，若 `query_model` 因此悬空则整体回退 Mock；
  - 单个配置层解析失败（未知字段忽略除外）→ 该层保留默认并打印 warning。
- **配置文件选择**：显式 `--config` 优先；未指定时仅当 `ribo.toml` 存在才加载，否则走全默认 Mock。

完整可复制的模板见 [`ribo_example.toml`](ribo_example.toml)：复制为 `ribo.toml` 并填入真实端点与密钥即可。

```toml
# ribo.toml 示例（节选）
query_model = "your-model-name"

[[providers]]
provider_id  = "chat-primary"
adapter      = "openai-chat"
base_url     = "https://your-endpoint.example.com"
api_key      = "$OPENAI_API_KEY"
model        = "your-model-name"

[routing.cross_provider]
"chat-primary" = ["anthropic-backup"]
```

---

## 三层信任域与两档信任

Ribo 把配置与资源划分为三个**信任域**，并区分两档信任：

| 域 | 位置 | 内容 |
| :--- | :--- | :--- |
| **Project（项目域）** | `harness.root` | 项目自身的 `ribo.toml`、`skills/`、`AGENTS.md`、`.wasm` 插件 |
| **Global（全局域）** | 平台配置目录 + `/ribo`（可用 `--global-dir` 覆盖） | `settings.toml`、`auth.toml`、`trusted_projects.toml`、跨项目共享资源 |
| **Plugin Domain（插件域）** | `[[plugins.domains]]` 声明的绝对路径 | 企业共享的技能 / 提示词 / 数据 / 插件仓库 |

**两档信任**（设计上刻意区分）：

- **配置信任**（默认开启，无需记录）：`ribo.toml` / `AGENTS.md` / `skills/` 等**文档级**资源。加载它们只让模型看到更多上下文，不执行代码，默认信任是安全的。
- **代码信任**（默认关闭，须显式授予）：`plugins/` 里的 `.wasm` 等**可执行**资源。Project 插件首次遇到时不会被加载，并在启动横幅提示：

  ```text
  WARN project `/path/to/proj` has plugins but is not code-trusted.
       Run `ribo trust add /path/to/proj` after reviewing the plugins.
       Skills and AGENTS.md are still loaded.
  ```

  审阅插件后运行 `ribo trust add <项目路径>` 授予信任；`ribo trust list` 查看、`ribo trust remove <项目路径>` 撤销。信任记录保存在全局域的 `trusted_projects.toml`（保留手写注释）。TUI 启动时若检测到未信任项目含插件，会弹出 y/n 确认框。

### 插件域与外部资源

`[[plugins.domains]]` 声明外部资源根，与 `[plugins]`（插件加载条目）是两个不同概念：

```toml
[[plugins.domains]]
id = "dataset"              # 也是 fs 工具的前缀名，须跨域唯一
root = "/mnt/datasets"      # 必须绝对路径且存在
kind = "data"               # plugins | skills | prompts | data | all
read_only = true            # data 默认只读
```

fs 六件套通过 `scheme://相对路径` 访问域内资源，例如 `read("dataset://train/img001.jpg")`；无前缀路径仍走项目主沙箱根。`data` 类域默认只读，写入返回 `plugin.plugin_forbidden`。

`trusted = true` 声明**只在全局域生效**：项目自己在 `ribo.toml` 里声明 `trusted = true` 无效（防止项目自签信任），会被忽略并打印 warning。插件域的 `skills/` 与 `AGENTS.md` 按其 `kind` 决定是否纳入合并。

### 一次性子命令

在装配 REPL 之前执行、打印后退出；无子命令时行为完全不变（直接进入 REPL）：

```bash
ribo auth login <provider> [--api-key KEY]   # 设置凭证（KEY 省略时从 stdin 读）
ribo auth status                             # 列出已配置 provider（不显示密钥）
ribo auth logout <provider>                  # 删除凭证
ribo trust add <path> [--note NOTE]          # 授予项目代码信任
ribo trust remove <path>                     # 撤销代码信任
ribo trust list                              # 列出信任记录
ribo config --show-origin                    # 显示每个配置字段的来源域
ribo config --explain                        # 同上（含冲突解释）
ribo plugins list                            # 两栏列出插件条目与插件域
```

凭证与信任文件在首次使用时自动创建，无需手动迁移。

---

## Provider 适配器

| 适配器 | 协议形态 | 备注 |
| :--- | :--- | :--- |
| `openai-chat` | OpenAI Chat Completions | 经典 `/chat/completions` 流式 SSE。 |
| `openai-responses` | OpenAI Responses API | 新形态 responses 端点。 |
| `anthropic` | Anthropic Messages | 含 SSE 解析。 |
| `mock` | 本地模拟 | 默认无配置时启用，用于开发与演示。 |

网关层提供重试、首 token 超时、跨提供方降级、熔断与准入；模型目录（`ra-provider::model`）统一管理可路由模型。

---

## 内置工具

注册入口为 `ra_tools::builtin::register_builtin`，共 **9** 个：

| 工具 | 说明 | 沙箱 |
| :--- | :--- | :--- |
| `echo` | 回显输入。 | — |
| `calculator` | 表达式求值（`evalexpr`）。 | — |
| `current_time` | 当前时间（`jiff`）。 | — |
| `read` | 读取文件内容。 | 限 `fs_root` |
| `write` | 写入文件。 | 限 `fs_root` |
| `edit` | 就地编辑文件。 | 限 `fs_root` |
| `ls` | 列出目录。 | 限 `fs_root` |
| `glob` | 通配符匹配路径。 | 限 `fs_root` |
| `grep` | 内容检索。 | 限 `fs_root` |

文件系统工具（read / write / edit / ls / glob / grep）共享同一沙箱根 `fs_root`，所有路径在沙箱内解析，越界即拒绝。

---

## 技能系统

技能为带 frontmatter 的 `SKILL.md` 文件（手写解析，不依赖 `serde_yaml`）：

```markdown
---
name: example
description: 一个示例技能
---
技能正文……
```

- 从配置 `harness.skills_roots`（默认 `["skills"]`，相对 `root` 解析，可多个、按序加载）扫描。
- 加载诊断（缺 `SKILL.md`、读取失败、frontmatter 解析失败等）**不致命**：返回可用集合 + 诊断，调用方写日志后继续装配。
- 可用 `/skills` 命令在运行时查看已加载技能与诊断。

---

## 开发与质量门禁

开发约定见 [`AGENTS.md`](AGENTS.md)。提交前必须通过的门禁：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo check --workspace
cargo test --workspace
```

- Clippy 以 `-D warnings` 运行，警告视为错误。
- 涉及外部服务的测试标记为 `#[ignore]`；`real_api.rs` 需手动带密钥运行。
- 日志使用 `tracing`，且必须用完全限定路径（`tracing::info!(...)`）、**只用英文**、不记录敏感信息。
- 详细设计文档位于 `internals/docs`。

---

## 参考项目

Ribo Agent 并非从零设计，下列项目是架构与实现过程中的主要**蓝本**。

### 目标架构蓝本

- **`pi-rust`**（pi-ai / pi-agent / pi-harness / pi-tools / pi-cli / pi-tui / rpi-extensions 等）：**目标架构蓝本**。本工作空间的 crate 分层（core / provider / tools / agent / harness / cli / tui）与核心模式——事件驱动的 agent loop、会话容器、网关降级 / 熔断 / 准入——均照此移植。

> 使用原则：同一能力优先对齐 `pi-rust` 蓝本形态。

---

## 许可证

本项目基于 [木兰宽松许可证，第 2 版（MulanPSL-2.0）](LICENSE) 发布。
