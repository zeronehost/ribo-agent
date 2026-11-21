# 插件开发指南

Ribo 插件是编译到 `wasm32-unknown-unknown` 的 WASM 模块，通过 guest 侧 SDK `ra-plug-pdk`（PDK）暴露工具。宿主以 ABI v0 与其交互，所有能力由宿主通过 ACL 注入，**不信任 guest 自述**。

## 开发环境准备

安装 wasm 编译目标：

```bash
rustup target add wasm32-unknown-unknown
```

插件 crate 的关键配置（`Cargo.toml`）：

```toml
[lib]
crate-type = ["cdylib"]        # 必须：cdylib 才产出 .wasm（rlib 不会）

[dependencies]
serde_json = { workspace = true }

[target.'cfg(target_arch = "wasm32")'.dependencies]
ra-plug-pdk = { workspace = true }
```

> guest 代码应整体用 `#[cfg(target_arch = "wasm32")]` 门控——PDK 的运行时与 IO 封装只在 wasm 目标可用；纯逻辑（信封解析、manifest 构建）在宿主目标也可用，便于单测。

## 第一个插件

完整示例（可对照 `examples/plugin-demo/src/lib.rs`）：

```rust
use ra_plug_pdk::{Value, json, ribo, ribo_plugin, ribo_tool};

#[ribo_plugin(id = "demo")]
mod plugin {
    use super::*;

    /// echo：纯 CPU 工具，原样返回参数。
    #[ribo_tool(
        name = "echo",
        description = "Echoes the arguments back",
        parameters = r#"{ "type": "object", "properties": {} }"#,
        idempotent = true
    )]
    pub fn echo(args: Value) -> Result<Value, String> {
        ribo::log("echo called");
        Ok(args)
    }

    /// greet：读取宿主注入的 config，并用 storage 计数。
    #[ribo_tool(
        name = "greet",
        description = "Greets a name with a configurable prefix",
        parameters = r#"{
            "type": "object",
            "properties": { "name": { "type": "string" } },
            "required": ["name"]
        }"#,
        idempotent = true
    )]
    pub fn greet(args: Value) -> Result<Value, String> {
        let prefix = ribo::config()
            .as_ref()
            .and_then(|c| c["greeting_prefix"].as_str())
            .unwrap_or("Hello");
        let name = args["name"].as_str().ok_or("missing name")?;

        let key = format!("count:{name}");
        let seen: u64 = ribo::storage::get(&key)?
            .map(|b| String::from_utf8_lossy(&b).parse().unwrap_or(0))
            .unwrap_or(0);
        ribo::storage::put(&key, (seen + 1).to_string().as_bytes())?;

        Ok(json!({ "message": format!("{prefix}, {name}! seen={seen}") }))
    }
}
```

### 两个宏

| 宏 | 作用 |
| :--- | :--- |
| `#[ribo_plugin(id = "...")]` | 标记插件模块，`id` 为插件唯一标识；生成 `__RIBO_TOOLS` 与 ABI v0 导出（`alloc` / `dealloc` / `list_tools` / `execute`） |
| `#[ribo_tool(...)]` | 标记工具函数，声明 `name` / `description` / `parameters`（JSON Schema 字符串）/ `idempotent`，可选 `timeout_hint_ms` / `memory_limit_bytes` |

工具函数签名为 `fn(Value) -> Result<Value, String>`：参数为业务参数对象，返回值成为工具输出；`Err(String)` 会生成 `{"ok":false,"error":...}` 错误信封回灌宿主（错误文本 4KB 截断）。

> 工具短名在宿主侧自动加 `{plugin_id}__` 前缀暴露给模型，例如 `demo__echo`。

## 宿主侧 API

PDK 通过 `ribo::` 命名空间提供 host function 封装（仅 wasm 目标可用）：

| API | 用途 | 需要的 ACL 能力 |
| :--- | :--- | :--- |
| `ribo::config()` | 读取宿主注入的插件配置 | 无 |
| `ribo::storage::get(key)` / `put(key, value)` | 插件级键值存储（进程内，不持久化） | `storage` |
| `ribo::http_get(url)` | 经宿主代理发起 GET | `http` |
| `ribo::log(msg)` | 写宿主日志 | `log` |

### 配置注入（args 信封）

宿主在声明 `config` 时，以 `{"config": cfg, "args": args}` 信封传给 guest。PDK 透明解包：工具函数收到的 `args` 仍是业务参数，配置通过 `ribo::config()` 读取。未配置时返回 `None`。

