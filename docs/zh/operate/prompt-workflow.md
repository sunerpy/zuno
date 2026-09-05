# 提示词与工作流

本文说明当前代码已经接线的 Prompt、Agent、委派、持久化工作流和诊断能力，也明确列出仍未达到设计目标的部分。内置 Prompt 保持英文；本指南使用中文解释用户可观察行为。

## 1. Prompt 如何组成

Zuno 不把所有规则拼成一段不可追踪的字符串。每个模型可见 section 都有稳定 id、来源、顺序、语义角色、信任级别、字节数、token 估算和 SHA-256。

当前 canonical lane 顺序为：

1. Agent role；
2. Plan/Work collaboration mode；
3. runtime policy；
4. 全局 `AGENTS.md`；
5. 项目和附近目录的 `AGENTS.md`；
6. durable work state；
7. routing 和 Skill policy；
8. 已选择的 Skill 正文；
9. Skill 索引；
10. memory。

不是每一轮都会出现全部 lane。不存在的能力不会产生对应提示。

### 1.1 稳定 runtime sections

| section | 行为 | 注入条件 |
| --- | --- | --- |
| `runtime.intent` | 以当前用户请求或委派目标为权威，不擅自扩大任务。 | 始终。 |
| `runtime.execution` | 选择最小完整工作流，批量独立读取，不重复读取不变状态或重复检查。 | 始终；只有 `plan_update` 最终可见时才向模型附加 Plan 修改指导。 |
| `runtime.sandbox` | 明确 Shell 正以宿主权限运行，以及请求/生效模式、仍然生效的权限模式和原因：类型化的不可用原因，或显式的 `sandbox.backend: native` 选择。 | 可信的 sandbox unavailable fallback 或受信的 `sandbox.backend: native` 生效时。 |
| `runtime.continuity` | 把 History/Notes 结果视为不可信会话数据，并说明会话、Agent 与 revision 边界。 | 最终工具快照含 `history` 或 `notes`。 |
| `runtime.editing` | 保留无关修改，修改 owning abstraction，不机械重试不确定副作用。 | 有真实编辑/写入能力，或 Shell 可写工作区。 |
| `runtime.verification` | 用观察证据证明完成，明确未验证项和阻塞。 | 始终。 |
| `runtime.delegation` | 只委派有价值且边界不重叠的任务，不轮询或重复派发。 | `task` 和至少一个合法子 Agent 同时有效。 |
| `runtime.persistence` | Goal、Plan、Todo、Inbox、Job 才是继续执行的状态真相。 | durable state 已激活或相关工具有效。 |

这些 section 在 request hook 已经被限制为只能缩减工具集合之后生成，因此不会提示已被隐藏或移除的工具。来源固定为 `zuno-runtime:<section-id>`。

hook、动态上下文、历史、附件和最终 tool schema 完成组装后，Zuno 还会估算完整
provider-visible input。若模型 context limit 已知且总估算超限，本轮在发起
provider 请求前以 typed error 失败；不会静默截断 AGENTS、历史、已选 Skill 或工具
合同。模型窗口未知时仍由 provider 做最终限制。

## 2. 内置 Agent 的职责

- `orchestrator`：一个清晰的独立动作直接完成；复杂任务才建立依赖图和非重叠委派。主会话保留集成、冲突处理和最终验收。
- `build`：一个端到端实现 lane，不委派。
- `deep`：复现问题、排序假设、追踪因果链、修复根因并验证恢复路径，不递归委派。
- `plan`：严格只读，完成事实探索、必要决策、实现设计和验收方案。
- Specialist：使用自然 Markdown；有价值时使用 `Outcome`、`Evidence`、`Inspected/Changed`、`Risks/Blocker`，不要求自行输出 JSON/XML 机器协议。

工具、权限、Sandbox、MCP、Skill、Plan 和 Goal 由 runtime 的有效能力生成，不应复制到每个角色 Prompt。

## 3. 什么时候需要 Plan

默认 profile 提供类型化的宿主 Planning capability。该 capability 存在时，宿主会在
第一次 provider request 之前运行确定性分类器；分类不依赖 `plan_update` 是否对模型可见：

- session 已有 active Plan 且输入是明确继续：保持并继续维护；
- 新的实质性多阶段目标：归档此前可见 Plan，并安装一个只包含新目标步骤的新根 Plan，
  不把通用模板重复追加到同一个列表；
