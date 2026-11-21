# 命令行参数与子命令

`ribo` 与 `ribo-tui` 共用同一套参数解析规则。

## 全局参数

| 参数 | 说明 |
| :--- | :--- |
| `-c, --config <PATH>` | 配置文件路径。未指定时自动探测当前目录的 `ribo.toml`，缺失则进入 Mock 模式 |
| `--root <PATH>` | 覆盖配置中的工作空间根目录（`harness.root`，默认进程 cwd） |
| `--global-dir <PATH>` | 覆盖全局域目录（默认平台配置目录 + `/ribo`） |
| `--allow-duplicate-plugin-id` | 允许跨源重复的 plugin_id（首声明胜出）；默认拒绝并启动致命 |

不带任何子命令时，直接进入 REPL（行为与旧版本完全一致）。

## 一次性子命令

子命令在装配 REPL 之前执行，打印结果后退出。

### 凭证管理 `auth`

```bash
ribo auth login <provider> [--api-key KEY]   # 设置 API key；省略 --api-key 时交互提示输入
ribo auth status                             # 列出已配置 provider（不显示密钥）
ribo auth logout <provider>                  # 删除 provider
```

`login` 未传 `--api-key` 时会先打印提示，再从 TTY **无回显**读取密钥（输入内容不会显示在终端）：

```text
$ ribo auth login openai
请输入 provider `openai` 的 API key：
```

> 无回显输入要求 TTY；脚本 / CI 等非交互场景请用 `--api-key KEY` 传入。

> `auth` 必须带子命令；不带时 clap 打印帮助并以状态码 2 退出。查看状态请用 `ribo auth status`。

### 信任管理 `trust`

```bash
ribo trust add <path> [--note NOTE]    # 授予项目代码信任
ribo trust remove <path>               # 撤销代码信任（保留条目，置 false）
ribo trust list                        # 列出全部信任记录
```

信任记录保存在全局域目录的 `trusted_projects.toml`，使用 `toml_edit` 写入，**保留用户手写注释**。首次使用时自动创建文件。

### 配置诊断 `config`

```bash
ribo config --show-origin    # 显示每个配置字段的来源域
ribo config --explain        # 同上（含冲突解释）
```

输出示例：

```text
配置来源（未列出的字段为 default）：
  harness.root = cli
  retry.max_retries = global
```

来源域包括 `default` / `global` / `project` / `cli`。

### 插件概览 `plugins`

```bash
ribo plugins list
```

输出插件条目（Plugin Entries）与只读挂载点（Read-only mounts，若有）：

```text
Plugin Entries:
  - plugins/echo.wasm  [project]

Read-only mounts:
  - dataset         /mnt/datasets                     [project, data]
  - company         /opt/company/plugins              [global, plugins]
```

## 退出码

成功为 0；参数解析失败、装配失败或子命令执行失败为非 0（通常为 1 或 2），错误信息打印到 stderr。
