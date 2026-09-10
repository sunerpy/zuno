# Agent

Agent 是一份契约：一段提示词、一条模型路由、一个确切的工具面、一组权限规则，以及一条委派边界。选择 Agent，就是同时选择要完成什么工作、以及可用于完成它的权限有多大。

方向很关键。Agent 契约只能*收窄*权限，不能放宽，这正是只读 Agent 成为一项保证、而不是一个可被配置悄悄反转的默认值的原因。

## 内置阵容

| Agent | 职责 | 委派 |
| --- | --- | --- |
| `orchestrator` | 承担结果、切分工作、整合产出、验证完成 | 可以委派 |
| `build` | 在单一通道内直接完成端到端实现 | 无子级工具 |
| `plan` | 只读调研与可直接实施的规划 | 无子级工具 |
| `review` | 只读的高保证评审：记录可复核证据，判定 draft 还是 ready | 可委派评审席位 |
| `deep` | 深度工作模式，或受委派的根因分析与横切实现 | 不递归委派 |
| `fixer` | 聚焦的局部改动及其回归范围 | 不递归委派 |
| `general` | 没有更窄专职 Agent 的有界工作 | 不递归委派 |
| `explorer` | 只读的仓库与调用链调研 | 不递归委派 |
| `librarian` | 当前的外部文档与上游调研 | 不递归委派 |
| `oracle` | 只读的架构与根因评审 | 不递归委派 |
| `looker` | 视觉产物检查 | 不递归委派 |

`orchestrator` 是默认 Agent，也是唯一向模型暴露通用 `task` 委派工具的原生主 Agent。原生 `review_open` 会自动在 `explorer`、`librarian`、`oracle` 上启动 `balanced-review` Council；评审模型看不到 `council_run`，不能替换 preset 或绕过评审绑定。两者都仍受约束——只读角色无法触达写入型子 Agent；`review` 不能再开一个 `review`、不能更新 Plan/Todo，也不能伪造来源或 receipt。席位输出会先由 runtime 解析并自动写入评审事件，再进入 synthesis。`deep` 的 mode 是 `all`，因此它既可以被直接选为会话 Agent，也可以被 `orchestrator` 作为目标；直接选择并不会赋予它递归委派能力。`deep` 可以读取、创建、更新当前持久 Goal，也可以为 Goal 请求输入，因此直接选择的深度工作会话能够关闭自己实现的证据门禁目标；Goal 所有权不会带来子 Agent 权限。

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
| 一份需要被判定能否直接实施的方案或设计 | `review` |
| 单一区域内范围明确的改动 | `build` |
| 一处局部修复加上它的回归范围 | `fixer` |
| 一个困难的横切问题 | `deep` |
| 需要在多个独立部分上并行展开的工作 | `orchestrator` |
| 只读的代码考古 | `explorer` |
| 当前的外部文档 | `librarian` |

选择按此顺序解析：客户端显式选择的 Agent，然后是顶层 `default_agent`，最后是内置 `orchestrator`。

## 工具降级

每个 Agent 都会按最终对 provider 可见的工具快照生成 `runtime.execution` 降级规则。某个
工具限速、不可用或暂时失败时，不原样重复同一调用；`tool_search` 可见时，Agent 用它发现
另一个已经授权的已连接工具。已连接的 `google_search` 就可以作为 `web_search` 的一种替代
路径。自定义 Agent 解析出相同工具面时也获得同一规则。

Shell 可用时，GitHub 操作优先使用已经安装的 `gh`，仓库搜索优先使用 `rg`，而不是先写原始
`curl` 或手工目录遍历。Agent 必须确认命令存在，并保持相同的来源与证据要求。降级绝不会赋予
原本没有的工具、网络路径、文件系统能力或权限；Shell 或已连接工具不存在时，也不会渲染相应
指导。

## 契约如何收窄权限

一共四层，每一层都只能移除能力：

1. 对于被委派的回合，父级 Attempt 实际对 provider 可见的工具 schema。
2. 目标 Agent 角色及其扩展工具继承策略。
3. 该 Agent 配置的确切 `tools` 允许列表。
4. 生效的用户与 Agent 权限规则。

一条 `allow` 无法恢复一个在父级 Attempt 中本就不存在的工具，而 `permission.mode: "allow_all"` 只会压制询问，不会扩大这个交集。schema 身份也算在内：同名但对 provider 可见的 schema 不同的工具，位于边界之外。

沙箱遵循同一条单向规则。即使调用时选择了 `workspace-write` 或 `danger-full-access`，只读 Agent 仍然获得 `read-only` 约束：