- child report、steering、retry 不会自行创建 Plan；
- 直接回答、一次有界读取、对已经准备好的修改执行一次短小 commit：可作为原子操作；
  单句短问句无论是否带问号都属于直接回答，包括疑问词位于句中的中文问法；
- 问候、致谢或单纯的确认（如 `你好`、`谢谢`、`好的`、`hi`、`thanks`）属于对话输入，
  归为 `Atomic`，不会打开 Plan；已有活跃 Plan 时视为继续维护；
- 图片、resource、selection、branch diff 等 typed context，以及足够大的多文本块
  输入：默认进入 planned path；
- 其他普通工程任务：宿主把请求分类为 `Required`，但不写入任何通用骨架；模型必须先
  读取当前 Plan，再通过操作式 `plan_update` 创建战略步骤。

一个活跃步骤需要聚焦临时工作时，使用 `plan_update action=push` 持久暂停父 Plan，
让子 Plan 暂时成为客户端唯一可见的 Plan；子步骤全部完成后使用 `action=pop`，父
Plan 只恢复一次。工作开始、完成、阻塞或范围变化时立即更新活跃 Plan，最终回复前
必须对账。

因此“调研 → 修改 → 验证”通常会有 Plan；跨组件、委派、多个验收 gate，
以及可能经历压缩或重启恢复的工作必须持续维护 Plan。模型可以通过
最终仍可见的 `plan_update` 创建或维护战略 Plan，但不能决定宿主是否要求 durable
execution state。隐藏 `plan_update` 会阻止模型创建或修改新 Plan；已有 Plan 仍会持久化、
投影并在重启后恢复。
Todo 是 Plan step 下可选的具体工作，用于更细的所有权、依赖或恢复跟踪，不要求
和 Plan step 机械地一一对应。`create`、`append`、`push` 的 step id 由宿主生成；
`patch` 只提交发生变化的 id；`pop` 只提交当前 revision。所有已有 Plan 的修改都要
携带 `expected_revision`，`completed` 与 `superseded` 都是不可回退的终态。

Plan collaboration mode 与“工作任务是否值得维护 Plan”是两件事。`plan` Agent 是只读模式；`build`、`deep` 或 `orchestrator` 也可以在复杂实现中通过 typed Plan 工具维护执行状态。

## 4. Typed `DelegationContract`

当前 `task` 工具要求四个字符串字段：

```json
{
  "objective": "定位 prompt receipt 与 provider request 的关联缺口",
  "deliverable": "调用链、缺口和最小修改文件集合",
  "instructions": "只读审计；使用结构化代码导航。",
  "success_evidence": "列出精确符号和文件，并区分事实与推断。",
  "scope": {
    "include": ["crates/zuno-engine", "crates/zuno-cli"],
    "exclude": ["credential stores"]
  },
  "constraints": {
    "must": ["保留未提交修改"],
    "must_not": ["编辑文件", "启动外部服务"]
  },
  "dependencies": ["CodeGraph index is current"],
  "agent": "explorer",
  "background": true,
  "reportDelivery": "nextStep"
}
```

规则：

- `objective`、`deliverable`、`instructions`、`success_evidence` 必填且不能为空。
- `scope.include/exclude`、`constraints.must/must_not`、`dependencies` 可选。
- `agent` 必填，并且必须属于父 Agent 的有效 delegate roster。
- `background` 默认为 `false`。
- background 的 `reportDelivery` 默认为 `nextStep`，也可设为 `quiet`。
- `task_id` 是此前子会话的 session id，只能继续同一 parent 拥有的 child。
- 子 Agent 的 model、reasoning、MCP、Skill 与 Sandbox 来自父 composition、Agent
  配置和 active preset 的有效交集，模型侧不能逐次覆盖。
- `description`、`prompt`、`subagent_type`、`category`、`model`、`effort`、
  `load_skills` 和未知字段直接拒绝，不提供迁移层。

宿主会把 typed contract 渲染为 child prompt。内部 `ChildTurnRequest` 仍有 `description` 和 `prompt` 字段，这是宿主编译后的内部类型，不是旧 model-facing 参数兼容。

## 5. 父子能力继承

子会话从父 composition 继承：

- 同一配置和模型目录；
- 默认的 parent provider/model 与 reasoning 路由；
- MCP 配置和已连接 catalog；
- Skill discovery 配置；
- Sandbox 配置；
- permission ceiling；
- immutable parent Attempt capability snapshot。

最终能力不是简单复制，而是取交集：

```text
parent Attempt 的精确工具 schema
∩ child role 权限
∩ child tools allowlist
∩ effective permission visibility
∩ 当前 MCP/extension/provider 可用性
```

