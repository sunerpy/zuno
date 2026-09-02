# 编写 Skill

Skill 是带自身身份的可复用指导，在匹配时加载，而不是每次请求都加载。这个区别正是 Skill 存在的全部理由：指令文件无条件消耗提示词预算，因此任何只在某些时候适用的内容都应放在这里，而不是放进 `AGENTS.md`。

[Skill](/zh/guide/skills)讲的是如何使用它们。本页讲如何编写一个 Skill 并配置发现机制。

## 文件布局

一个 Skill 是一个包含 `SKILL.md` 的目录，再加上它引用的任何资源：

```text
my-skill/
  SKILL.md
  agents/
    openai.yaml
    zuno.yaml
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

一个完全没有 `description` 的 Skill 会被加载，但会从模型驱动的发现中隐藏，除非受支持的侧车文件提供 `interface.short_description`。对于只会按名称显式调用的 Skill，这偶尔正是你想要的；其他情况下绝不是。

没有侧车文件时，`description` 是主要触发面。它应当同时说明这个 Skill 做什么、
以及何时使用它。搜索还会考虑调用名称与可选侧车显示元数据；初始索引优先使用
`interface.short_description`，未提供时才回退到 frontmatter 描述。写成
「dependency helper」的描述太弱；写成「Use when adding, upgrading, or reviewing
a third-party package」才足够可执行。

## 可选侧车元数据

`agents/openai.yaml` 是共享的 Agent Skills 元数据表面。Zuno 只消费已经有运行时语义的字段，并忽略未知字段：

```yaml
interface:
  display_name: Dependency Audit
  short_description: Audit third-party packages before adding or upgrading them
policy:
  allow_implicit_invocation: false
```

- `interface.display_name` 是搜索和诊断使用的人类可读标题。
- `interface.short_description` 会替代较长的 frontmatter 描述进入有界目录，但搜索仍会同时考虑两者。
- `policy.allow_implicit_invocation: false` 会把 Skill 设为 `explicit`。

`agents/zuno.yaml` 按字段覆盖共享文件，并可额外设置 Zuno 原生的目录暴露方式：

```yaml
policy:
  exposure: search