```sh
# Shell cannot modify the workspace, whatever sandbox.mode says.
zuno run --agent plan "audit the retry policy"
```

这项保证只有在受约束后端真正运行的地方才由 OS 强制执行。在受信的 `sandbox.backend: native` 选择之下——那是只读 Agent 在 macOS 与 Windows 上唯一的原生路径——同样的 `read-only` 请求会被记录但不由 OS 强制执行，此时“只读”是一道由工具白名单、权限规则与 Shell 风险门禁构成的角色边界，而不是 OS 边界。

## 只读是角色边界，不只是沙箱模式

`explorer` 的只读来自角色，而不只是沙箱模式。它的默认工具面是 `read`、`glob`、`grep`、只读的 `lsp`、`skill`、`report_write`，以及在只读文件系统策略下的 `shell` 与 `bg`；源码编辑、递归委派、`job` 和网络调研都被拒绝。因此 `du`、`stat`、`file` 可以用来取证，工作区编辑与 Shell 写入仍由运行时限制。

只读调查也能生成报告文件。`report_write` 是独立的宿主能力：只在 `.zuno/reports/` 中保存不可覆盖的产物，并返回确切路径与 SHA-256 receipt。父 Agent 可以读取这些文件，或使用自己的编辑权限复制到用户指定的目标。前台与后台任务的报告元数据都会携带产物 receipt。这不会开放 `.zuno` 配置、扩展、项目源码或 Shell 写权限。

父 Attempt 必须暴露 `report_write`；配置的工具白名单和权限规则仍可移除它。没有该能力时，子 Agent 返回报告正文，由父 Agent 保存。子会话 Shell 的 `Read-only file system` 表示该次尝试的沙箱约束，不能据此判断宿主磁盘或父 Agent 也是只读。

每个能运行命令的角色也必须能检查自己启动的东西。凡是授予 `shell` 的地方都会一并授予 `bg`，只读角色也不例外：后台执行只能通过 `bg` 读回，一个大到无法直接返回进对话的结果同样如此。

全局 `permission.mode: "allow_all"` 会跳过常规确认，但不会抹掉这些显式拒绝。当确实需要外部调研或修改时，请委派给拥有该职责的角色——仓库之外的证据交给 `librarian`，改动交给 `deep` 或 `general`——或者在父会话中完成这项工作。

## Plan 模式

终端应用中的 `/plan` 与 `/start-plan` 会幂等进入 Plan 协作模式，而这项限制是在提示词之下由一层默认拒绝的能力覆盖层强制执行的：允许仓库检查、只读 LSP 与搜索、外部调研、提问、Skill、后台检查、宿主管理的报告以及带类型的 Goal/Plan/Todo 操作，而工作区文件修改、委派、`job` 与 `execute` 被拒绝。`shell` 在该角色获得的只读沙箱下仍然可用，因此命令可以取证，但不能改动工作树。

回到 Work 模式要求已存在一个持久 plan，确认信息会指出它的标题、revision 和已完成步骤数。模型可以建议开始工作，但不能替你选择。一次确认过的选择会作为会话 Agent 落盘，因此 `--continue`、`--session`、`/session` 选择器以及 ACP 的 `session/load` 都会恢复该模式，连同该会话上次使用的模型与推理强度。

当内置 `plan` Agent 完成回答时，当前持久 Plan 与 Todo 就是交给 Start Work 的
工作；它们保留原有执行状态，不会触发自动执行对账续轮，也不会覆盖已经完成的
规划回答。活动 Job 仍会阻止交接；内置 Plan profile 本身不允许调用 `job`。

## 检查一个 Agent 实际解析成什么

```sh
zuno agent list
zuno debug agent explorer
zuno debug permissions
```

`debug agent` 报告经 Agent 过滤后的生效视图，包括元数据与选中正文的 Skill 预算、已渲染与被省略的覆盖情况，以及一段有界预览。`debug permissions` 同时报告配置的与生效的权限模式。请用这些命令，而不是从配置推断结果，因为全局定义与项目定义会相互重叠。

## 自定义 Agent

Agent 既可以在 `zuno.json` 的 `agents.<name>` 下定义，也可以作为 `.zuno/agent/` 下带 frontmatter 的 Markdown 文件定义。文件本身就是定义：Zuno 没有替你写出定义的命令。请自己写好 `.zuno/agent/reviewer.md`，再用 `zuno agent list` 读回解析后的定义。

```markdown
---
description: Review diffs for regressions
mode: subagent
model: openai/gpt-5
---

审查 diff 中的回归，并逐条给出文件与行号。
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
