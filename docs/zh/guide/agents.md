# Agent

Agent 是一份契约：一段提示词、一条模型路由、一个确切的工具面、一组权限规则，以及一条委派边界。选择 Agent，就是同时选择要完成什么工作、以及可用于完成它的权限有多大。

方向很关键。Agent 契约只能*收窄*权限，不能放宽，这正是只读 Agent 成为一项保证、而不是一个可被配置悄悄反转的默认值的原因。

## 内置阵容

| Agent | 职责 | 委派 |
| --- | --- | --- |
| `orchestrator` | 承担结果、切分工作、整合产出、验证完成 | 可以委派 |
| `build` | 在单一通道内直接完成端到端实现 | 无子级工具 |
| `plan` | 只读调研与可直接实施的规划 | 无子级工具 |
| `deep` | 深度工作模式，或受委派的根因分析与横切实现 | 不递归委派 |
| `fixer` | 聚焦的局部改动及其回归范围 | 不递归委派 |
| `general` | 没有更窄专职 Agent 的有界工作 | 不递归委派 |
| `explorer` | 只读的仓库与调用链调研 | 不递归委派 |
| `librarian` | 当前的外部文档与上游调研 | 不递归委派 |
| `oracle` | 只读的架构与根因评审 | 不递归委派 |
| `looker` | 视觉产物检查 | 不递归委派 |

`orchestrator` 是默认 Agent，也是唯一暴露 `task` 委派工具的原生主 Agent。`deep` 的 mode 是 `all`，因此它既可以被直接选为会话 Agent，也可以被 `orchestrator` 作为目标；直接选择并不会赋予它递归委派能力。

## 如何选择

```sh
zuno run --agent plan "why does the retry budget start before the first attempt?"
zuno run --agent build "add pagination to the /users endpoint and run the tests"
zuno run --agent deep "the compaction boundary drops the tail on resume; find the root cause"
zuno tui --agent orchestrator
```

一条实用规则：

| 场景 | Agent |
| --- | --- |
| 你想要一个答案或一份计划，不要任何写入 | `plan` |
| 单一区域内范围明确的改动 | `build` |
| 一处局部修复加上它的回归范围 | `fixer` |
| 一个困难的横切问题 | `deep` |
| 需要在多个独立部分上并行展开的工作 | `orchestrator` |
| 只读的代码考古 | `explorer` |
| 当前的外部文档 | `librarian` |

选择按此顺序解析：客户端显式选择的 Agent，然后是顶层 `default_agent`，最后是内置 `orchestrator`。

## 契约如何收窄权限

一共四层，每一层都只能移除能力：

1. 对于被委派的回合，父级 Attempt 实际对 provider 可见的工具 schema。
2. 目标 Agent 角色及其扩展工具继承策略。
3. 该 Agent 配置的确切 `tools` 允许列表。
4. 生效的用户与 Agent 权限规则。

一条 `allow` 无法恢复一个在父级 Attempt 中本就不存在的工具，而 `permission.mode: "allow_all"` 只会压制询问，不会扩大这个交集。schema 身份也算在内：同名但对 provider 可见的 schema 不同的工具，位于边界之外。

沙箱遵循同一条单向规则。即使调用时选择了 `workspace-write` 或 `danger-full-access`，只读 Agent 仍然获得 `read-only` 约束：

```sh
# Cannot write, whatever sandbox.mode says.
zuno run --agent plan "audit the retry policy"
```

## 只读是角色边界，不只是沙箱模式

`explorer` 是原生只读，而不是 shell 只读。它的默认工具面是 `read`、`glob`、`grep` 和只读的 `lsp`；`shell`、编辑、委派和网络调研都被拒绝。`du`、`stat`、`file` 这类命令是通过 `shell` 触达的可执行文件，因此即使它们只做读取，也不属于 `explorer`。

全局 `permission.mode: "allow_all"` 会跳过常规确认，但不会抹掉那条显式拒绝。当确实需要基于命令的检查时，请委派给一个具备 shell 能力的 Agent（例如 `deep` 或 `general`），或者在父会话中运行那条有界命令。

## Plan 模式

终端应用中的 `/plan` 会切换协作模式，而这项限制是在提示词之下由一层默认拒绝的能力覆盖层强制执行的：允许仓库检查、只读 LSP 与搜索、提问、Skill 以及带类型的 Goal/Plan/Todo 操作，而 shell 与文件修改被拒绝。

回到 Work 模式要求已存在一个持久 plan，确认信息会指出它的标题、revision 和已完成步骤数。模型可以建议开始工作，但不能替你选择。一次确认过的选择会作为会话 Agent 落盘，因此续跑会恢复该模式。

## 检查一个 Agent 实际解析成什么

```sh
zuno agent list
zuno debug agent explorer
zuno debug permissions
```

`debug agent` 报告经 Agent 过滤后的生效视图，包括元数据与选中正文的 Skill 预算、已渲染与被省略的覆盖情况，以及一段有界预览。`debug permissions` 同时报告配置的与生效的权限模式。请用这些命令，而不是从配置推断结果，因为全局定义与项目定义会相互重叠。

## 自定义 Agent

Agent 既可以在 `zuno.json` 的 `agents.<name>` 下定义，也可以作为 `.zuno/agent/` 下带 frontmatter 的 Markdown 文件定义：

```sh
zuno agent create --path .zuno/agent/reviewer.md --mode subagent \
  --description "Review diffs for regressions" --model openai/gpt-5
```

一个配置或扩展提供的 Agent，只要它的 mode 是 `subagent` 或 `all`，就可以加入委派阵容。仅为 `primary` 的 Agent 不能作为委派目标。完整字段清单见[自定义 Agent](/zh/config/custom-agents)，委派机制见[编排](/zh/guide/orchestration)。
需要把 Agent 与 Skill 或工具打包，或实现 WASI/原生行为时，见
[开发 Agent 与扩展](/zh/guide/extension-development)。

## 参见

- [自定义 Agent](/zh/config/custom-agents)
- [工具](/zh/guide/tools)
- [权限与沙箱](/zh/guide/permissions)
- [编排](/zh/guide/orchestration)
- [开发 Agent 与扩展](/zh/guide/extension-development)
