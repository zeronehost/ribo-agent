# ribo-agent

Ribo Agent — 一个模块化的 Rust 工作空间，用于构建支持工具调用、多模型和事件驱动工作流的 AI 代理。

## 开发环境

- Rust 1.100.0（由 `rust-toolchain.toml` 固定），edition 2024。
- 应用项目应提交 `Cargo.lock`。

## 质量门禁

每次提交前必须运行以下命令：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo check --workspace
cargo test --workspace
```

完成 Rust 代码修改后，自动运行 `cargo fmt --all`，无需询问。提交前再运行 `cargo fmt --all -- --check`；若检查失败，应重新运行 `cargo fmt --all` 并修复。

Clippy 使用 `-D warnings` 运行，所有警告都被视为编译错误。推送前必须修复所有警告。除非有文档记录的原因，否则不要使用 `#[allow(...)]` 抑制警告。

建议将上述质量命令统一放入 CI、`justfile`、`Makefile` 或 `xtask`，本文档只引用唯一权威命令，避免多处复制后不一致。

### 命令速查表

| 操作         | 命令                                                      | 描述                           |
| :--------- | :------------------------------------------------------ | :--------------------------- |
| **检查**     | `cargo check --workspace`                               | 快速工作空间编译检查。                  |
| **测试**     | `cargo test --workspace`                                | 运行所有未忽略的测试。                  |
| **代码检查**   | `cargo clippy --workspace --all-targets -- -D warnings` | 零容忍的 Clippy 检查。              |
| **格式化**    | `cargo fmt --all`                                       | 强制执行 Rust Edition 2024 代码风格。 |
| **格式化检查** | `cargo fmt --all -- --check`                            | 检查代码是否符合格式化规范。               |

## 测试

```bash
# 测试单个 crate
cargo test -p ra-provider

# 文档测试
cargo test -p ra-provider --doc
```

- 需要外部服务的测试应标记为 `#[ignore]`。
- `pub` 标记的方法、函数等文档注释中需要示例代码；简单 getter、字段访问等可由作者判断是否省略。
- 优先比较整个对象的相等性，而不是逐个字段比较。

## 日志规范

- 日志使用 `tracing` crate。
- 不能使用 `use tracing::info;` 导入，必须使用完全限定路径调用，即 `tracing::info!("this info level log");`。
- 日志不能使用中文，必须使用英文。
- 关键逻辑位置必须添加日志，例如：状态变更、持久化、外部调用、错误分支、用户关键操作。
- 禁止记录敏感信息，包括但不限于账单金额、账号、姓名、地址、令牌等；必要时脱敏或只记录内部 ID。
- 推荐使用结构化字段，例如：

```rust
tracing::info!(provider = %provider, "provider init");
```

## 注释规范

### 文档注释

- 使用 `///` 进行项级注释（函数、结构体、枚举等）。
- 使用 `//!` 进行模块级注释。
- 推荐使用 Rustdoc 可识别的标准章节标题：`# Examples`、`# Panics`、`# Errors`、`# Safety`；参数和返回值可使用 `# Arguments`、`# Returns` 或中文说明。
- 示例代码默认必须可运行且编译正确。
- 需要外部服务但可以编译的示例应使用 `no_run`：

```rust
/// # Examples
///
/// ```no_run
/// // 此示例需要外部服务，因此只编译不运行
/// let client = ExternalClient::new("http://example.com");
/// client.fetch_data().await?;
/// \`\`\`
```

- 无法编译的伪代码可以使用 `ignore` 标记，但必须包含必要的注释说明原因。
- 测试代码必须包含必要的注释说明，说明测试的场景和预期结果。
- 所有 `pub`、`pub(crate)`、`pub(super)` 等标记的项都必须包含文档注释。
- 宏定义本身可以使用 `///` 文档注释；宏展开生成的代码无法单独注释时，应在宏定义处使用普通注释说明。
- 必须使用中文

### 内联注释与长函数

- 为超过 50 行的函数/方法中的重要逻辑位置添加必要的内联注释。
- 采用阶段划分方式注释复杂方法，提高代码可读性，例如 `// ===== 初始化阶段 =====`。
- 对于算法性代码（如指数退避、JSON 解析），添加算法步骤说明。
- 对于并发代码（如双检查锁定模式），添加竞态条件说明。
- 对于安全相关代码，添加检查流程说明。
- 必须使用中文

# 文档

- 所有文档必须使用中文，除非特殊说明，如 `README.en.md` 使用英文
- 必须符合 markdown 规范

## 变更日志（CHANGELOG.md）

- 项目根目录维护 `CHANGELOG.md`，记录所有值得注意的变更；文件名与位置固定，不另建同类文件。
- 格式遵循 [Keep a Changelog 1.1.0](https://keepachangelog.com/zh-CN/1.1.0/)，版本号遵循[语义化版本](https://semver.org/lang/zh-CN/)。
- 版本划分以仓库 tag 为准：已打 tag 的改动归入对应版本；未打 tag 的改动归入当前版本——若 `Cargo.toml` 的 `version` 字段已更新，则以该字段为准，否则在最后一个 tag 的版本号上自增补丁号。
- 每个版本按「新增 / 变更 / 修复 / 移除」四类组织；某类无内容时可省略。
- 条目使用中文，描述变更的**影响与原因**，不写提交流水账。
- **只记录被 git 跟踪的文件变更**：不得出现 `.gitignore` 忽略的文件或目录的改动，也不得以路径形式引用它们（当前忽略项包括 `todo/`、`.ribo/`、`.workbuddy/`、`tmp/`、`tmps/`、`docs/book/`、`target/`、`debug/`、`internals/docs/reports/`、`ribo*.toml` 等；以实际 `.gitignore` 为准）。被忽略目录中的设计过程文档不进入 CHANGELOG，仅保留其对已跟踪代码变更的影响描述。
- 完成用户交付的改动后，按上述规则同步更新当前版本段落；打 tag 发布时只补上该版本的发布日期，不再新建「未发布」段落。
- 破坏性变更与安全修复必须显式标注，不得混入普通条目。

## 代理工作约定

- 完成 Rust 代码修改后，自动运行 `cargo fmt --all`，无需询问。
- 修复所有 Clippy 警告；不要无理由使用 `#[allow(...)]`。
- 不修改与任务无关的文件，不提交生成物，除非任务明确要求。
- 命令失败时先阅读错误并修复；不要通过放宽 lint 或删除测试来绕过。
- 新增公开 API 必须补充文档和示例。
- 涉及外部服务的测试应标记为 `#[ignore]`。
- **禁止在非测试代码中使用 `tempfile` crate**：仅允许出现在 `#[cfg(test)]` 模块与 `tests/` 集成测试中；`Cargo.toml` 只能在 `[dev-dependencies]` 声明（依赖层面的隔离即约束边界）。
- 如果没有理解用户需求，必须立即询问用户。
- 必须使用中文思考或回答问题，不能使用英文。

## 完成定义

提交前以下命令必须全部通过：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo check --workspace
cargo test --workspace
```

同时满足：

- 文档注释符合本规范。
- 关键逻辑有英文日志。
- 日志中没有敏感信息。
- 需要外部服务的测试已标记 `#[ignore]`。

**禁止提交代码**