# 技能与 AGENTS.md

## 技能（Skill）

技能是带 frontmatter 的 Markdown 文件，装配期扫描并注入系统提示的 `<available-skills>` 清单。

### 文件布局

技能扫描根由 `harness.skills_roots` 配置（默认 `["skills"]`，相对 `harness.root` 解析）。根目录内支持两种布局：

- 目录型：`<root>/<skill-name>/SKILL.md`
- 扁平型：`<root>/<skill-name>.md`

隐藏项（`.` 开头）与非 `.md` 文件被跳过。

### 文件格式

```markdown
---
name: code-review
description: 审阅代码并给出改进建议
---

审阅时关注以下方面：
1. 正确性
...
```

- `name`：技能名，缺省回退文件名主干；最长 64 字符
- `description`：一句话描述，最长 1024 字符；支持 `|` 块标量多行写法
- frontmatter 之后为技能正文

frontmatter 解析失败、空正文、重名等问题只产生诊断（不致命），其余技能照常加载。诊断可通过 `/skills` 查看。

### 多域来源与优先级

技能按来源域聚合，优先级为 **Project > Global > Plugin**（Plugin 按声明序，先声明优先）。同名技能按优先级取首个，被覆盖者产生诊断提示。

每个技能记录 `origin` 字段（`project` / `global`），在三处保持一致：

- 系统提示 `<skill ... origin="..."/>` 属性
- 配置来源命令 `ribo config --show-origin`
- REPL 命令 `/skills` 输出（显示"来自项目 / 来自全局"）

## AGENTS.md 指令文件

### 加载位置

| 域 | 路径 | 条件 |
| :--- | :--- | :--- |
| Project | `harness.root/AGENTS.md` | 默认加载 |
| Global | 全局域目录下的 `AGENTS.md` | 目录存在时默认加载 |

### 严格限制

- **只认精确文件名 `AGENTS.md`**——`agents.md` / `CLAUDE.md` / `AGENTS.txt` 均不识别
- **只加载域根单文件**——不向上递归（`root/../AGENTS.md` 不加载），不向下递归（`root/sub/AGENTS.md` 不加载）

> 不向上递归是与 pi-agent 等工具的关键行为差异：父目录的指令文件不会被意外加载。

### 拼接语义

所有域的 `AGENTS.md` 按 **Global → Project → Plugin**（声明序）**拼接**，而非取其一：

- 每段带 `<!-- source: <domain> (<path>) -->` 标注，便于审计；
- 矛盾不自动裁决——两份都保留，由模型根据上下文判断（通常"更具体、更靠后"优先）；
- 结果注入系统提示的 `<agents-instructions>` 段。

`AGENTS.md` 属**文档级资源，默认加载**，与代码信任无关——即使项目未授信，其 `AGENTS.md` 仍会加载。

### 大小上限

单个 `AGENTS.md` 上限 **32KB**。超出时按 UTF-8 字符边界截断（不会切坏汉字），并打印 warning 提示"AGENTS.md 超过 32KB，已截断"。

若所有域都没有 `AGENTS.md`，系统提示中**不含** `<agents-instructions>` 段，启动行为与无指令文件完全一致。
