# 用户级配置（全局域）

项目外的配置与资源集中在**全局域**目录：跨项目的凭证、代码信任记录、只读挂载点，以及可在用户级声明的全部配置层。

## 目录与文件布局

默认位置为平台配置目录 + `/ribo`（Linux `~/.config/ribo` 等），可用 `[global].dir` 或 `--global-dir` 覆盖：

```text
<global-dir>/
├── settings.toml            # 用户级配置层（schema 与 ribo.toml 相同）
├── auth.toml                # 凭证（ribo auth login 写入，权限 0600）
└── trusted_projects.toml    # 代码信任记录（见两级信任）
```

全局域由 `[global]` 层控制（`enabled` / `dir`，见[配置详解](./configuration.md)）：
`enabled = false` 时禁用；目录不存在时自动禁用（非致命，不会主动创建）。

## settings.toml：用户级配置层

`settings.toml` 与项目 `ribo.toml` 使用**同一套层 schema**（provider / routing /
retry / compression / session / tools / harness / global），加载与覆盖顺序：

**全局 `settings.toml` ← 项目 `ribo.toml` ← CLI 参数**（后者覆盖前者）

单层解析失败 → warning + 该层保留默认（局部降级），不影响其他层。

### providers 的跨域合并

`[[providers]]` 是数组，不走整体覆盖，而是按 `provider_id` 合并：

- project 通道在前，按声明序全部放入；
- global 通道在后，只追加 project 未占用的 `provider_id`；
- 同名冲突 → **project 整条胜出**（不继承全局字段），并产生诊断提示。

典型用法：在 `settings.toml` 声明个人常用通道（个人密钥随用户走），项目
`ribo.toml` 声明项目专用通道；同名时项目配置赢。

## auth.toml：凭证

- 由 `ribo auth login` / `ribo auth status` 管理（见[命令行参数与子命令](./cli.md)），
  写入全局域 `auth.toml`，权限强制 `0600`（非 0600 会 warning 但继续读取）；
- 日志不打印完整路径与密钥内容；
- 为 `settings.toml`（用户级）声明的通道提供密钥补充；项目 `ribo.toml` 通道的
  密钥不读取 `auth.toml`——项目来源的 `api_key` 用 `$ENV` 占位或直接写值。

## trusted_projects.toml：代码信任

代码信任（`.wasm` 插件执行授权）的记录文件，由 TUI 信任闸门写入。
判定语义、授予与撤销方式见[两级信任域与两档信任](./trust.md)。

## fs.mounts：只读挂载点（仅用户级可声明）

模型默认只能访问工作目录内的文件。若需读取**项目外**的共享数据（如 NAS 挂载），
在全局 `settings.toml` 声明只读挂载点：

```toml
[fs.mounts]
datasets = "/mnt/company-data"
refs     = "/opt/reference-docs"
```

模型侧用 `read("datasets://train/img001.jpg")` 访问：只读，写入会被拒绝。

- 挂载名须匹配 `^[a-z][a-z0-9_]{0,31}$`，且不能是保留字（`ribo` / `resources` / `fs` / `http` / `https`）；
- 路径必须绝对、存在、是目录，且**不能位于当前工作目录内**；
- **仅全局可声明**：项目配置无权声明（项目内路径本就可读，项目外路径的暴露决定权只属用户；项目 `ribo.toml` 中出现 `[fs.*]` 会被忽略并产生 warning）；
- 任一校验失败即启动致命错误 `config.mount_invalid`。

详见[内置工具与文件系统沙箱](./tools.md)。