即使 `permission.mode` 为 `allow_all`，也不能恢复父请求没有看到的工具 schema。

`agents.<name>.requiredSkills` 会按名称和精确来源解析，并在 provider request 前自动加载。缺失或同名多来源会失败关闭。父 Agent 已展开的 Skill 正文不会无条件复制到 child；child 会从自己的有效 Skill catalog 重新加载。Skill 本身不授予工具权限。

Skill 目录元数据和已选 Skill 正文使用两套独立预算：

- `skills.maxContextTokens` 只约束紧凑目录；
- 所有已选正文共享一个聚合预算，默认是已知模型窗口的 10%，下限约
  2,000 tokens、上限 32,000 tokens；未知窗口使用约 8,000 tokens；
- `skills.maxSelectedContextTokens` 可覆盖推导值，但仍受 32,000-token
  上限约束；
- 新加载或历史恢复超过预算时，在 provider request 前明确失败，不截断正文，
  也不静默丢弃某个 Skill。

## 6. 宿主生成的 `TaskReportMetadata`

native child 终止时，宿主从 durable session 和 typed tool metadata 生成：

```json
{
  "schemaVersion": 2,
  "jobId": "job_*",
  "workContext": {
    "schemaVersion": 1,
    "goalId": null,
    "planId": "plan_*",
    "planRevision": 3,
    "planStepId": "verify-release"
  },
  "sessionId": "ses_child",
  "parentSessionId": "ses_parent",
  "agent": "explorer",
  "status": "completed",
  "finalText": "...",
  "usage": {},
  "changedPaths": [],
  "verificationRecords": [],
  "uncertainSideEffects": [],
  "evidenceErrors": []
}
```

其中：

- usage 来自 child session 的 durable usage snapshot；
- `workContext` 由宿主在 Job 准入时从当前 Plan 捕获，未绑定 Plan 时为 `null`；
- `changedPaths` 只读取工具 metadata 中的 `writtenPaths`；
- `verificationRecords` 只读取 typed `taskVerification`；
- `uncertainSideEffects` 来自 typed metadata 或 restart reconciliation；
- 不解析模型 prose 或任意 Shell 输出伪造机器字段。

background Job 的 `agent_job.result` 保存该 metadata。`nextStep` 还会把同一 metadata
放入 parent inbox 的 `subagentReport.metadata`；`quiet` 不创建 parent input。
foreground `task` 同样先创建一个 attached、quiet 的 internal Job，并在同一
`subagent.report` 位置返回包含该 `jobId` 的相同 schema。报告进入 parent transcript
后，同一值还会写入规范 user message 的
`message.data.taskReport`。ACP 历史回放投影为
`_meta.zuno.kind = "task_report"` 和 `_meta.zuno.taskReport`，不再退化为只能查看
raw tool JSON。TUI 数据库回放也会恢复 typed status、final text、changed paths、
verification、uncertain evidence 和 evidence errors。三种表面均消费宿主字段，
而不是解析英文展示文本或要求模型生成机器协议。

每个 internal Job 还会持久化本次执行的证据起点。继续已有 child session 时，
`changedPaths`、`verificationRecords` 等只取起点之后的 typed tool metadata，不会
把该 child 先前轮次的结果误算到当前委派。
只要 `workContext.planStepId` 对应的 Plan 步骤尚未完成，终态 Job 的成功或失败证据
也会继续进入 `runtime.work_state`，避免上下文压缩后重复委派同一调查。

## 7. Job、报告和 Goal completion barrier

### 7.1 `nextStep`

terminal Job settlement 与 parent report admission 在一个 SQLite 事务内完成。提交后才尝试 wake parent：

- parent 正在运行时，报告在 tool-safe boundary 进入后续 step；
- 错过最后安全点时，等待当前 lease 结束后启动下一 turn；
- parent idle 时立即驱动；
- restart 时从 durable inbox 恢复。

进程在 promote 后退出时，恢复逻辑复用原 input row 并回到原 delivery lane，不重新
创建 report，也不重新执行 child。同一进程内并发请求唤醒同一个
`(session_id, input_id)` 时，coordinator 只允许一个 in-flight wake；其他调用返回
`AlreadyInFlight`。若实际 wake 失败，lease 会释放，后续可针对同一个 durable
input 安全重试。保证的是一份逻辑报告和一次有效 admission/drive，而不是跨进程
崩溃边界禁止重试 wake 函数。普通 settlement 与 restart recovery 共用有界重试：
最多 3 次，从 10 ms 开始指数退避，单次上限 100 ms。

