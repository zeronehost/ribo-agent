# 附录：完整配置示例

以下是与 schema 逐字段对应的完整配置模板，与仓库根目录的
`ribo_example.toml` 同源（该文件始终是权威版本，
本页为其快照）。复制为 `ribo.toml` 并填入真实端点与密钥即可使用；
密钥一律走 `$ENV_NAME` 占位，不落盘。

各层相互独立、层间零交叉；示例中的值与代码 `Default` 一致。

```toml
# ── provider 层 ─────────────────────────────────────────────
# 根级字段：网关查询入口，必须是某个 provider 暴露的 model；
# 未配置时默认 "mock-model"。
query_model = "your-model-name"

# adapter 白名单：openai-chat | openai-responses | anthropic
# api_key 支持 "$ENV_NAME" 占位（运行时读环境变量，未设置则跳过该 provider）。
[[providers]]
provider_id = "chat-primary"
adapter = "openai-chat"
base_url = "https://your-endpoint.example.com"
api_key = "$OPENAI_API_KEY"
model = "your-model-name"

[[providers]]
provider_id = "anthropic-backup"
adapter = "anthropic"
base_url = "https://your-anthropic-endpoint.example.com"
api_key = "$ANTHROPIC_API_KEY"
model = "your-model-name"

# 跨提供方降级链：主 provider → 候选列表（须引用已注册的 provider_id）。
[routing.cross_provider]
"chat-primary" = ["anthropic-backup"]

# ── retry 层 ────────────────────────────────────────────────
# 未配置字段显式取 RetryPolicy::default()（max_backoff 30s、multiplier 2.0）。
[retry]
max_retries = 3                # 默认 3（不含首次请求）
initial_backoff_ms = 500       # 默认 500，首次重试基础退避
jitter_ratio = 0.25            # 默认 0.25，延迟 ±25% 随机抖动

# ── compression 层 ──────────────────────────────────────────
[compression]
enabled = false                # 默认 false：未显式开启绝不丢弃历史
trigger_ratio = 0.75           # 默认 0.75，估算 token 超过 context_window × 比例触发
keep_recent_turns = 4          # 默认 4，保留最近 4 个 user 轮
# summarizer_model = "..."     # Option 字段：注释掉即 None（默认），无摘要模型时
#                              # 压缩退化为纯滑窗；填写则用该模型生成摘要
max_summary_chars = 2048       # 默认 2048，摘要按字符截断

# ── session 层 ──────────────────────────────────────────────
[session]
default_key = "default"        # 默认 "default"，REPL 启动会话键（/new 轮换基准）
history_lines = 10             # 默认 10，/history 未带参数时的条数

# ── tools 层 ────────────────────────────────────────────────
# fs 沙箱 = harness.root（v3：无独立 fs_root 配置；项目外数据走挂载点）。
[tools]
builtin_enabled = true         # 默认 true，注册全部内置工具

# ── harness 层 ──────────────────────────────────────────────
[harness]
root = "."                     # 默认 "."（进程 cwd），内部产物集中 .ribo/：
                               # .ribo/skills/、.ribo/plugins/、.ribo/sessions/、
                               # .ribo/trusted_keys.json；可被 --root 覆盖
include_environment = true     # 默认 true，系统提示注入 <environment> 段

# ── global 层（全局域，跨项目共享；默认启用）─────────────────
# [global]
# enabled = true                 # 默认 true；目录不存在时自动禁用（非致命）
# dir = "~/.config/ribo"         # 可选；命令行 --global-dir 优先；
#                                # 未设置时用平台默认配置目录 + /ribo。
#                                # 全局配置 settings.toml、auth.toml 与信任记录
#                                # trusted_projects.toml 位于该目录。
#
# ── plugins 层（WASM 插件装配，默认关闭）─────────────────────
# enabled = true 时启动期加载插件：固定扫描 .ribo/plugins/ 下全部 *.wasm
#（缺失 = 软跳过；最小权限：无注入 config、ACL 全关）+ 逐条加载 entries
# 显式清单（可声明 ACL 与注入 config）。任一插件加载 / 注册失败 →
# 启动致命错误（禁止半装配）。
# [plugins]
# enabled = false
#
# [plugins.verify]               # 来源校验（默认 require_signature = false）
# require_signature = true       # 生产须开启：每条插件都要有有效 Ed25519 签名；
#                                # 公钥白名单固定为 .ribo/trusted_keys.json
#                                #（缺失 → plugin_trusted_keys_missing 启动致命）
#
# [[plugins.entries]]            # 显式清单：可逐插件声明 ACL 与 config
# path = "plugins/demo.wasm"     # 相对 harness.root；绝对路径 / ".." 逃逸 → 启动致命
# manifest = "manifest.json"     # 权威清单（guest 自述降级为交叉校验）
# provenance = "provenance.json" # 侧载签名块（require_signature = true 时必需）
#                                #   {"signer_id":"release","signature":"<128 hex>","wasm_digest":"<64 hex>"}
#
# [plugins.entries.acl]          # 能力由宿主注入，不信任 guest 自述（默认全关）
# log = true
# storage = true
# http = true
# http_hosts = ["api.example.com"]  # http 出站白名单（空 = 实际不可出站）
# memory_limit_bytes = 16777216     # 可选：线性内存覆写（只能收紧 32MB）
# storage_quota_bytes = 262144      # 可选：storage 配额覆写（只能收紧 1MB）
# timeout_ceiling_ms = 5000         # 可选：单工具超时上限覆写（只能收紧 60s）
#
# [plugins.entries.config]       # 注入插件的 config（args 信封，18_ §9）
# greeting_prefix = "你好"
#
# ── 只读挂载点（[fs.mounts]，仅全局 settings.toml）────────
# 模型默认只能读工作目录内文件。项目外共享数据在全局配置声明挂载点；
# 项目 ribo.toml 无权声明（写了会被 warning + 忽略）：
# [fs.mounts]
# datasets = "/mnt/company-data"   # 绝对路径、必须存在、不能在工作目录内
# read("datasets://a/b.jpg") 只读访问；write 返回 plugin.plugin_forbidden
# 保留字：ribo / resources / fs / http / https 不可用作挂载名
```

