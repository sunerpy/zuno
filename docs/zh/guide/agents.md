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
| `deep` | 建立证据、检验竞争假设、在授权后修复根因并验证恢复 | 在运行时权限内委派有界任务 |
| `fixer` | 聚焦的局部改动及其回归范围 | 不递归委派 |
| `general` | 没有更窄专职 Agent 的有界工作 | 不递归委派 |
| `explorer` | 只读的仓库与调用链调研 | 不递归委派 |
| `librarian` | 当前的外部文档与上游调研 | 不递归委派 |
| `oracle` | 只读的架构与根因评审 | 不递归委派 |
| `looker` | 视觉产物检查 | 不递归委派 |
| `compaction` | 在上下文检查点中保留任务、约束、证据与未完成工作 | 隐藏；无工具 |
| `title` | 用用户的语言按主题命名会话 | 隐藏；无工具 |
| `summary` | 凝练会话成果与尚待用户处理的请求 | 隐藏；无工具 |
| `council-synth` | 综合给定的 Council 证据，保留来源归属与分歧 | 隐藏；无工具 |

全部 15 个原生角色均保留。`orchestrator` 仍是默认 Agent，它与 `deep` 都声明通用
`task` 委派工具。实际能否委派取决于当前工具面、父级权限、配置的深度和各 Agent 的限制。
`build` 继续在单一通道内直接完成工作，不提供子级工具。`deep` 的 mode 是 `all`，既可直接
选为会话 Agent，也可由有委派权限的 Agent 分配一个有界目标。

原生 `review_open` 会自动在 `explorer`、`librarian`、`oracle` 上启动
`balanced-review` Council；评审模型看不到 `council_run`，不能替换 preset 或绕过评审绑定。
只读角色无法触达写入型子 Agent；`review` 不能再开一个 `review`、不能更新 Plan/Todo，
也不能伪造来源或 receipt。席位输出会先由 runtime 解析并自动写入评审事件，再进入 synthesis。

`deep` 可以读取、创建、更新当前持久 Goal，也可以为 Goal 请求输入，因此直接选择的深度工作
会话能够关闭自己实现的证据门禁目标。Goal 所有权与委派是两项独立能力。

## 验证与行为测试先行

执行工作的内置 Agent 共用一份验证规则，涵盖实现、修复、测试、规划和评审：

1. 对已授权的可复现缺陷修复或状态机变更，先新增或扩展一个聚焦的行为测试，并在旧实现上运行。
2. 确认 red 结果确实体现目标行为的失败。构建、依赖、权限或环境错误不算复现了回归。
3. 再实现改动，把同一个测试跑到 green，并运行相关回归检查。覆盖可观察的输入、输出、
   状态迁移，以及适用的中断、重启或恢复路径。

记录确切命令、工作目录、被测源码与输入、预期和实际结果、退出状态，以及权威的测试输出或
产物／运行回执。明确区分已完成、计划执行、受阻和未运行的检查。无法复现时，说明原因并报告
实际可行的最充分验证，不能虚构已通过的运行。

只读角色收集已有的复现步骤、测试与回执，不编辑文件，也不执行会写入的命令；需要补测试时，
交给有写入权限的 Agent。评审者检查 red/green 证据并指出缺口；规划者说明测试和预期失败，
不把计划描述成已经执行。

文档、简单改动和命令／脚本交付采用与影响相称的检查，不要求为每条命令新建测试。
只断言源码中含有某个字符串，不能证明运行时行为。提示词输出契约测试只能验证实际渲染的
提示词，不能证明模型遵循了指引，也不能证明其中描述的运行时行为正确。

串行 CI 等待留在当前前台工作流的关键路径上，由一个调用方负责轮询。后台工作只用于独立的
并行任务或用户明确要求的情形。轮询超时不代表远端失败；报告结果前应检查权威的运行状态。

这是内置提示词指引，不新增运行时门禁、审批机制或权限。显式提示词覆盖仍然生效，
既有角色职责和委派边界继续适用；隐藏的无工具角色保留各自的输出契约。

设计参考为 Codex `eaa8b6d917`：
`codex-rs/models-manager/prompt.md` 的 “Validating your work”、
`codex-rs/prompts/src/review_request.rs::REVIEW_PROMPT` 及其
`codex-rs/prompts/templates/review/rubric.md`，以及
`codex-rs/core/tests/suite/prompt_caching.rs::prompt_tools_are_consistent_across_requests`。
Zuno 借鉴聚焦验证和可复核结论，并将提示词组装方式适配为工作角色共用一份规则。
对可复现缺陷和状态机变更要求先在旧实现上得到 red、再修复到 green，是用户选择的更严格的
Zuno 策略；这不表示 Codex 具有运行时强制的测试先行门禁。

## 深度工作

原生 `deep` 定义要求每回合开始时加载第一方 `deepwork` 与 `verification-planning`
Skill，并按以下方法工作：

1. 从实际代码、测试、日志、持久状态与权威来源建立证据和可复现的基线。
2. 排序竞争假设，沿调用方、状态迁移、清理与错误路径追踪因果链。
3. 选择能用预期观测区分不同原因的实验。每次改变一个因果变量，检查结果，再修正假设。
4. 在获得授权后修复拥有该行为的抽象，并更新受影响的调用方。
5. 验证原始故障、修正后的行为，以及相关的中断、重启或恢复路径；报告实际运行的检查与剩余不确定性。

只要求解释或诊断时，以答案或已证实的根因为交付；选择写入型角色不代表授权未请求的修复。
已经授权的命令和脚本也是有效工作，交付可以是一项运维操作，而不必是源码变更；执行仍受
运行时权限约束。

专业分工有价值时，Deep 可以委派有界的取证或实现任务，但保留因果推理、整合和对子报告的
独立验证责任。委派限制由 runtime 控制，提示词不规定固定的并行数量。

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
# 显式要求 OS 只读约束，不可用时拒绝。
zuno run --agent plan --sandbox-backend auto "audit the retry policy"
```

这项保证只在受约束后端真正运行时由 OS 强制执行。受信的 `sandbox.backend: native` 或
Windows/macOS 平台默认原生执行会记录相同的 `read-only` 请求，但不由 OS 强制执行；
此时“只读”是工具白名单、权限规则与 Shell 风险门禁构成的角色边界，不是 OS 边界。

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

`GET /api/agent` 使用同一份解析后的目录顺序、基础提示词与原生角色权限覆盖层。配置和
Markdown 的提示词覆盖会被保留，隐藏原生角色也不例外；显式环境配置层仍具有最高优先级。
权限规则按公共默认值、原生角色策略、全局配置、解析后的 Agent 配置排列。该接口描述的是
运行时工具过滤与父级 Attempt 权限求交之前的目录策略，不能替代 debug 命令展示的生效视图。

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