### 7.2 `quiet`

`quiet` 只持久化 Job terminal result：

- 不创建 parent input；
- 不 wake parent；
- 由 `job` 工具或客户端 Job projection 主动查看。

### 7.3 restart

- 尚未开始的 queued Job：重启后记为 `cancelled`。
- 已经开始但进程丢失的 running Job：记为 `uncertain`，不机械重放。
- 已经 settle 的 `nextStep` report：恢复同一 inbox row。

### 7.4 Goal completion barrier

模型 Goal 工具与 `/goal complete` 都调用同一个 transactional barrier。以下状态会阻止完成：

- 未完成的 Plan step；
- 未完成的 Todo/WorkItem；
- queued/running Job；
- terminal `nextStep` Job 的 report input 仍为 queued、steering 或 promoted。
- uncertain Job，无论 delivery 或 report 是否已消费。

普通 terminal report 被 consumed 后，该 Job 不再阻止完成。terminal `quiet`
completed/failed/cancelled Job 不阻止完成；uncertain 始终阻止，直到后续 typed
authoritative reconciliation 改变其状态。

### 7.5 逻辑任务去重与 `job_reconcile`

宿主根据 `agent + DelegationContract` 计算稳定 `logical_key`。同一 parent 下，只要
先前逻辑任务仍为 queued、running、uncertain，或 terminal `nextStep` 报告尚未
consumed，即使换一个新 child session 也会拒绝重复派发。该规则同时覆盖 foreground
和 background。创建新 child 时，child session 与 internal Job 在一个 SQLite
事务内 admission；重复任务被拒绝时不会残留孤立 child session。同一 provider
Attempt 内，即使第一个 foreground 已经完成，模型在同一响应里发出的第二个相同
task 也不会因串行执行而重跑。

`job_reconcile` 只处理当前 parent 拥有的 uncertain Job：

```json
{
  "jobID": "job_*",
  "outcome": "completed",
  "finalText": "权威系统确认操作已完成",
  "authority": "CI run 123 or deployment API",
  "evidence": "terminal status=success, artifact digest=..."
}
```

- `completed` 必须提供非空 `finalText`，且不能提供 `error`；
- `failed`/`cancelled` 必须提供非空 `error`；
- `authority` 与 `evidence` 都不能为空；
- 工具为 `ToolReplayPolicy::Never`，绝不重放原操作；
- reconciliation 与 uncertain report 替换在一个事务中完成；
- `nextStep` 产生一份替换报告，`quiet` 不唤醒 parent。

## 8. Step 上限

默认没有隐式 100-step 上限。Agent 未配置 `steps` 时可以继续工具循环，直到正常完成、被中断或发生其他 typed terminal failure。

显式配置示例：

```json
{
  "agents": {
    "build": {
      "steps": 24
    }
  }
}
```

`steps: N` 表示最多 N 个 tool-capable provider steps。达到上限后，Zuno：

1. 关闭工具列表；
2. 注入 host-owned finalization instruction；
3. 额外执行一次 text-only provider request；
4. 要求总结已完成、未完成、证据和阻塞；
5. 在 `session.provider.request.1.stepLimitFinalization` 记录上限、指令和指令 digest。

这不是继续执行机会，也不会给模型再开放工具。

## 9. Compaction 与 durable state

Compaction 只改变后续 provider request 使用的 transcript boundary，不删除以下持久状态：

- Goal；
- Plan；
- Todo/WorkItem；
- Agent Job；
- parent inbox 和未消费 report；
- session event log；
- prompt receipts；
- selected Skill provenance。

当前恢复路径：

- Goal 每次从 SQLite 重新生成 model-visible dynamic context；
- mounted host 保留 selected Skill，host 重开时从最新 prompt receipt 恢复 Skill 正文；
- Plan、Todo、active/uncertain Job、未消费报告和上一份 prompt receipt id 每轮从
  SQLite 生成有界 `runtime.work_state` developer instruction；
- pending report 仍作为 durable inbox input 投递；
- prompt receipt 仍可通过 debug 命令查询。

`runtime.work_state` 在一个 deferred SQLite transaction 中读取所有相关表，避免
Plan、Todo、Job、Inbox 和 receipt 来自不同时间点。各集合最多投影 64 项，完整
section 最多 16 KiB。超限时先 UTF-8 安全截断描述和最终文本，再从尾部省略
Todo、Plan step、pending report 与 Job，并记录 omitted count；稳定 identity、
状态、revision 和 reconciliation 字段优先保留。若权威 identity 本身仍无法放入
预算，组装会失败关闭。typed tools 仍是进一步查询和修改这些 Store 的唯一入口。