## 全局域目录布局

全局域（默认平台配置目录 + `/ribo`，`--global-dir` 可覆盖）下的文件：

```text
<global-dir>/ribo/
├── settings.toml          # 全局默认配置；[fs.mounts] 只读挂载点仅在此声明
├── auth.toml              # api_key 凭证（0600，优先级低于配置文件显式值）
└── trusted_projects.toml  # 项目代码信任记录（ribo trust add/remove 维护）
```

项目域（`harness.root`）下的内部产物集中在 `.ribo/`：

```text
<harness.root>/
└── .ribo/
    ├── skills/            # 项目技能（固定扫描根）
    ├── plugins/           # 插件 wasm（固定扫描目录；缺失 = 软跳过）
    ├── sessions/          # 会话存储
    ├── logs/              # 日志（全局域启用时日志写全局目录）
    └── trusted_keys.json  # 插件签名公钥白名单（require_signature=true 时必须存在）
```

## TOML 配置文件清单与维护方式

系统中全部 TOML 配置文件及其维护方式——**手动维护**的文件可直接编辑；
**命令维护**的文件建议走对应子命令（直接手改会保留注释，但不做校验）；
**自动生成**的产物由程序写入，无需也不要手动维护。

### ① `ribo.toml`（项目配置）— 手动维护

项目根目录；探测规则与逐层字段见上文完整示例与[配置详解](./configuration.md)。

### ② `<全局域>/settings.toml`（全局默认配置）— 手动维护

字段与 `ribo.toml` 完全一致（project 同名字段覆盖 global）；
**`[fs.mounts]` 仅在此文件声明有效**。

```toml
query_model = "your-model-name"

[[providers]]
provider_id = "chat-primary"
adapter = "openai-chat"
base_url = "https://your-endpoint.example.com"
api_key = "$OPENAI_API_KEY"      # 留空时从 auth.toml 补充（Global 来源）
model = "your-model-name"

[retry]
max_retries = 3

[fs.mounts]                      # 只读挂载点：项目配置无权声明
datasets = "/mnt/company-data"
```

### ③ `<全局域>/auth.toml`（凭证）— 命令维护

由 `ribo auth login <provider_id>` / `ribo auth logout <provider_id>` 维护；
文件不存在时首次使用自动创建（权限 0600）。仅对 **Global 来源且 api_key
为空** 的 provider 生效（G2：项目配置显式填写的 key 优先）。

```toml
[providers.chat-primary]
api_key = "sk-..."               # ribo auth login 写入
# oauth_access_token = "..."
# oauth_refresh_token = "..."
# expires_at = 2026-12-01T00:00:00Z
# scopes = ["read", "write"]
```

### ④ `<全局域>/trusted_projects.toml`（项目代码信任）— 命令维护

由 `ribo trust add/remove/list <project-path>` 维护；`trust remove`
只把 `code_trust` 置 `false`，条目保留。

```toml
version = 1

[projects."/home/user/proj-a"]
code_trust = true
trusted_at = 2026-11-17T10:00:00Z
trusted_by = "user"
note = "reviewed by reading plugins/*.wasm"
```

### ⑤ `<项目域>/.ribo/trusted_keys.json`（插件签名公钥白名单）— 手动维护

`[plugins.verify].require_signature = true` 时**必须存在且非空**，
缺失 → `plugin_trusted_keys_missing` 启动致命。内容为 JSON 格式
（provenance 通道 JSON-only，扩展名与内容一致）：

```json
{"keys":[{"id":"release","public_key":"<64 位 hex，Ed25519 公钥>"}]}
```

### 自动生成产物（勿手动维护）

| 产物 | 位置 | 说明 |
| :--- | :--- | :--- |
| 会话存储 | `<root>/.ribo/sessions/*.jsonl` | JSONL 会话流，程序自动读写 |
| 日志 | `<root>/.ribo/logs/cli.log`（或全局域 logs/） | 按日滚动，非阻塞写入线程持有 |

## 相关章节

- 配置文件选择与降级语义：[配置详解](./configuration.md)
- 信任模型：[两级信任域与两档信任](./trust.md)
- 插件与签名：[插件系统](./plugins.md)
