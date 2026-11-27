# 配置详解

## 配置层次

配置采用多层 schema，各层相互独立、层间零交叉。完整可复制模板见仓库根目录的 `ribo_example.toml`，各字段细节见下方逐层说明。

各层均可在**用户级** `settings.toml` 声明（加载顺序：全局 ← 项目 ← CLI，后者覆盖前者），用户级的凭证、代码信任与只读挂载点见[用户级配置（全局域）](./global.md)。本文档聚焦项目级 `ribo.toml`。

- **provider 层**：模型通道与多模型声明（`providers` / `providers.models`）
- **routing 层**：查询入口与降级（`routing.default` / `routing.fallback`）
- **retry 层**：请求重试
- **compression 层**：上下文压缩
- **session 层**：REPL 会话状态
- **tools 层**：内置工具开关
- **harness 层**：运行环境（工作空间根）
- **global 层**：全局域（跨项目目录）

此外，`[plugins]`（插件加载条目）见[插件系统](./plugins.md)；`[fs.mounts]`（只读挂载点）仅用户级可声明，见[用户级配置（全局域）](./global.md)。

## 配置文件选择

- 显式 `--config <PATH>`：指向不存在的文件 → 启动报错 `config.config_file_missing`；
- 未指定时向上探测 `ribo.toml`（至 `.git` 边界），找到即加载；
- 否则全默认。

## 降级语义

启动横幅会打印当前模式与全部降级原因：

- debug 构建：无 provider / 占位配置 / `default` 悬空 → 整体回退 Mock + warning；
- **release 构建**：上述三类一律启动致命（`*_in_release` 错误码），不再静默降级；
- 某 `$ENV` 密钥未设置 → 局部降级：该 provider 跳过注册，引用它的降级目标一并剔除；
- 单个配置层解析失败 → 该层保留默认并打印 warning。

## provider 层

一个 provider 是**通道声明**（adapter / base_url / api_key），可挂多个模型；`context_window` 随模型走，状态行 ctx 占比与压缩预算共用此值。

### providers

- 类型：`[[providers]]` 数组
- 默认值：无（debug 构建降级 Mock；release 构建报错 `config.no_providers_in_release`）

通道数组，每个条目声明一条通道。可在项目 `ribo.toml` 与用户级 `settings.toml` 两处声明：数组按 `provider_id` 跨域合并——同名冲突时**项目整条胜出**，全局只追加项目未占用的通道，详见[用户级配置](./global.md)。

### providers.provider_id

- 类型：`string`

通道唯一标识，`routing` 与降级规则通过它引用通道。

### providers.adapter

- 类型：`string`

适配器名，白名单固定三种：`openai-chat` / `openai-responses` / `anthropic`。

### providers.base_url

- 类型：`string`

API 端点。

### providers.api_key

- 类型：`string`

