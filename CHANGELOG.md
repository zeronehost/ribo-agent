# 变更日志

> 本项目所有值得注意的变更都会记录在本文件中。
> 格式遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，版本号遵循[语义化版本](https://semver.org/lang/zh-CN/)。


## [v0.0.4] — 2026-11-27

### 新增（Added）

- **[session] `max_iterations` 配置项**：迭代轮次上限从硬编码的 25 改为可配置，默认 150。`BaseAgent` 新增 `with_max_iterations()` 构造器，CLI 装配层透传配置值；配置文档同步补全。
- **`agent_mock_loop` 示例**（`crates/agent/examples/agent_mock_loop.rs`）：演示 MockProvider + 内置工具 + 多轮工具调用 + steering 注入 + abort 三个场景，零密钥、零新增依赖（补齐 M1 里程碑最后一个验收缺口）。
- **失败路径产出落盘**：`Harness::prompt_messages` / `continue_run` 在错误路径通过快照差量补写会话转录，避免轮次耗尽等失败时整轮产出丢失；落盘失败只告警不覆盖原业务错误。
- **每轮轮次日志**：`agent loop turn start session_key=… turn=N turns_limit=M`，失败日志补 `error = %err`；解决「轮次耗尽时日志看不到轮次推进」的可观测性缺口。
- **上下文压缩测试补全**：新增超时回退滑窗、取消令牌提前回退、空摘要流报告、冷却期结束后重新摘要共 4 个测试（`crates/harness/src/compaction.rs`）。
- **插件 start 段 trap 专测**：新增 `unreachable` trap 与死循环两类 guest，验证 start 段 trap 映射为执行失败、死循环被 epoch 中断（`crates/plugins/src/wasm/tests.rs`）。
- **失败产出保留与续跑集成测试**：验证超限产出落盘、重启后保留、续跑成功（`crates/harness/tests/integration.rs`）。

### 变更（Changed）

- **轮次超限补发 `AgentEnd`**：`max_iterations` 超限返回前补发终止事件，修复 TUI 侧 turn 卡死。
- **工具链版本口径统一**：`README.md` / `README.en.md` / `docs/src/quickstart.md` 中过时的 1.85.0 / stable 统一为 `rust-toolchain.toml` 锁定的 1.100.0；全仓无旧版本残留。
- **英文 README 配置层数修正**：`six independent layers` 改为 `eight`，与中文 README 及 `docs/src/configuration.md` 的 8 层（provider / routing / retry / compression / session / tools / harness / global）一致。
- **CI 工作流调整**：添加锁文件漂移检查、优化缓存策略、删除 Gitee 同步工作流（见 `b9486d9` / `461b5d6`）。
- **会话长度读取去掉全量拷贝**：`Session` 新增 `message_count()`（只读锁取长度、不克隆消息体），`Harness` 错误路径差量起点改用它，避免超长会话每回合两次深拷贝整个转录。

### 修复（Fixed）

- **上下文 token 估算低估**：`estimate_context_tokens` 此前忽略 toolCall 的参数内容，导致压缩永不触发、真实请求越限（HTTP 400 / code 1261）。现按字符权重计入 `call.name` / `call.id` / `call.arguments`，图片按 0 处理。三处共用估算口径（压缩触发 / 降级窗口 / TUI 占比）自动受益，会话估算占比由 66% 修正为 81%。
- **轮次耗尽产出丢失**：失败路径差量补写落盘，重启后可恢复（原实现用 `?` 提前返回，直接跳过落盘）。

### 移除（Removed）

- **TUI 未使用依赖**：移除 `once_cell` / `tracing-appender` / `ra-harness` 声明，同步清理 workspace 根中孤立的 `once_cell`。

## [0.0.3] — 2026-11-26

### 新增（Added）

- **用户可见错误文案映射层**（`crates/core/src/error/user_facing.rs`）：provider / config / agent / tool / plugin / io 各域错误统一映射为中文结论 + 可操作建议，原始错误只进日志；CLI 与 TUI 启动、装配及流内错误路径均已接入（TUI 实测渲染红色 `error ›` 气泡）。
- **文档构建部署工作流**：mdBook 文档自动构建并部署至 Pages。
- **Gitee-GitHub 双向同步工作流**（该工作流在后续未发布版本中移除）。

### 变更（Changed）

- 文档补充 TUI 界面截图、预编译下载说明与配置示例格式调整。
- 过时的「配置探测缺失时回退 Mock 模式」说明从文档中移除，与 release 构建行为对齐。

## [0.0.2] — 2026-11-25

### 变更（Changed）

- **Rust 工具链升级至 1.100.0**：新增 `rust-toolchain.toml` 锁定版本，CI 各工作流同步对齐。
- **依赖版本锁定**：更新依赖版本并统一注释与文档中的冗余标记。

## [0.0.1] — 2026-11-24

首个发布版本，完成 v3 架构核心链路。

### 新增（Added）

- **`ra-core`**：通用错误处理与对话消息模型（支持文本 / 工具调用 / 图片内容块）。
- **`ra-provider`**：多模型提供方接入基础设施——OpenAI 兼容、Anthropic、OpenAI Responses 适配器，通用 SSE 解析骨架，脚本化 Mock 适配器；网关含并发准入、熔断、重试与降级路由。
- **`ra-tools`**：工具调用框架与内置工具（calculator / echo / current_time / glob / grep / ls / read / write / edit）。
- **`ra-agent`**：核心 Agent 运行时——主循环、会话管理、事件系统、steering / follow-up 队列、取消树。
- **`ra-harness`**：会话容器与门面层（JSONL 持久化、上下文压缩、技能加载、系统提示组装）。
- **`ra-cli` / `ra-tui`**：命令行交互层与终端界面；修复 agent 取消根永久闩锁缺陷；配置路径解析 API 与 `/?` 命令别名。
- **`ra-plugins` / PDK**：插件系统完整实现，插件来源校验与权威清单信任链，Rust PDK 与宏；三层信任域模型与代码信任闸门。
- **上下文压缩与降级路由**：摘要生成、cooldown 抑制、失败回退滑窗。
- **发布工作流**：多平台预编译包与 release 体积优化。