宿主侧配置示例：

```toml
[[plugins.entries]]
path = "plugins/demo.wasm"

[plugins.entries.config]
greeting_prefix = "你好"
```

### 长度上限

PDK 在发起请求**前**检查以下上限（宿主侧另有兜底拒绝）：

| 项 | 上限 |
| :--- | :--- |
| storage key | 256 字节 |
| storage value | 256 KB |
| http 请求 JSON | 64 KB |
| 日志文本 | 16 KB |
| 错误文本 | 4 KB（截断） |

## 构建

```bash
cargo build --manifest-path examples/plugin-demo/Cargo.toml \
  --target wasm32-unknown-unknown
```

产物位于 `target/wasm32-unknown-unknown/debug/plugin_demo.wasm`。发布构建加 `--release` 可减小体积。

## 宿主装配与能力授予

把 wasm 放到项目内（如 `plugins/`），在 `ribo.toml` 声明：

```toml
[plugins]
enabled = true

[[plugins.entries]]
path = "plugins/demo.wasm"

[plugins.entries.acl]
log = true
storage = true
http = true
http_hosts = ["api.example.com"]   # http 出站白名单（大小写不敏感）
# memory_limit_bytes = 16777216    # 可选：只能收紧 32MB 默认上限
# storage_quota_bytes = 262144     # 可选：只能收紧 1MB 默认配额
# timeout_ceiling_ms = 5000        # 可选：只能收紧 60s 上限

[plugins.entries.config]
greeting_prefix = "你好"
```

要点：

- ACL 默认全关——guest 调用了未授予的 host function 会被宿主拒绝；
- `http = true` 时须提供非空 `http_hosts`，否则实际不可出站；
- 覆写类字段（内存 / 配额 / 超时）**只能收紧**，不能放大。

也可以用 `roots = ["plugins"]` 扫描目录批量加载（最小权限，无 config/ACL），需要精细控制时用 `entries` 清单。

## 权威清单与签名（生产）

除 guest 自述外，可侧载权威清单与签名，形成完整信任链：

```toml
[[plugins.entries]]
path = "plugins/demo.wasm"
manifest = "manifest.json"           # 权威清单，相对 harness.root
provenance = "provenance.json"       # 侧载签名块

[plugins.verify]
require_signature = true             # 公钥白名单固定为 .ribo/trusted_keys.json
```

- `manifest.json`：`{"plugin_id":"demo","tools":[...]}`，宿主逐项比对 id / 工具名 / schema / 超时 / 幂等 / 内存覆写，不符即拒绝加载；
- `provenance.json`：`{"signer_id":"release","signature":"<128 hex>","wasm_digest":"<64 hex>"}`，签名者须在 `.ribo/trusted_keys.json` 白名单内，wasm 摘要为 blake3；
- 生产策略下 `require_signature = true`，缺签名块或缺白名单即启动致命。

## 代码信任闸门

即使配置了 `[plugins] enabled = true`，Project 插件仍需显式代码信任才会加载（见[信任文档](./trust.md)）：

```bash
ribo trust add /path/to/project
```

未授信时插件被跳过并打印提示，其余功能（技能、AGENTS.md）照常。TUI 会在启动时弹 y/n 确认框。

## 测试插件

guest 侧纯逻辑（信封解包、config、manifest 构建、dispatch）可在宿主 target 上单测；host function 相关逻辑需 wasm 目标。端到端测试可参照 `examples/plugin-demo/tests/e2e.rs`：它自动完成 wasm 构建，并走完整的加载 → 注册 → 调用链路。

```bash
cargo test --manifest-path examples/plugin-demo/Cargo.toml
```

## 常见问题

| 现象 | 原因与处理 |
| :--- | :--- |
| 编译产物没有 `.wasm` | 忘记 `crate-type = ["cdylib"]`，或未指定 wasm target |
| 工具调用被拒 `plugin_forbidden` | 对应 host function 能力未在 ACL 授予，或 http 域名不在白名单 |
| 加载失败提示 id 不符 | 权威清单的 `plugin_id` 与 guest 自述不一致 |
| 加载失败提示重复 id | 多个插件声明了相同 `plugin_id`；改名或用 `--allow-duplicate-plugin-id` |
| 插件未加载但无报错 | 项目未授予代码信任，检查启动横幅的 `not code-trusted` 提示 |
