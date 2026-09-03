# Workflow 与命令

两件不同的东西共用这一页，因为人们常常拿错。命令（command）是用户用斜杠触发的提示词模板。Workflow 是由运行时执行的不可变多 Agent DAG。命令请求模型去做某件事；workflow 决定哪些 Agent 以什么顺序运行。

两者都不授予工具，也都不绕过 Agent 权限。

## 命令

命令是一个带参数宏的字面提示词模板。当有价值的东西正是某个重复请求的确切措辞时，使用它。如果有价值的东西是应当在匹配时触发的可复用指导，那就用 Skill —— 参见[编写 Skill](/zh/config/authoring-skills)。

### 配置形式

命令位于 `command` 映射中，以命令名为键：

| 键 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `template` | `string` | 必填 | 提示词模板 |
| `agent` | `string` \| `null` | 无 | 以哪个 Agent 运行该命令 |
| `description` | `string` \| `null` | 无 | 该命令做什么 |
| `model` | `string` \| `null` | 无 | 用哪个模型运行该命令 |
| `subtask` | `boolean` \| `null` | 无 | 在子任务中运行该命令，而不是在当前会话中 |
| `variant` | `string` \| `null` | 无 | 运行该命令使用的模型 variant |

`template` 是整份配置 schema 的叶子节点中唯一必填的字段。

```json
{
  "command": {
    "audit-deps": {
      "description": "Audit a dependency for supply-chain risk",
      "agent": "reviewer",
      "subtask": true,
      "template": "Audit the dependency $1 for supply-chain risk. Report maintenance status, transitive additions, and pinned version."
    }
  }
}
```

`subtask: true` 比看起来重要。没有它，模板在当前会话中运行并消耗其上下文；有了它，工作发生在一个子级中，父级收到一份报告。

### Markdown 形式

Zuno 从全局配置目录和每个项目 `.zuno` 配置目录递归加载 `command/**/*.md` 与 `commands/**/*.md`。frontmatter 提供同样的元数据；正文是模板。

```markdown
---
description: Audit a dependency for supply-chain risk
agent: reviewer
subtask: true
---

Audit the dependency $1 for supply-chain risk.
Report maintenance status, transitive additions, and pinned version.
```

命令名由路径去掉 `command/` 或 `commands/` 前缀后推导，因此 `command/review/security.md` 就是 `/review/security`。Markdown 命令不需要单独的优先级层级 —— 配置发现会在解析看到 `command` 映射之前就把它们加载进去。

### 参数展开

展开发生在解析期间、分发之前，因此被分发出去的提示词已经是最终形态。

| 占位符 | 展开为 |
| --- | --- |
| `$1`、`$2`、`$3` 等 | 那一个被切分出来的参数 |
| 编号最大的占位符 | 所有剩余参数，以一个空格连接 |
| 超出参数列表末尾的占位符 | 空字符串 |
| `$0` | 空字符串；编号占位符从 `$1` 开始，`$0` 不指向任何参数 |
| `$ARGUMENTS` | 未经切分的原始输入，保留引号、空格和换行 |

「编号最大者贪婪」这条规则最容易让人意外：在有四个参数的 `A=[$1] B=[$2]` 中，`B` 会收到其中三个。正是这一点让 `$1 $2` 可以用于「一个标志，然后一段自由文本」而不必加引号。

当模板完全没有提及占位符、而输入不为空时，原始输入会被追加在一个空行之后。因此一个不含宏的模板仍会收到参数，而不是把它们丢掉。

`$ARGUMENTS` 完全按键入原样保留输入，这就是为什么它是任何需要模型逐字看到的内容的正确选择 —— 一条 shell 命令、一份 diff、一句带引号的句子。输入中的任何字符都不会被当成语法：`$$` 仍是 `$$`，`$&` 仍是 `$&`。

展开在模板上只走一遍，且绝不回读自己刚写入的内容。因此参数中如果本身含有 `$ARGUMENTS` 或 `$2`，它们会作为文本原样插入，不会被再展开一次。

### 哪个定义胜出

四种来源可以定义同一个命令名。优先级递增：

1. 内置的 `init` 与 `init-deep`；
2. `command` 映射条目，包括 Markdown 命令；
3. MCP prompt，以 `<server>:<prompt>` 为键；
4. Skill —— 仅当该名称仍然空闲时。

第 2 层与第 3 层无条件覆盖。第 4 层不会：Skill 绝不遮蔽内置命令、已配置命令或 MCP prompt。MCP prompt 以它的 server 前缀为键，因此它只可能与一个键字面上就是 `server:prompt` 的已配置命令冲突。

一个无歧义的 Skill 本来就可以用 `/<skill-name>` 调用，所以不要仅仅为了造一个斜杠入口而添加命令文件。

Zuno 不注册内置的 `/review`。像评审或发布策略这类产品与组织特有的工作流仍然归用户所有；请把它们定义为 Skill 或命令，语义完全按你项目的需要来定。

## Workflow

Workflow 是一份具名的、由配置拥有的 DAG 模板，由面向模型的 workflow 工具实例化。该模板在运行时不可变 —— 模型选择的是要不要运行它，而不是它包含什么。

| 键 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `nodes` | array of node | 必填 | 由面向模型的 workflow 工具实例化的不可变 DAG |
| `maxAgents` | integer \| `null` | `12` | 一次运行中被准入的最大节点数 |
| `maxParallel` | integer \| `null` | `4` | 同时运行的最大节点数 |

每个节点：

| 键 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `id` | `string` | 必填 | 模板内稳定的节点 id |
| `agent` | `string` | 必填 | 要运行的已配置或内置 Agent |
| `dependsOn` | `string[]` | `[]` | 必须先成功完成的节点 id |
| `description` | `string` \| `null` | 无 | 在运行时投影中展示的、给人读的用途说明 |
| `prompt` | `string` \| `null` | 无 | 可选的节点特定指令，追加到运行提示词之后 |

```json
{
  "workflows": {
    "release-check": {
      "maxParallel": 2,
      "maxAgents": 4,
      "nodes": [
        {
          "id": "code",
          "agent": "reviewer",
          "description": "Review the diff",
          "dependsOn": []
        },
        {
          "id": "upstream",
          "agent": "explorer",
          "description": "Check upstream breaking changes",
          "dependsOn": []
        },
        {
          "id": "synthesis",
          "agent": "deep",
          "description": "Reconcile both reports",
          "dependsOn": ["code", "upstream"]
        }
      ]
    }
  }
}
```

`dependsOn` 要求成功完成，因此一个失败的依赖会让它的下游节点停止，而不是让它们基于不完整的输入运行。`maxParallel` 限定同时运行的节点数，`maxAgents` 限定整次运行；两者都会被校验，而它们存在的原因是无界的扇出是一个成本与限流问题，不只是调度问题。

节点指定 Agent。一个节点的模型来自当前激活 preset 的类别路由，然后是父会话模型 —— `general` Agent 的路由刻意不参与解析。当节点不应硬编码 Agent 名称时，请在 preset 中使用 `categories`。参见[模型路由](/zh/config/models)。

节点指定的每个 Agent 都必须可以从正在运行的 Agent 委派过去。父级上的 `delegates` 是一份确切的允许列表，它同时作用于 workflow 节点和直接委派，因此 workflow 无法绕过一份被收窄的契约。参见[自定义 Agent](/zh/config/custom-agents)。

## 参见

- [编写 Skill](/zh/config/authoring-skills)
- [自定义 Agent](/zh/config/custom-agents)
- [Agent 编排](/zh/guide/orchestration)
- [配置项参考](/zh/config/reference)
