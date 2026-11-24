# 开发指南

## 构建与运行

```bash
cargo build --workspace        # 构建全部 crate
cargo run -p ra-cli            # 运行 REPL
cargo run -p ra-tui            # 运行 TUI
```

## 四道质量门禁

提交前依次执行：

```bash
cargo fmt --all -- --check                              # 格式
cargo clippy --workspace --all-targets -- -D warnings   # lint（零警告）
cargo check --workspace                                 # 类型检查
cargo test --workspace                                  # 全量测试
```

全部通过才算完成。测试覆盖单元测试、集成测试与各 crate 的文档测试（doctest）。

## 测试约定

- 错误码断言：`RiboError::code()` 返回**裸 code**（如 `plugin_file_missing`），不含域前缀（不是 `config.plugin_file_missing`）；
  注意部分 domain 的**常量本身**带前缀（如 `tool.not_found`、`plugin.plugin_forbidden`、`provider.rate_limited`），
  这类 code 断言时保留其常量前缀；文档与终端展示中看到的 `config.xxx` 是「domain + 裸 code」的
  展示写法（`Display` 格式为 `[domain] code: message`），与断言值不同；
- `CliSession` 未实现 `Debug`，断言错误时用 `match` 提取而非 `expect_err`；
- 给公共 struct 加字段后，立即全仓搜索所有字面量初始化并补齐（避免 E0063 留到全量测试才暴露）。

## 文档构建

本指南使用 mdbook：

```bash
cargo install mdbook            # 若尚未安装
mdbook build docs               # 生成 docs/book/
mdbook serve docs               # 本地预览（默认 http://localhost:3000）
```

文档源码在 `docs/src/`，目录结构由 `docs/src/SUMMARY.md` 定义；构建产物在 `docs/book/`（已 gitignore）。

## 网络环境提示

在有 HTTP 代理的环境中运行 provider 集成测试时，需设置：

```bash
export NO_PROXY=127.0.0.1,localhost
```

否则 transport 错误分类用例会被代理返回的 502 干扰而失败。
