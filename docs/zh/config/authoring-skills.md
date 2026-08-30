# 编写 Skill

Skill 是带自身身份的可复用指导，在匹配时加载，而不是每次请求都加载。这个区别正是 Skill 存在的全部理由：指令文件无条件消耗提示词预算，因此任何只在某些时候适用的内容都应放在这里，而不是放进 `AGENTS.md`。

[Skill](/zh/guide/skills)讲的是如何使用它们。本页讲如何编写一个 Skill 并配置发现机制。

## 文件布局

一个 Skill 是一个包含 `SKILL.md` 的目录，再加上它引用的任何资源：

```text
my-skill/
  SKILL.md
  references/
    ci.md
    architecture.md
  scripts/
    check.sh
```

资源通过 `skill` 工具的 `read_resource` 动作按需读取，路径相对于 Skill 目录。在 `SKILL.md` 中引用一个文件并不会加载它，这正是让一个大 Skill 在任务真正需要那些细节之前保持廉价的原因。

## Frontmatter：恰好两个键

```markdown
---
name: dependency-audit
description: Audit a dependency for supply-chain risk. Use when adding, upgrading, or reviewing a third-party package.
---

# Dependency audit

Steps and criteria go here.
```

| 键 | 类型 | 是否必填 | 说明 |
| --- | --- | --- | --- |
| `name` | `string` | 是 | 该 Skill 在目录中被寻址所用的键 |
| `description` | `string` | 否 | 在目录中公布的触发条件与用途 |

其他内容一概不读。`license`、`version`、`allowed-tools` 之类的键会被忽略而不是被拒绝，因此它们存在无害，但也没有作用。

有两种失败模式值得知道，因为它们看起来是静默的：

- `name` 是数字、布尔值或 null 会导致该 Skill 被整体丢弃；
- `description` 存在但不是字符串同样会导致它被丢弃 —— 包括一个不带取值的裸 `description:`，YAML 会把它解析为 null。

一个完全没有 `description` 的 Skill 会被加载，但会从面向模型的目录中隐藏。对于只会按名称显式调用的 Skill，这偶尔正是你想要的；其他情况下绝不是。

`description` 是触发面。它应当同时说明这个 Skill 做什么、以及何时使用它，因为模型是拿请求与这段文本做匹配，别无其他依据。写成「dependency helper」的描述不会触发；写成「Use when adding, upgrading, or reviewing a third-party package」的会。

## 发现顺序

Zuno 按这个作用域顺序发现 Skill：

| 序号 | 根目录 | 模式 |
| --- | --- | --- |
| 1 | 从当前目录向上直到 worktree 的每个项目 `.zuno` | `{skill,skills}/**/SKILL.md` |
| 2 | 每个项目的 `.agents`，然后 `.claude` | `skills/**/SKILL.md` |
| 3 | Zuno 的全局与已配置的 config 目录 | `{skill,skills}/**/SKILL.md` |
| 4 | `$HOME/.agents`，然后 `$HOME/.claude` | `skills/**/SKILL.md` |
| 5 | 每个 `skills.paths` 条目 | `**/SKILL.md` |
| 6 | 每个由 `skills.urls` 索引产生的缓存目录 | `**/SKILL.md` |

项目作用域会先于用户全局作用域被公布。Zuno 绝不扫描 `.opencode` 或 OpenCode 的配置目录。

同一个规范化来源路径会被去重，包括符号链接别名。来自不同来源的同名 Skill 仍可各自独立寻址，不会选出隐藏的胜出者 —— 这就是目录会报告一个 `source` 定位符、以及名称有歧义时模型必须提供一个来源的原因。有歧义的名称还会禁用直接的 `/<skill-name>` 斜杠形式。

| 变量 | 效果 |
| --- | --- |
| `ZUNO_DISABLE_EXTERNAL_SKILLS=1` | 禁用 `.agents` 与 `.claude` 根目录 |
| `ZUNO_DISABLE_CLAUDE_CODE_SKILLS=1` | 只禁用 Claude 的 skill 根目录 |

在这个较宽的外部开关下，Zuno 原生的 `.zuno` 根目录仍然启用。

## 配置

