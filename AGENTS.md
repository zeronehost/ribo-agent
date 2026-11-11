# ribo-agent

Ribo Agent — 一个模块化的 Rust 工作空间，用于构建支持工具调用、多模型和事件驱动工作流的 AI 代理。

## 开发环境

- Rust 1.85.0+，edition 2024。
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

## 代理工作约定

- 完成 Rust 代码修改后，自动运行 `cargo fmt --all`，无需询问。
- 修复所有 Clippy 警告；不要无理由使用 `#[allow(...)]`。
- 不修改与任务无关的文件，不提交生成物，除非任务明确要求。
- 命令失败时先阅读错误并修复；不要通过放宽 lint 或删除测试来绕过。
- 新增公开 API 必须补充文档和示例。
- 涉及外部服务的测试应标记为 `#[ignore]`。
- **禁止在非测试代码中使用 `tempfile` crate**：仅允许出现在 `#[cfg(test)]` 模块与 `tests/` 集成测试中；`Cargo.toml` 只能在 `[dev-dependencies]` 声明（依赖层面的隔离即约束边界）。
- 如果没有理解用户需求，必须立即询问用户。

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