# 两级信任域与两档信任

## 两个信任域

| 域 | 位置 | 内容 |
| :--- | :--- | :--- |
| **Project（项目域）** | `harness.root`（内部产物在 `.ribo/`） | 项目的 `ribo.toml`、`AGENTS.md`、`.ribo/skills/`、`.ribo/plugins/` wasm |
| **Global（全局域）** | 平台配置目录 + `/ribo`（`--global-dir` 可覆盖） | `settings.toml`、`auth.toml`、`trusted_projects.toml`、跨项目共享资源 |

每个域内路径都经过统一的词法与物理归属校验（拒绝绝对路径、`..` 逃逸组件、符号链接逃逸）。域根本身是"域选择器"，允许绝对路径，由独立逻辑校验存在性与目录类型。

## 两档信任

设计上刻意区分两类资源：

- **配置信任**（默认开启，无需记录）：`ribo.toml` / `AGENTS.md` / `skills/` 等**文档级**资源。加载它们只让模型看到更多上下文，不执行代码，默认信任是安全的。
- **代码信任**（默认关闭，须显式授予）：`plugins/` 里的 `.wasm` 等**可执行**资源。

这一区分的原因：clone 一个仓库后，`AGENTS.md` 是文档，默认加载无风险；但 `plugins/` 里的 `.wasm` 会执行代码，必须用户确认后才能加载。

## 代码信任的判定

```text
Project 插件可加载  = code_trust(project) == true
                     （全局 settings.toml 声明的条目走同一项目闸门）
签名校验            = 既有 Ed25519 链（production 策略下 require_signature）
```

关键安全点：项目 `.wasm` 必须用户在 `trusted_projects.toml [projects]` 显式授予（或 TUI gate 确认）——项目不能自授代码信任。v3 已移除插件域信任（`[plugin_domains]`），代码信任只看项目记录。

## 未信任时的行为

启动时不加载未信任项目的插件，并在横幅打印提示：

```text
WARN project `/path/to/proj` has plugins but is not code-trusted.
     Run `ribo trust add /path/to/proj` after reviewing the plugins.
     Skills and AGENTS.md are still loaded.
```

注意：**技能与 AGENTS.md 仍照常加载**——它们属文档级资源，不受代码信任闸门影响。这是域故障隔离（P3）的一部分。

## 授予与撤销信任

```bash
ribo trust add /path/to/proj --note "reviewed plugins"
ribo trust list
ribo trust remove /path/to/proj
```

信任记录位于全局域的 `trusted_projects.toml`：

```toml
version = 1

[projects."/home/user/proj-a"]
code_trust = true
trusted_at = 2026-11-17T10:00:00Z
trusted_by = "user"
note = "reviewed by reading plugins/*.wasm"
```

文件由 `toml_edit` 改写，保留用户手写注释。`trust remove` 只是把 `code_trust` 置为 `false`，条目本身保留。

## TUI 信任确认

TUI 启动时若检测到项目声明了插件但未授信，会在进入主循环前弹出确认：

- 按 `y`：写入信任记录，重新加载配置，插件正常加载；
- 按 `n` / Enter：跳过插件，继续启动（与 CLI 的 banner 提示一致）。

CLI 无交互，统一走 banner + `ribo trust add` 提示。
