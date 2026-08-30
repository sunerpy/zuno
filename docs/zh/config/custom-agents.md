# 自定义 Agent

Agent 是一份契约，不是一个人设。它固定了运行哪个模型、哪些工具可见、可以委派给哪些子级，以及这个回合拥有什么权限。定义一个 Agent 的理由是让一套更窄的能力集合可复现，而不是依赖提示词的措辞。

支配下文一切的规则是：Agent 契约只能收窄。一个配置的 `allow`，包括 `permission.mode: "allow_all"`，都无法恢复父级 attempt 从未拥有的能力。完整的权限模型见[权限与沙箱](/zh/guide/permissions)。

## 两种定义方式

Agent 位于 `agents` 映射中，以 Agent 名为键：

```json
{
  "agents": {
    "reviewer": {
      "description": "Reviews a diff and reports findings without editing",
      "mode": "subagent",
      "tools": ["read", "grep", "glob"],
      "permission": { "rules": { "shell": "deny" } }
    }
  }
}
```

或者写成带 frontmatter 的 Markdown，在全局配置目录和每个项目 `.zuno` 目录下的 `{agent,agents}/**/*.md` 中被发现。frontmatter 接受与一个 `agents` 映射条目相同的字段；正文是系统提示词。

```markdown
---
description: Reviews a diff and reports findings without editing
mode: subagent
tools: [read, grep, glob]
---

Report findings as a list. Do not edit files.
```

当提示词长到 JSON 字符串转义变得难受时，优先用 Markdown；当定义主要由能力字段构成时，优先用 JSON。

## 全部字段

| 键 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `color` | 主题颜色 \| `#rrggbb` \| `null` | 无 | 显示颜色 |
| `delegates` | `string[]` \| `null` | 无 | 用于直接委派与 workflow 的确切子 Agent 允许列表 |
| `description` | `string` \| `null` | 无 | 何时使用该 Agent |
| `disable` | `boolean` \| `null` | 无 | 移除这个 Agent |
| `hidden` | `boolean` \| `null` | 无 | 在 `@` 自动补全菜单中隐藏该 Agent |
| `mode` | `subagent` \| `primary` \| `all` \| `null` | 无 | 该 Agent 可以在哪里使用 |
| `model` | `string` \| `null` | 无 | `provider/model` 形式的模型 |
| `options` | object \| `null` | 无 | provider 选项，包含每一个被收拢进来的未知键 |
| `permission` | object \| `null` | 无 | 该 Agent 的逐工具权限 |
| `prompt` | `string` \| `null` | 无 | 系统提示词 |
| `reasoning` | `off` \| `low` \| `medium` \| `high` \| `xhigh` \| `max` \| `null` | 无 | provider 中性推理级别，仅在已配置模型时生效 |
| `requiredSkills` | `string[]` \| `null` | 无 | 该 Agent 每个回合开始时加载的 Skill |
| `steps` | 正整数 \| `null` | 无上限 | 在一次纯文本收尾请求之前，可调用工具的最大迭代次数 |
| `temperature` | number \| `null` | 无 | 采样温度 |
| `tools` | `string[]` \| `null` | 无 | 该 Agent 对模型可见的确切工具允许列表 |
| `top_p` | number \| `null` | 无 | 核采样截断值 |
| `variant` | `string` \| `null` | 无 | 默认模型 variant，仅在该 Agent 已配置模型时生效 |

具名的主题颜色是 `primary`、`secondary`、`accent`、`success`、`warning`、`error` 和 `info`。十六进制颜色码在进入时会被校验。

这里不会拒绝未知键。`agents` 映射会把任何它没有具名的键收拢进 `options` 并原样保留，这就是 provider 特有设置无需 schema 逐一枚举也能到达 SDK 的方式。这也是整份配置中唯一一处拼写错误不会产生错误的地方，所以在添加不常见的键之后请检查 `zuno debug agent`。

## 行为不那么直观的字段

`tools` 是一份确切的允许列表，不是追加。设置它会替换该角色本来会暴露的一切，而且由于数组在配置层之间是替换关系，一个设置了 `tools` 的项目层会完全丢弃全局列表。`delegates` 与 `requiredSkills` 同理。