数据库当前格式为 9。format 5 会在一个 `BEGIN IMMEDIATE` 事务中
增加 learning 表、Plan 栈字段、archive 表、`verification_receipt` 账本与
`session_memory_policy`；format 6 增加 Plan 栈、账本与 policy；format 7 增加账本与
policy；format 8 只增加 policy。四条路径都在最后以条件更新将 marker
改为 9，并保留已有的 `session`、`message`、`memory_candidate` 与 `work_plan` 值。未来格式、缺少 marker 或结构损坏会在不修改
数据库的情况下失败关闭；当前二进制支持的旧格式不要求重建数据库。

## 10. 诊断命令

### 10.1 `zuno debug prompt`

```sh
# 数据库中最新的一份 receipt
zuno debug prompt

# 某会话最新 provider request 实际引用的 receipt
zuno debug prompt --session ses_...

# 某会话第 N 个 provider step 引用的精确 receipt
zuno debug prompt --session ses_... --step N
```

`--step` 必须为正数且必须与 `--session` 同时使用。查询链为：

```text
session.provider.request.1
→ promptReceiptID
→ session.prompt.assembled.1
```

输出包含 section id、order、source、role、trust、priority、bytes、estimatedTokens、SHA-256、assembly/actual digest、hook 状态和 provider projection。

默认会把 section content、system/developer projection 和 post-hook system prompt 替换为 `<redacted>`。但 source path、session/event id、Agent 名、大小和 digest 仍可暴露项目结构。`--show-sensitive` 会显示完整 AGENTS、Skill、memory、runtime instruction 和 hook-transformed prompt；不要直接粘贴到公开 issue、CI log 或聊天。

### 10.2 `zuno debug agent`

```sh
zuno debug agent deep
```

输出当前目录和有效配置下的：

- Agent 名、mode、source、description 和显式 step limit；
- effective provider/model、reasoning support 和 reasoning resolution inputs；
- policy-visible 与 unavailable tools，以及原因；
- parent tool authority；
- MCP server 的实时生命周期、discovery、连接结果、精确 tool schema 和清理结果；
- required Skills，以及 available Skill 的数量、歧义、预算、覆盖率和有界预览；
- delegates；
- Sandbox mode、backend readiness、network/workspace policy；
- policy sources 和 resolution notes。

该命令不会创建 session 或连接 provider，但会复用正式 `McpRuntime` 连接所有
enabled server，获取真实 tool ids/schema，逐个与角色权限、Agent allowlist 和
parent Attempt authority 取交集，然后等待 transport 清理完成。连接、授权、初始化
或清理失败都会进入结构化诊断，而不是被伪装成可用。它也会执行本地只读 Sandbox
readiness 检查。

当前限制：输出尚未包含设计目标中的 capability snapshot identity、extension revision、Prompt policy digest、完整 permission rule 展开和历史 request 的最终工具 schema。它是 configuration-time diagnostic，不是某个历史 provider request 的法证快照。

## 11. 四类真实 E2E

以下矩阵已分别使用真实授权的 `kiro-local/gpt-5.6-sol` 与
`kiro-local/claude-opus-5` 执行。模型 id 以本机 `zuno models` 实际输出为准；
后续复测若 provider、账号或模型不可用，应记录阻塞，不使用 mock 冒充 E2E。

```sh
zuno models kiro-local --verbose
```

每次运行使用独立测试仓库或干净 worktree，并记录 JSON 输出：

```sh
zuno run \
  --format json \
  --model "$MODEL" \
  --agent "$AGENT" \
  --title "$TITLE" \
  "$PROMPT"
```

从输出或以下命令取得 session id：

```sh
zuno session list --format json --limit 20
```

### 11.1 原子实现

- Agent：`build`。
- Fixture：一个局部、确定、可测试的小修复。
- Prompt 明确只需完成该修复和一个定向测试。
- 预期：不调用 `task`，不创建 durable Plan，不重复读取同一未变化文件，不重复运行相同 Git 检查。

### 11.2 深度 Debug

- Agent：`deep`。
- Fixture：可稳定复现的失败，包含至少两个可证伪假设和一个恢复路径。
- 预期：记录复现、假设排序、因果链、owning abstraction、根因修复和原失败/恢复验证；不递归委派。