密钥支持 `$ENV_NAME` 占位（推荐，不落盘）；也接受直接写值（不推荐，会落盘）。对应环境变量未设置时该通道被跳过（见[降级语义](#降级语义)）。

### providers.models

- 类型：`[[providers.models]]` 数组
- 默认值：空

该通道的模型列表。**声明序即隐式降级序**：`default` 模型失败后按序尝试同通道其他模型。

### providers.models.id

- 类型：`string`

模型标识，`routing.default` 通过它选择模型。

### providers.models.context_window

- 类型：`u32`
- 默认值：`8192`

模型上下文窗口（token）。

## routing 层

### routing.default

- 类型：`{ provider, model }` 二元组表
- 默认值：`{ provider = "mock", model = "mock-model" }`

网关查询入口：每次请求先走这个 (provider, model)。

### routing.fallback

- 类型：`[[routing.fallback]]` 数组
- 默认值：无

降级规则数组。规则按声明顺序，首条 `match` 命中生效：

- `match`：精确二元组 `{ provider, model }` 或 provider 级通配 `{ provider }`；
- `targets`：候选目标数组，支持显式二元组 `{ provider, model }` 与 `{ provider }`（同名 → 目标默认）。

**降级顺序**：`default` 模型失败 → 同通道内其他模型（`models` 声明序，即隐式链）→ 首条命中规则的 `targets`（按序尝试）。规则声明顺序即优先级：精确规则应写在通配规则之前（通配遮蔽会产生 warning）。窗口不足的候选会被过滤（估算 token × 1.1 作为下限），防止降级到装不下当前对话的小模型。

provider 与 routing 的完整声明示例：

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

## retry 层

### retry.max_retries

- 类型：`u32`
- 默认值：`3`

单次请求的最大重试次数。

### retry.initial_backoff_ms

- 类型：`u64`
- 默认值：`500`

首次重试的退避基数（毫秒）。

### retry.jitter_ratio

- 类型：`f64`
- 默认值：`0.25`

退避随机抖动比例，避免重试请求同时扎堆。

## compression 层

### compression.enabled

- 类型：`bool`
- 默认值：`false`

压缩总开关：未显式开启绝不丢弃历史。

### compression.trigger_ratio

- 类型：`f64`
- 默认值：`0.75`

估算 token 超过 `context_window × trigger_ratio` 时触发压缩。

### compression.keep_recent_turns

- 类型：`usize`
- 默认值：`4`

压缩时保留最近的 user 轮数，不参与摘要。

### compression.summarizer_model

- 类型：`Option<String>`
- 默认值：`None`

摘要模型（如 `"gpt-4o-mini"`）。缺省时压缩退化为纯滑窗。

### compression.max_summary_chars

- 类型：`usize`
- 默认值：`2048`

摘要按字符截断的上限。

### compression.hysteresis_ratio

- 类型：`f64`
- 默认值：`1.10`（范围 1–10）

滞回比例：估算需达到上次触发值的该倍数才重新摘要，防止边界来回抖动。

### compression.cooldown_turns

- 类型：`u32`
- 默认值：`2`（上限 100）

触发后的冷却轮数：冷却期复用上次摘要、不调 LLM。

### compression.enable_incremental

- 类型：`bool`
- 默认值：`true`

增量压缩：只摘要上次检查点之后的新增内容。`false` = 每次全量摘要。

### compression.incremental_rebuild_every

- 类型：`u32`
- 默认值：`8`（范围 1–32）

每 N 次增量压缩强制一次全量重建（`1` = 每次全量）。

### compression.max_summary_input_tokens

- 类型：`usize`
- 默认值：`0`（自动取摘要模型窗口的 70%）

摘要输入的 token 上限。超限时丢弃最早的增量部分，保留摘要块与最近增量。

## session 层

### session.default_key

- 类型：`string`
- 默认值：`"default"`

REPL 启动时的会话键；`/sessions <名称>` 可切换或新建会话。

### session.history_lines

- 类型：`usize`
- 默认值：`10`

预留字段：`/history` 缺省条数当前固定为 10，暂不读取此配置（TUI 无 /history 命令，对话区即完整历史）。

### session.max_iterations

- 类型：`usize`
- 默认值：`150`

单回合最大迭代轮数（一轮 = 一次助手响应 + 其工具批）。超过该值返回 `agent.max_iterations`，已产出的助手消息与工具结果保留在转录中，可直接续跑而不必重做。读取十几个文件再总结的中等任务通常需要 20–30 轮，默认 150 留有充足余量；若模型陷入反复工具调用，可调低此值以尽早熔断。

## tools 层

### tools.builtin_enabled

- 类型：`bool`
- 默认值：`true`

是否注册内置工具（fs / calculator / echo / current_time 等），见[内置工具与文件系统沙箱](./tools.md)。

## harness 层

### harness.root

- 类型：`PathBuf`
- 默认值：`"."`（进程当前目录）

工作空间根：模型文件访问的沙箱根。优先级四档：`--root` > `[harness].root` > 配置文件所在目录 > 当前目录。

### harness.include_environment

- 类型：`bool`
- 默认值：`true`

是否在系统提示中注入环境信息。

## global 层

`[global]` 控制全局域本身（用户级配置的存放位置）。全局域目录内的
`settings.toml` / `auth.toml` / `trusted_projects.toml` / 只读挂载点，
见[用户级配置（全局域）](./global.md)。

### global.enabled

- 类型：`bool`
- 默认值：`true`

全局域开关。`false` 时禁用；目录不存在时自动禁用（非致命，不会主动创建）。

### global.dir

- 类型：`Option<PathBuf>`
- 默认值：平台默认配置目录（Linux `~/.config/ribo` 等）

全局域目录，须绝对路径（代码不展开 `~`）；`--global-dir` 命令行参数优先级最高。

## 只读挂载点（项目外资源）

只读挂载点（`[fs.mounts]`）**仅可在用户级 `settings.toml` 声明**：项目内路径本就可读，项目外路径的暴露决定权只属用户。声明格式、命名与路径校验规则见[用户级配置（全局域）](./global.md)。

## 查看配置来源

使用一次性子命令查看每个字段来自哪一层：

```bash
ribo config --show-origin
ribo config --explain
```

来源标记同时用于技能清单的 `origin` 属性与 `/skills` 输出，两处保持一致。