| 键 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `includeInstructions` | `boolean` \| `null` | 启用 | 各回合是否接收 skill 触发策略与元数据目录 |
| `maxContextTokens` | 正整数 \| `null` | 模型上下文的 2%；未知时约 8,000 个 Token | 目录使用的最大近似 Token 数。超过 10,000 的值会被运行时夹住 |
| `maxSelectedContextTokens` | 正整数 \| `null` | 已知上下文的 10%，下限 2,000，上限 32,000；未知时为 8,000 | 一个会话提示词中所有被完整选中的 Skill 正文使用的最大近似 Token 数。超过运行时上限的值会被夹住 |
| `paths` | `string[]` \| `null` | 无 | 额外的 skill 文件夹路径 |
| `urls` | `string[]` \| `null` | 无 | 用于抓取 skill 的 URL |

```json
{
  "skills": {
    "includeInstructions": true,
    "maxContextTokens": 8000,
    "maxSelectedContextTokens": 16000,
    "paths": ["~/work/shared-skills"]
  }
}
```

这两份预算是刻意分开的。`maxContextTokens` 限定紧凑的元数据目录 —— 名称与描述。`maxSelectedContextTokens` 限定一个会话提示词中被完整加载的正文总量。如果被选中的正文装不下，加载或恢复会话会在 provider 请求之前失败，而不是静默丢弃指令，因为一个只加载了一部分的 Skill 比没有更糟。

`includeInstructions: false` 会把触发策略和目录都从模型提示词中移除。`skill` 工具仍然支持分页的 `list` 与 `search`，因此显式调用继续可用；只有隐式匹配停止。

## 渐进式披露

模型绝不会收到每一份 `SKILL.md` 正文。它看到一份有界目录，然后按需拉取：

| 动作 | 返回 |
| --- | --- |
| `list` | 对模型可见的 Skill 的分页目录，带确切的 `source` 定位符 |
| `search` | 针对一个能力查询的元数据匹配结果 |
| `load` | 一份 Skill 正文，分页并带续读游标 |
| `read_resource` | 一个被引用的文本资源，按相对路径读取 |

`load` 与 `read_resource` 返回与内容绑定的续读游标。调用方必须读到 `complete: true` 才能应用这些指令 —— 一份不完整的 `SKILL.md` 不是可用的指导，而一个步骤被游标边界切开的 Skill 否则可能在只读了一半的状态下被执行。

请按这种消费模型来写。把与决策相关的内容放在正文靠前的位置，把长表格和背景推到 `references/` 里，使第一页就可执行。

## Skill 不能做什么

Skill 提供指令。它不授予工具、权限、文件系统访问、网络访问或环境访问。当前运行时能力快照始终是权威，而选择一个 Skill 永远无法扩大它。

这意味着一个让模型运行某条命令的 Skill 并不授权那条命令。如果该工具不在这个 Agent 的允许列表中，或者某条权限规则拒绝了它，那条指令就会在调用时失败。设计 Skill 时应当说明要做什么，把是否允许交给能力模型判断。

对于必须始终收到某个特定 Skill 的子 Agent，请使用 `agents.<name>.requiredSkills` —— 参见[自定义 Agent](/zh/config/custom-agents)。子级回合独立运行发现，因此在父级加载一个 Skill 不会把它的正文注入被委派的子级。

## 内置 Skill

Zuno 把第一方 Skill 编译进 `zuno-orchestration` 包，带稳定的 `builtin://zuno-orchestration/...` 来源、内容哈希、来源溯源、允许的 Agent profile，以及所需工具声明。它们被编译进可执行文件，不会复制到用户配置目录，因此随二进制一起更新。

不要把其中一个复制到用户 Skill 目录来「覆盖」它。那会造成同名来源歧义，从而禁用该名称的直接斜杠形式。

## 检查发现结果

```sh
zuno debug skill
zuno debug agent build
```

`zuno debug skill` 报告原始发现结果：`view.kind: "raw_discovery"`、`agentFiltered: false`、`extensionOverlayApplied: false`、保留了来自不同来源同名条目的 `skills` 数组，以及一个含来源数、有描述数、唯一数和存在歧义名称的 `summary`。读它之前请先重启，因为它反映的是该进程的发现结果。

`zuno debug agent <name>` 给出的是经 Agent 过滤的视图，包括元数据与被选中正文的预算、已渲染/被省略/被截断的覆盖情况，以及一段有界预览。当一个 Skill 确实存在、但某个特定 Agent 看不到它时，用的就是这一条。

## 参见

- [Skill](/zh/guide/skills)
- [Workflow 与命令](/zh/config/workflows)
- [自定义 Agent](/zh/config/custom-agents)
- [诊断](/zh/operate/diagnostics)