```

支持 `index`、`search` 与 `explicit`。原生 exposure 覆盖
`allow_implicit_invocation`；匹配的用户 `skills.config` 条目最终覆盖两个侧车。
受支持字段格式错误会产生发现警告，但不会丢弃一份原本有效的 `SKILL.md`。

## 发现顺序

Zuno 按这个作用域顺序发现 Skill：

| 序号 | 根目录 | 模式 |
| --- | --- | --- |
| 1 | 从当前目录向上直到 worktree 的每个项目 `.zuno` | `{skill,skills}/**/SKILL.md` |
| 2 | 每个项目的 `.agents` | `skills/**/SKILL.md` |
| 3 | Zuno 的全局与已配置的 config 目录 | `{skill,skills}/**/SKILL.md` |
| 4 | `$HOME/.agents` | `skills/**/SKILL.md` |
| 5 | 每个 `skills.paths` 条目 | `**/SKILL.md` |
| 6 | 每个由 `skills.urls` 索引产生的缓存目录 | `**/SKILL.md` |

项目作用域会先于用户全局作用域被公布。Zuno 不会隐式扫描 `.claude`、
`.opencode` 或其他产品的配置目录；只有确实需要共享时才通过 `skills.paths`
显式加入。

同一个规范化来源路径会被去重，包括符号链接别名。来自不同来源的同名 Skill
仍可各自独立寻址，不会选出隐藏的胜出者。紧凑提示词索引对唯一名称省略来源
路径，只为同名歧义项报告 `source` 定位符；名称有歧义时模型必须提供来源，
并且不能使用直接的 `/<skill-name>` 斜杠形式。

| 变量 | 效果 |
| --- | --- |
| `ZUNO_DISABLE_EXTERNAL_SKILLS=1` | 禁用隐式 `.agents` 根目录 |

在这个较宽的外部开关下，Zuno 原生的 `.zuno` 根目录仍然启用。

## 运行中 catalog generation

每个运行中会话拥有一份不可变的
`SkillCatalogSnapshot { generation, digest, skills, warnings }`。`zuno-watch`
监听全部有效的本地 Skill 根目录与远端缓存根；如果合法根目录尚不存在，只会以
**非递归**方式监听最近的安全已有父目录。缺失目录逐级创建后，订阅会向逻辑根目录
收窄，只有到达精确根目录才切换为递归监听。因此 Zuno 既能发现运行中新建的 Skill
根目录，也不会在启动阶段扫描用户 home 的其他内容。相关事件会防抖，watcher
overflow 会触发完整重扫，新 generation 一次性原子发布。

Prompt 元数据、`requiredSkills`、斜杠命令、`skill` 工具、TUI 与 ACP 都读取同一份
快照。因此新增、修改、删除或重命名 Skill 后，现有会话无需重启即可识别。一个损坏或
暂时不可读的 `SKILL.md` 会保留上一份有效来源并发布 warning，不会把不完整结果替换
进整个 catalog。

`load` 与 `read_resource` 收到当前 generation 中不存在的 locator 时，会强制刷新一次。
来源重新出现则正常加载；已删除或重命名则返回 typed `CatalogStale`，并给出当前可用的
精确 locator。Zuno 不会扫描调用方任意提供的路径，也不会模糊加载一个同名 Skill。

修改发现配置本身，例如新增一个 `skills.paths` 根目录，仍需重配或重启会话，因为这会
改变需要监听的目录集合。

## 配置

| 键 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `includeInstructions` | `boolean` \| `null` | 启用 | 各回合是否接收 skill 触发策略与元数据目录 |
| `maxContextTokens` | 正整数 \| `null` | 模型上下文的 2%；未知时约 8,000 个字符 | 目录的显式近似 Token 上限。超过 10,000 的值会被运行时夹住 |
| `maxSelectedContextTokens` | 正整数 \| `null` | 已知上下文的 10%，下限 2,000，上限 32,000；未知时为 8,000 | 一个会话提示词中所有被完整选中的 Skill 正文使用的最大近似 Token 数。超过运行时上限的值会被夹住 |
| `paths` | `string[]` \| `null` | 无 | 额外的 skill 文件夹路径 |
| `urls` | `string[]` \| `null` | 无 | 用于抓取 skill 的 URL |
| `config` | object[] \| `null` | 无 | 按顺序应用的逐路径启停与暴露方式覆盖 |

```json
{
  "skills": {
    "includeInstructions": true,
    "maxContextTokens": 8000,
    "maxSelectedContextTokens": 16000,
    "paths": ["~/work/shared-skills"],
    "config": [
      {
        "path": "~/.agents/skills/private-release",
        "enabled": false
      },
      {
        "path": "~/.config/zuno/skill/powerapps",
        "recursive": true,
        "exposure": "search"
      }
    ]
  }
}
```

默认元数据字符预算是已知模型上下文数值的 2%；上下文未知时使用约
8,000 个字符。`maxContextTokens` 仍是显式的近似 Token 覆盖；Zuno 会将其
换算为字符，并最多按 10,000 Token 计算。

每个 `config` 对象接受：

| 键 | 类型 | 含义 |
| --- | --- | --- |
| `path` | string | Skill 目录、精确 `SKILL.md` 或子树根目录 |
| `enabled` | boolean | 加载或排除匹配 Skill；省略表示启用 |
| `exposure` | `index` \| `search` \| `explicit` | 覆盖模型发现暴露方式 |
| `recursive` | boolean | 将条目应用到 `path` 下的所有 Skill |

条目按顺序求值，最后一个匹配项获胜。已存在路径会规范化，因此符号链接别名
会匹配同一来源。配置中暂时不存在的路径不是错误，因为 Skill 可能稍后安装。

这两份预算是刻意分开的。`maxContextTokens` 限定紧凑的元数据目录 —— 名称与描述。`maxSelectedContextTokens` 限定一个会话提示词中被完整加载的正文总量。如果被选中的正文装不下，加载或恢复会话会在 provider 请求之前失败，而不是静默丢弃指令，因为一个只加载了一部分的 Skill 比没有更糟。

`includeInstructions: false` 会把触发策略和目录都从模型提示词中移除。`skill` 工具仍然支持分页的 `list` 与 `search`，因此显式调用继续可用；只有隐式匹配停止。

## 渐进式披露

模型绝不会收到每一份 `SKILL.md` 正文。它看到一份有界目录，然后按需拉取：

| 动作 | 返回 |
| --- | --- |
| `list` | `index` 与 `search` 条目的分页目录；只有同名歧义时才带 `source` |
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

`zuno debug skill` 报告原始发现结果：`view.kind: "raw_discovery"`、
`agentFiltered: false`、`extensionOverlayApplied: false`、保留了来自不同来源同名
条目的 `skills` 数组，以及一个含来源数、有描述数、唯一数和存在歧义名称的
`summary`。每次命令都会重新执行发现；运行中的 TUI 或 ACP 会话会自动更新自己的快照。

`zuno debug agent <name>` 给出的是经 Agent 过滤的视图，包括元数据与被选中正文的预算、已渲染/被省略/被截断的覆盖情况，以及一段有界预览。当一个 Skill 确实存在、但某个特定 Agent 看不到它时，用的就是这一条。

## 参见

- [Skill](/zh/guide/skills)
- [Workflow 与命令](/zh/config/workflows)
- [自定义 Agent](/zh/config/custom-agents)
- [诊断](/zh/operate/diagnostics)