### 11.3 并行委派

- Agent：`orchestrator`。
- Prompt：要求两个只读、scope 不重叠的调查 lane，随后由 parent 集成。
- 预期：每个 child 有完整 `DelegationContract`；产生两个独立 child session/Job；每份 `nextStep` report 只 admission 一次；parent 不轮询、不重复派发，消费报告后再完成。

### 11.4 Plan-only

- Agent：`plan`。
- Prompt：一个跨组件任务，只要求 decision-complete plan。
- 预期：只读检查，维护 durable Plan，包含事实、决策、接口、步骤、失败/恢复和验收；不编辑产品文件，不进入 Work。

### 11.5 证据采集

检查实际 Prompt：

```sh
zuno debug prompt --session "$SESSION_ID"
zuno debug prompt --session "$SESSION_ID" --step 1
```

检查 provider request 数：

```sh
zuno db --format json \
  "SELECT COUNT(*) AS provider_calls
   FROM event
   WHERE aggregate_id = '$SESSION_ID'
     AND type = 'session.provider.request.1'
     AND json_extract(data, '$.status') = 'started'"
```

检查 native tool 次数和名称：

```sh
zuno db --format json \
  "SELECT json_extract(data, '$.tool') AS tool, COUNT(*) AS calls
   FROM part
   WHERE session_id = '$SESSION_ID'
     AND json_extract(data, '$.type') = 'tool'
   GROUP BY tool
   ORDER BY tool"
```

检查 child Job 和 report：

```sh
zuno db --format json \
  "SELECT id, status, report_delivery, report_input_id, result
   FROM agent_job
   WHERE parent_session_id = '$SESSION_ID'
   ORDER BY time_created, id"
```

检查 Plan/Todo：

```sh
zuno db --format json \
  "SELECT session_id, revision, title, steps FROM work_plan
   WHERE session_id = '$SESSION_ID'"

zuno db --format json \
  "SELECT id, status, owner, dependencies FROM work_item
   WHERE session_id = '$SESSION_ID'
   ORDER BY time_created, id"
```

最终报告至少记录：

- provider calls；
- 各工具调用次数；
- 重复读取或重复检查；
- Plan 更新；
- child Job、report admission 和恢复/wake 证据；
- prompt receipt id 和 section digest；
- 真实测试或行为证据；
- 未验证项与剩余风险。

### 11.6 2026-08-28 真实结果

| 模型/场景 | Provider calls | 关键结果 |
| --- | ---: | --- |
| GPT 原子 | 1 | 无工具、无 Plan、无委派，精确返回预期文本。 |
| Opus 原子 | 1 | 无工具、无 Plan、无委派，精确返回预期文本。 |
| GPT Deep | 12 | 复现整数除法缺陷，修改一次，原测试和边界测试通过。 |
| Opus Deep | 10 | 完成同一缺陷的复现、根因修复和恢复验证。 |
| GPT 并行委派 | 9 | 两个不重叠 child；两份 `nextStep` 报告各 admission 一次。 |
| Opus 并行委派 | 3 | 两个 child 并行完成；parent 只消费报告并集成。 |
| GPT Plan-only | 6 | 只读取两个授权文件；7 步 Plan、7 个显式 ID Todo。 |
| Opus Plan-only | 8 | 只匹配并读取两个授权文件；9 步 Plan、10 个显式 ID Todo。 |

## 12. 当前实现边界

截至本指南对应代码状态，以下边界仍需在验收报告中明确：

1. 宿主分类器只判断是否需要 Plan，模型负责创建或维护战略步骤；模型仍可能把 Plan
   或 Todo 拆得过细，因此
   GPT/Opus 的 provider call 成本仍应通过真实遥测持续观察。
2. `debug agent` 现在会主动连接当前配置的 MCP，并检查实时 schema；历史 request
   当时真正发送的最终 schema 仍应从对应 provider request/receipt 取证，不能用
   当前服务器状态代替历史证据。
3. `TaskReportMetadata.changedPaths` 和 `verificationRecords` 只接受 typed tool
   metadata。未提供这些 metadata 的自定义工具或原生 Shell side effect 不会被
   猜测为已记录证据。
4. `zuno run --variant <name>` 会选择模型声明的精确 variant；canonical
   `low|medium|high|xhigh|max` 会在 provider I/O 前校验。`--thinking` 自动选择
   可用的 `high`，否则选择最强的非 `off` 等级。两者互斥；需要精确 `max`/`xhigh`
   时应使用 `--variant`。
