# 配置详解

## 配置层次

配置采用多层 schema，各层相互独立、层间零交叉；`[routing].default` 是网关查询入口的显式 `(provider, model)` 二元组。完整可复制模板见仓库根目录的 `ribo_example.toml`。

| 层 | 关键字段 | 默认值 |
| :--- | :--- | :--- |
| `provider` | `[[providers]]`（`provider_id`/`adapter`/`base_url`/`api_key`）、`[[providers.models]]`（`id`/`context_window`） | 无 provider；窗口默认 8192 |
| `routing` | `[routing].default`（`provider`/`model` 二元组）、`[[routing.fallback]]`（`match`/`targets`） | default = mock/mock-model、无规则 |
| `retry` | `max_retries`、`initial_backoff_ms`、`jitter_ratio` | 3 / 500ms / 0.25 |
| `compression` | `enabled`、`trigger_ratio`、`keep_recent_turns`、`summarizer_model`、`max_summary_chars`、`hysteresis_ratio`、`cooldown_turns`、`enable_incremental`、`incremental_rebuild_every`、`max_summary_input_tokens` | 关闭 / 0.75 / 4 / None / 2048 / 1.10 / 2 / true / 8 / 0（自动取模型窗口 70%） |
| `session` | `default_key`、`history_lines` | `"default"` / 10 |
| `tools` | `builtin_enabled` | true |
| `harness` | `root`、`include_environment` | `"."` / true |
| `global` | `enabled`、`dir` | true / 平台配置目录 + `/ribo` |

此外，`[plugins]`（插件加载条目）与 `[fs.mounts]`（只读挂载点）是两组独立配置，见[插件系统](./plugins.md)。

## 配置文件选择

- 显式 `--config <PATH>`：指向不存在的文件 → 启动报错 `config.config_file_missing`（v3.4）；
- 未指定时向上探测 `ribo.toml`（至 `.git` 边界），找到即加载；
- 否则全默认。

工作目录另有四档优先级（`--root` > `[harness].root` > 配置文件所在目录 > 当前目录），见 `harness.root` 说明。

## 降级语义

启动横幅会打印当前模式与全部降级原因：

- debug 构建：无 provider / 占位配置 / `default` 悬空 → 整体回退 Mock + warning；
- **release 构建（D7）**：上述三类一律启动致命（`*_in_release` 错误码），不再静默降级；
- 某 `$ENV` 密钥未设置 → 局部降级：该 provider 跳过注册，引用它的降级目标一并剔除；
- 单个配置层解析失败 → 该层保留默认并打印 warning。

## provider 与 routing 配置

一个 provider 是**通道声明**（base_url / api_key / adapter），可挂多个模型；
`context_window` 随模型走（状态行 ctx 占比与压缩预算共用，默认 8192）。

```toml
[routing]
default = { provider = "chat-primary", model = "your-model-name" }

[[providers]]
provider_id = "chat-primary"
adapter     = "openai-chat"          # openai-chat | openai-responses | anthropic
base_url    = "https://your-endpoint.example.com"
api_key     = "$OPENAI_API_KEY"      # 环境变量占位，不落盘

  [[providers.models]]
  id             = "your-model-name"
  context_window = 131072

  [[providers.models]]               # 同通道备选（models 声明序即隐式降级序）
  id             = "your-model-lite"

[[routing.fallback]]                 # 规则按声明顺序，首条 match 命中生效
match = { provider = "chat-primary" }
targets = [{ provider = "anthropic-backup" }]
```

`api_key` 支持 `$ENV_NAME` 占位；也接受直接写值（不推荐，会落盘）。adapter 类型固定为三种白名单值。

**降级顺序**：`default` 模型失败 → 同通道内其他模型（按 `models` 声明序，
即隐式链）→ 首条命中的 `[[routing.fallback]]` 规则的 `targets`（按序尝试）。
`match` 支持精确二元组 `{ provider, model }` 与 provider 级通配 `{ provider }`；
`target` 支持显式二元组 `{ provider, model }` 与 `{ provider }`（同名 → 目标默认）。
规则声明顺序即优先级——精确规则应写在通配规则之前（通配遮蔽会产生 warning）。
窗口不足的候选会被过滤（估算 token × 1.1 作为下限），防止降级到装不下当前对话的小模型。

## 全局域

```toml
[global]
enabled = true
# dir = "/home/you/.config/ribo"    # 可选；须绝对路径，代码不展开 ~
```

- `enabled = false` 时全局域禁用；
- 目录不存在时自动禁用（非致命，不会主动创建）；
- `dir` 省略时用平台默认配置目录（Linux `~/.config/ribo` 等）；
- `--global-dir` 命令行参数优先级最高。

全局域目录存放 `settings.toml`、`auth.toml`、`trusted_projects.toml` 等跨项目文件。

## 只读挂载点（项目外资源）

模型默认只能访问工作目录内的文件。若需读取**项目外**的共享数据（如 NAS 挂载），
在全局 `settings.toml` 声明只读挂载点：

```toml
[fs.mounts]
datasets = "/mnt/company-data"
refs     = "/opt/reference-docs"
```

模型侧用 `read("datasets://train/img001.jpg")` 访问——只读，写入会被拒绝。

- 挂载名须匹配 `^[a-z][a-z0-9_]{0,31}$`，且不能是保留字（`ribo` / `resources` / `fs` / `http` / `https`）；
- 路径必须绝对、存在、是目录，且**不能位于当前工作目录内**；
- **仅全局可声明**——项目配置无权声明（项目内路径本就可读，项目外路径的暴露决定权只属用户）；
- 任一校验失败即启动致命错误 `config.mount_invalid`。

详见[内置工具与文件系统沙箱](./tools.md)。

## 查看配置来源

使用一次性子命令查看每个字段来自哪一层：

```bash
ribo config --show-origin
ribo config --explain
```

来源标记同时用于技能清单的 `origin` 属性与 `/skills` 输出，三处保持一致。