`mode` 决定可达性。`subagent` 表示该 Agent 只能作为委派目标使用，绝不能作为顶层会话 Agent；`primary` 反之；`all` 两者皆可。`default_agent` 必须是一个 primary Agent。

`steps` 统计一个回合内可调用工具的 provider 迭代次数。当上限耗尽而模型仍然请求继续时，Zuno 会额外发送一次不带工具的纯文本请求，要求它简要说明已完成的工作、剩余工作、证据和阻塞点。它不会静默提高或重置这个上限。省略该字段即使用默认的无上限行为。

`requiredSkills` 保证的是指令，不是权限。在 profile 与 Agent 可见性过滤之后，每个名称都必须恰好解析到一个来源；名称缺失或存在歧义会让子级启动失败，而不是静默跳过这项要求。该字段不会把 Skill 复制进配置，也不授予工具 —— 所以 `"requiredSkills": ["codegraph"]` 保证的是 CodeGraph 指令，不是 CodeGraph 的执行权限。

`reasoning` 与 `variant` 只在该 Agent 已配置 `model` 时生效。没有模型却设置其中任何一个不是错误，但它对别处选出的路由没有影响。

`disable` 会把某个内置 Agent 从阵容中移除。要去掉一个你不想让它可用的角色，这是预期做法，而不是试图逐个剥离它的工具。

`hidden` 只影响 `@` 自动补全菜单。被隐藏的 Agent 仍然可以被委派，仍然可以按名称触达。

## 能力上限

对一次被委派的回合，父级 attempt 实际对 provider 可见的工具 schema 构成一个不可变的上界。目标角色、它的 MCP 与扩展继承策略、该 Agent 的确切 `tools` 允许列表，以及生效的权限规则，都只能收窄这个集合。一个同名但对 provider 可见 schema 不同的工具同样位于边界之外。

因此 MCP 与扩展工具不会自动对每个只读 Agent 可用。以下条件必须全部成立：那个确切的 schema 在父级 attempt 中可见、目标角色要么自动继承扩展工具、要么带有一次针对该 Agent 的确切 `permission.rules` 授予、允许列表保留了它的 wire id，并且之后没有显式规则拒绝它。未知的 MCP 工具默认仍然有副作用，所以授予一个经过审计的查询工具，不会让该 Agent 一并获得所有 MCP 工具。

## 逐 Agent 权限

`permission` 对象与全局的那一份对应：

| 键 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `mode` | `standard` \| `strict` \| `allow_all` | `standard` | 未决与有副作用的调用如何被准入 |
| `rules` | object | `{}` | 有序的逐工具规则。显式拒绝在任何模式下都是终态 |

显式拒绝在任何模式下都是终态，包括 `allow_all`。这种不对称正是关键所在：`allow_all` 移除的是询问，不是限制。

## Agent 的模型路由

Agent 的 `model` 是最具体的路由。当它缺省时，先应用当前激活 preset 的 Agent 路由，然后是父会话模型。为整个团队做路由时，preset 是更合适的工具 —— 参见[模型路由](/zh/config/models) —— 因为它把模型选择集中在一处，而不是散落在各个 Agent 定义中。

## 创建与检查

```sh
zuno agent create --path .zuno/agent/reviewer.md --mode subagent
zuno debug agent reviewer
```

`zuno debug agent <name>` 打印生效的解析后契约：模型路由、工具可见性、权限规则集，以及经 Agent 过滤的 Skill 视图（含元数据与被选中正文的预算）。在改动 `tools`、`permission` 或 `requiredSkills` 之后请读一遍，因为解析出的集合是多层作用的结果，仅靠读配置无法可靠预测。

## 相关的顶层键

| 键 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `default_agent` | `string` \| `null` | 无 | 未指定时使用的 Agent。必须是 primary Agent |
| `subagent_depth` | integer \| `null` | `1` | 子 Agent 的最大嵌套深度 |

## 参见

- [Agent](/zh/guide/agents)
- [权限与沙箱](/zh/guide/permissions)
- [模型路由](/zh/config/models)
- [Agent 编排](/zh/guide/orchestration)
