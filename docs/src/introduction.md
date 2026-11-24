# 引言

Ribo Agent 是一个以 Rust 构建的终端 AI Agent，提供 `ribo`（命令行 REPL）与 `ribo-tui`（全屏终端）两个二进制入口。它在单一的 Agent 主循环之上，组合了分层配置、两级信任域、技能与指令注入、沙箱文件系统工具、WASM 插件等能力。

## 核心概念

| 概念 | 说明 |
| :--- | :--- |
| **分层配置** | provider（通道 + 多模型）/ routing（默认二元组 + 降级规则）/ retry / compression / session / tools / harness / global 等层相互独立、层间零交叉；另有 `[plugins]` 与全局 `[fs.mounts]` 两组独立配置；未配置或解析失败时按层降级（详见[配置详解](./configuration.md)） |
| **两级配置域** | Project（项目域，`.ribo/` 集中目录）与 Global（全局域，用户目录）；项目外数据经 `[fs.mounts]` 只读挂载点 |
| **两档信任** | 配置信任（文档级资源，默认开启）；代码信任（可执行资源，须显式授予） |
| **技能（Skill）** | 带 frontmatter 的 Markdown 文件，装配期扫描并注入系统提示，可按域标记来源 |
| **AGENTS.md** | 域根下的指令文件，按 Global → Project → Plugin 顺序拼接注入 |
| **插件（Plugin）** | WASM 模块，宿主注入能力（ACL），加载前须通过代码信任闸门 |

## 架构分层

| Crate | 职责 |
| :--- | :--- |
| `ra-core` | 统一路径原语与错误码（`path.invalid` / `path.escaped` 等） |
| `ra-provider` | 模型提供方契约、网关、重试与路由 |
| `ra-agent` | 事件驱动的 Agent 主循环 |
| `ra-tools` | 工具系统（calculator / echo / time + fs 六件套） |
| `ra-harness` | 会话容器、上下文压缩、系统提示与技能注入 |
| `ra-plugins` | WASM 插件运行时、ACL、签名校验与注册表 |
| `ra-cli` / `ra-tui` | 命令行与全屏终端交互层 |

## 本文档的范围

本指南聚焦**使用**——如何配置、如何运行、各功能的语义与注意事项。若需了解内部实现与架构决策，请阅读源码与各 crate 的模块注释。

零配置即可启动：无需任何密钥或配置文件，运行 `ribo` 即进入 Mock 模式（本地模拟器），可体验完整交互与工具流程。
