# 内置工具与文件系统沙箱

## 内置工具

`[tools] builtin_enabled = true`（默认）时注册以下 9 个工具：

| 工具 | 用途 |
| :--- | :--- |
| `calculator` | 表达式计算 |
| `echo` | 回显输入（调试用） |
| `current_time` | 获取当前时间 |
| `read` | 按行窗口读取 UTF-8 文本文件（支持 offset / limit 分页） |
| `write` | 写入文件（覆盖语义，`create_dirs` 控制是否创建父目录） |
| `edit` | 精确文本替换（`old` 唯一匹配或 `replace_all=true`） |
| `ls` | 列出目录 |
| `glob` | 模式匹配文件 |
| `grep` | 内容搜索 |

## 文件系统沙箱

fs 六件套（read / write / edit / ls / glob / grep）共用一个沙箱根：

- 主沙箱根 = `harness.root` 下的 `tools.fs_root` 子目录；未配置时即 `harness.root` 本身；
- `tools.fs_root` 必须是相对 `harness.root` 的路径（绝对路径或 `..` 逃逸组件会 warning 后回退 `harness.root`）。

### 路径校验（双层）

所有 fs 工具路径经过两层校验：

1. **词法层**：拒绝绝对路径、`..` 逃逸组件、非法前缀；
2. **物理层**：从最近已存在祖先 canonicalize，确认符号链接 / Windows 目录联接不能把路径导向沙箱外。

任何越界都返回错误，杜绝通过符号链接逃逸沙箱。

## 只读挂载点与 `scheme://` 前缀

全局 `settings.toml` 的 `[fs.mounts]` 声明只读路径别名，模型用
`scheme://相对路径` 访问项目外数据：

```text
read("datasets://train/img001.jpg")
  → 查名为 "dataset" 的挂载根（用户全局声明）
  → 相对部分在主沙箱相同的双层校验下解析
```

规则：

- `scheme` 为挂载名（须匹配 `^[a-z][a-z0-9_]{0,31}$`，保留字禁用）；未知 scheme → `path.invalid`；
- 路径必须绝对、存在、是目录，且**不在当前工作目录内**——违反即 `config.mount_invalid`；
- 挂载恒为只读：`write` / `edit` 返回 `plugin.plugin_forbidden`；
- 无前缀路径仍走主沙箱根，**不能访问 `.ribo/` 内部目录**（返回 `path.internal_forbidden`）；
- 仅全局可声明——项目配置无权声明挂载点。

```toml
[fs.mounts]
datasets = "/mnt/company-data"
```
