# 快速开始

## 环境要求

- Rust 工具链（stable，edition 2024）
- Windows / macOS / Linux 均可

## 构建

在仓库根目录执行：

```bash
cargo build --workspace
```

构建产物为 `target/debug/ribo` 与 `target/debug/ribo-tui`。

## 零配置启动（Mock 模式）

```bash
cargo run -p ra-cli
```

未指定 `--config` 且当前目录不存在 `ribo.toml` 时，整个请求链路回退为本地模拟器（`mock-model`），无需任何密钥即可体验交互、工具调用与会话流程。启动横幅会显式打印当前模式（real / mock）与降级原因。此降级仅限 debug 构建；release 构建下无 provider 会直接报错退出（`config.no_providers_in_release`）。

TUI 版本：

```bash
cargo run -p ra-tui
```

## 接入真实模型

1. 复制配置模板：`cp ribo_example.toml ribo.toml`
2. 填写 `[routing].default` 二元组、`[[providers]]` 的 `base_url` 与 `[[providers.models]]` 的模型 `id`，并把 `api_key` 写成环境变量占位：

   ```toml
   api_key = "$OPENAI_API_KEY"
   ```

3. 设置环境变量后启动：

   ```bash
   export OPENAI_API_KEY="sk-..."
   cargo run -p ra-cli
   ```

密钥一律以 `$ENV_NAME` 占位，**不落盘**。若某 provider 的密钥环境变量未设置，该 provider 被跳过；若因此导致 `default` 二元组悬空，debug 构建下整体回退 Mock 模式，release 构建下报错退出（`config.no_providers_in_release`）。

> 说明：`ribo auth login` 管理的凭证存在于全局域的 `auth.toml`，用于凭证的查看与增删；provider 实际读取的密钥仍来自 `ribo.toml` 中的 `$ENV` 占位。

## 第一次进入一个含插件的项目

Project 域的 `.wasm` 插件默认**不加载**（代码信任默认关闭）。启动时会看到提示：

```text
WARN project `/path/to/proj` has plugins but is not code-trusted.
     Run `ribo trust add /path/to/proj` after reviewing the plugins.
     Skills and AGENTS.md are still loaded.
```

审阅插件后执行：

```bash
ribo trust add /path/to/proj
```

再次启动即可加载插件。TUI 会在启动时弹出 y/n 确认框，按 `y` 等效于执行 `trust add`，按 `n` 则跳过插件继续启动。

详见[两级信任域与两档信任](./trust.md)。
