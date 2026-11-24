# 插件系统

## 插件条目

插件条目描述加载哪些 wasm；项目外数据另见 `[fs.mounts]` 只读挂载点（见[用户级配置（全局域）](./global.md)）。

## 插件条目 `[plugins]`

```toml
[plugins]
enabled = false                # 默认关闭：不扫描、不加载
# 扫描目录固定为 .ribo/plugins/（相对 harness.root，缺失 = 软跳过）

[[plugins.entries]]
path = "plugins/demo.wasm"     # 相对 harness.root
# manifest = "manifest.json"   # 权威清单（可选）
# provenance = "prov.json"     # 侧载签名块（可选）
# config = { greeting_prefix = "你好" }

[plugins.entries.acl]
log = true
storage = true
http = true
http_hosts = ["api.example.com"]

[plugins.verify]
require_signature = false      # 生产须开启；公钥白名单固定为 .ribo/trusted_keys.json
```

- 固定扫描 `.ribo/plugins/` 下的 `*.wasm`，按文件名排序加载，取最小权限（ACL 全关）；目录缺失 = 软跳过；
- 需要授予能力或注入 config 时用 `[[plugins.entries]]` 显式清单；
- ACL 由宿主注入，**不信任 guest 自述**；
- 生产策略下应开启 `require_signature = true` 并提供非空公钥白名单 `.ribo/trusted_keys.json`，缺失即启动致命。

## 安全约束

- 插件 wasm 路径必须相对 `harness.root`，且 canonicalize 后仍在沙箱内（堵符号链接逃逸）；
- 装配期检测跨源重复的 `plugin_id`：默认**启动致命**并列出全部来源；传 `--allow-duplicate-plugin-id` 时降级 Warning + 首声明胜出。registry 层保留运行时二道防线。

## 代码信任闸门

`[plugins] enabled = true` 时，Project 插件仍需通过代码信任闸门才会加载：

- 未授信 → 跳过加载 + 启动横幅提示（不致命，其余功能正常）；
- 授予方式：`ribo trust add <项目路径>` 或 TUI 启动确认框。

详见[两级信任域与两档信任](./trust.md)。
