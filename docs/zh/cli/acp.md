# zuno acp

`zuno acp` 在 stdin 与 stdout 上说 Agent Client Protocol。支持 ACP 的编辑器会把这个可执行
文件作为子进程启动，并在管道上交换带帧的消息，因此没有端口需要绑定，也没有 HTTP 面需要
加固。这是 Zed 以及其他 ACP 客户端的集成路径。

由于协议占用了 stdout，不要把那条流当作人类可读输出来读。只想确认适配器存在时用 `--check`，
用 `--print-logs` 把诊断信息路由到 stderr，那里不会破坏协议流。

编辑器只启动并持有一个进程。那个进程就是提供协议服务的进程，因此终止它就结束该会话，它的
管道也随之到达 EOF。参见[一次调用就是一个进程](/zh/cli/#一次调用就是一个进程)。

在 Agent Panel 中打开 Zuno 会调用 ACP `session/new`，先保留进程内 session id 并解析
配置、命令、Skill 与 MCP。普通空面板仍然是临时的；但进入 Plan 这类持久原生协作控制会
物化 Session，因为 mode、Goal pause 与未来 Work identity 必须能够跨重启恢复。
普通空面板不会创建持久 Session，也不会让空的「New Agent Thread」出现在列表中；关闭从未
发送消息且没有执行持久控制的面板不会留下历史行。
换句话说，关闭从未发送消息的面板不会留下历史行；执行过 Plan 等持久控制的面板已经不再是
空面板。
普通空面板不会创建持久 Session，也不会让空的「New Agent Thread」出现在列表中；关闭从未
发送消息且没有执行持久控制的面板不会留下历史行。

## Agent、Mode 与 Plan 投影

Mode 与 Agent 是两份独立的持久选择。Mode 拥有 Plan/Work 边界；Agent selector 只显示
实现 Agent。Plan 活跃时切换 Agent 只更新未来 Work Agent，不会离开只读 `plan` host；
模型与 reasoning 选择也会更新同一份未来 Work identity。

`/plan` 与 `/start-plan` 都是幂等进入 Plan。`/start-work` 与
`session/set_mode(build)` 调用同一个原子授权：当前精确 Plan revision 必须
handoff-ready；绑定 review 时默认要求 Ready，除非用户通过
`--accept-draft-risk <原因>` 显式接受；随后恢复保存的 Agent/provider/model/reasoning。
空闲会话立即开始，忙碌会话把控制持久排队到下一个安全点。

空闲状态下切换 Agent、模型、Mode 或推理等级时，Zuno 仍会原子替换 turn host，
但如果解析后的 MCP server 集合与连接并发度没有变化，会保留该会话已经连接的 MCP
runtime，避免重复网络或子进程握手。结构性 MCP 配置发生变化时仍会重新连接。重配置
日志会记录锁等待、解析、关闭、打开和总耗时，但不会记录所选值或凭据。

忙碌时到达的 `session/prompt` 先进入持久 inbox，再于安全点 steer 或保留排队。
该 RPC 等待这条输入的关联处理结果，正常完成时返回合法 `stopReason`，不再用 `-32001` busy
表示接收成功。执行由会话持有，不依赖某个 RPC 观察者存活；`_meta.zuno.receipt`
区分已接收、已写入历史、已进入模型与执行终态。

普通 prompt 可提供 `_meta.zuno.messageId`（1–256 字节）。同一会话重试同一 ID
只观察原回执，内容冲突则拒绝；文本相同但 ID 不同仍是两条用户输入。

支持 Zuno 扩展的客户端可以改用 `session/steer`。初始化响应通过
`_meta.zuno.steering` 宣告能力；回合内 `session/update` 通过
`_meta.zuno.turnId` 给出目标回合。请求必须把该值作为 `expectedTurnId`，
成功时立即返回 `turnId`、`inputId`、`admittedSequence`、
`admission: "steered"` 与 `delivery: "steer"`。拒绝使用 `-32002`，
`reason` 为 `noActiveTurn`、`expectedTurnMismatch`、
`activeTurnNotSteerable` 或 `emptyInput`。目标回合会在 inbox 事务提交前再次校验；
如果等待 SQLite 期间原回合结束或已被替换，输入行与准入事件一起回滚，被拒绝的 steer
不会进入后续回合。Stop 使用 `session/cancel`；后续输入使用 prompt 准入或
`session/steer`，不能通过取消后重发来模拟 steer。

斜杠命令无法被转向，会以 `reason: "commandRequiresIdleSession"` 被拒绝，且不写入任何
持久内容；只有能解析到真实命令、Skill 或原生控制项的文本才算斜杠命令，因此仅以 `/`
开头的提示词会作为普通内容被接纳。`$/cancel_request` 只撤回该请求贡献的输入；
如果它选中的原生输入已经运行，实际发出取消时仍会核对输入身份。取消不能抹掉已处理
历史，也不能由重复 ID 的观察者撤回原输入。
断线不等于撤回：已接收输入与处理回执继续持久保留。完整形态见
[Zed ACP 集成](/zh/guide/editors)。

后台完成不是斜杠命令。终态命令、子 Agent、workflow 与 product Agent 会发布确定性的
completion envelope 并主动唤醒父会话。同步 `bg wait` 最长 60 秒，并与 callback 竞争
唯一 durable owner，避免重复 turn。

Plan 投影由 durable work-state revision 驱动，不再依赖识别 `plan_update` 工具调用。
每个会话订阅当前 host，发生变化后读取权威 Plan 并发送完整的
`sessionUpdate: "plan"`。`(plan_id, revision)` 会抑制重复或过期更新；连续快速提交
可以合并到最新 revision，但 prompt 返回前必须 flush 最终状态。Plan 被移除时发送空
entries 清除 Zed 旧面板。load、resume、detached Goal continuation 与 host 重建都复用
同一个投影器。ACP 没有 `superseded` 状态，因此 wire 上映射为 `completed`，真实语义
保存在 `_meta.zuno.outcome: "superseded"`。每个非空的 Plan 快照还携带 `_meta.zuno.planId`、
`revision`、`title` 与 `stackDepth`；Plan 绑定 Goal 时附带 `goalId`，可见的是聚焦子 Plan 时附带
`parentPlanId`，客户端无需比对 entries 就能区分推入的子 Plan 与被替换的根 Plan；每个 entry 带
`_meta.zuno.stepId`，清空更新只携带 `_meta.zuno.cleared: true`。

`edit`、`write` 与 `apply_patch` 统一投影为 `Editing files` 卡片。成功且存在结构化
diff 时，可见内容只保留 `A/M/D <path>`，不再重复显示成功文案；完整原始输出仍在
`rawOutput`。写入前失败展示可操作错误而不伪造 diff；部分写入或其他不确定结果使用
failed 状态，保留已观察到的路径/diff，并设置 `_meta.zuno.outcome: "uncertain"`。
实时更新与历史 replay 使用同一策略。

运维通知——无法抓取的远程规则文件，或因装不进 prompt 预算而整份跳过的完整本地规则文件
（其规则本轮不生效，回合继续）、被 token、工具调用次数或墙上时间额度停下的回合，以及
预算或上下文策略要求的一次压缩——以带
`_meta.zuno.notice` 标记的 `agent_thought_chunk` 投影，
其中 `severity` 取 `info`、`warning` 或 `error`，`code` 是稳定的机器可读码，例如
`instruction.not_in_force`、`budget.compact`、`budget.token_budget`、`context.compact`。
客户端靠这个标记把它们与模型输出区分开；它们永远不进入模型看到的对话记录。
内部历史回放诊断只写结构化日志，不会投影成 thought chunk。

压缩成功后，ACP 会收到完全相同的持久摘要：它以 `agent_message_chunk` 投影，并带
`_meta.zuno.kind: "compaction_summary"`。回合中的自动压缩会在同一次 host drive 内、
重试之前发送该摘要，因此编辑器不会先收到终端 prompt 失败，也不必等待下一次 wake。
历史 load/resume replay 的是同一份摘要和同一标记。

ACP `usage_update` 使用原生 `ContextUsageSnapshot`：最近供应商确认基线，加尚未计入内容的估算。
较小的粗估值不能覆盖确认基线；分批 usage 按快照合并，压缩通过新 epoch 表达。
`_meta.zuno.contextUsage` 携带来源、请求标识、freshness 和更新时间。
累计非重叠用量与当前窗口分开，未知值保持未知。

运行中的 ACP 会话会订阅统一 Skill catalog generation。新增、修改、删除或重命名
Skill 后，会发送新的 `available_commands_update`，无需重启会话。

## 精确取消与旧客户端

初始化响应通过 `_meta.zuno.cancellation` 宣告 `version: 1`、
`method: "session/cancel"`、`expectedTurnIdPath: "_meta.zuno.expectedTurnId"`、
`legacySessionIdOnly: "currentTargetAtDispatch"` 与 `armsNextTurn: false`。
客户端从实时 `session/update` 的 `params.update._meta.zuno.turnId` 读取回合 ID：

```json
{
  "jsonrpc": "2.0",
  "method": "session/cancel",
  "params": {
    "sessionId": "ses_example",
    "_meta": { "zuno": { "expectedTurnId": "turn_example" } }
  }
}
```

目标 ID 必须是非空字符串，最多 256 字节。Zuno 在同一把原生锁内校验目标并发出取消，
因此迟到的 T1 取消不会中断 T2。目标不存在或已结束、ID 不匹配、exact metadata 格式
非法时，都不会降级为取消当前回合。这是 notification，不产生 JSON-RPC 响应；
拒绝信息写到 stderr。实际执行结果以原 prompt 的持久回执和更新为准。

旧客户端只发送 `sessionId` 时，Zuno 在处理通知时绑定一次当前执行目标。空闲时取消
没有效果，也不会给未来回合预置取消。协议没有提供识别网络迟到意图的信息：原本想取消
T1 的 session-only 通知如果在 T2 运行时到达，可能取消 T2。需要精确目标的客户端必须
使用上述扩展。

`$/cancel_request` 通过 `requestId` 标识原客户端 RPC，字符串与数字 ID 分开处理。
响应后复用 wire ID 会获得新的内部请求身份；使用 `_meta.zuno.messageId` 幂等重试时，
仍只观察原持久输入。撤回不能取消无关的 Agent 到客户端 RPC。`-32800` 表示请求撤回，
不表示工具副作用已回滚；应通过持久回执观察执行结果。

## 已保存输入与执行门禁

输入可以已经消费并写入历史，但原生执行仍被门禁阻止。此时 `InputAdmissionReceipt`
保持 `recorded`，附带可选 `executionGate`，`appliedAt`、`completedAt`、`turnId`
均缺省。门禁不会把这条已保存但未应用的输入改为 `failed`，也不证明模型已开始采样。

这种情况下，`session/prompt` 返回 JSON-RPC error `-32005`，其 `error.data` 包含
`admission: "accepted"`、`reason: "executionGated"`、`recoveryRequired: true`
和权威 `receipt`。消息已经保存，不要作为新输入重发。重连后以相同
`_meta.zuno.messageId` 重试，只会观察原回执。

`executionGate` 包含：

| 字段 | 含义 |
| --- | --- |
| `reason` | `user`、`authentication`、`turn_budget`、`uncertain_side_effect`、`blocked`、`waiting_human`、`waiting_external`、`no_progress`、`no_executable_work` 或 `execution_unavailable` |
| `recovery` | `resume_work`、`resume_goal`、`start_work`、`resolve_human_request`、`wait_for_event`、`reauthenticate`、`inspect_outcome`、`review_budget` 或 `inspect_session` |
| `executionRevision`、`cycleId` | 作出门禁决定时的原生执行 revision 与周期 |
| `requestId`、`sourceId` | 可选的待答人工请求或所等待外部来源的身份 |

恢复值只是提示，不授予权限，也不承诺一条命令即可解除门禁。`start_work` 指向 Start Work
的 Plan 授权，`/resume` 不能代替。普通 `/resume` 必须先通过
既有 Work、Plan、Goal、等待、认证、预算、blocked 状态及未知副作用审计，随后才把匹配的
gated 且未应用 anchor 绑定到新周期，不会重复插入原文。直到真实 turn 绑定这条输入前，
重复观察者仍可收到相同 gate；之后应用与完成按正常回执生命周期推进。既有 `failed`、
`cancelled`、`applied`、`completed` 回执不会重置。

通过 `session/prompt` 提交的普通 `/resume` 在通过这些检查后，由该 RPC 观察原生恢复
控制的持久回执。控制入队不会提前返回 `stopReason: "end_turn"`：请求会保持待决，
直到关联的原生执行完成、等待人工输入、被取消或失败。恢复执行期间，客户端可以保持
面板 busy 和 Stop 可用。RPC 观察者丢失不会取消会话持有的执行；
这仅指单个观察者，不包括整个 ACP 连接、运行时或进程关闭。连接 EOF 最多等待 25 ms
以排空已就绪的请求，随后运行时关闭会取消正在运行的恢复控制；其数据和 `cancelled`
回执继续持久保留，`session/load` 不会重放该控制。
显式 `$/cancel_request` 只撤回该请求贡献的控制输入，`session/cancel` 仍遵循上文的
精确回合或旧客户端目标规则。

这不改变普通 Stop 的边界：下一条新消息可正常运行。中断 Goal 仍需显式 Goal 恢复控制，
旧周期 callback 也不能复活已停止的工作。

自动识别旧普通停止要求没有失败桥接，且原始周期、原生事件及时间证据充分。
既有 v0.10.32 failed 桥接缺少完整的前序暂停来源；这些桥接和未知暂停仍保留门禁。
普通 Work 恢复需要显式 `/resume`，并通过上述全部审计，不会重开旧 `failed` 回执。
上文的重连与恢复行为适用于本补丁中回执保持 `recorded` 的新 gated 输入。

## Goal 续跑

`/goal <目标>` 是“原生控制 + 自主执行”，不是只返回一行 Goal JSON。Zuno 先持久化
类型化命令结果，随后立即通过共享 driver 推进 active Goal。新会话会把目标经由持久
inbox 准入为首个 user turn；字面的斜杠命令不会发送给 provider。

自动 Goal 回合使用当前 ACP host 已选择的 Agent 与模型。最新的真实 user message 只保留为
因果 transcript anchor，不授予权限。从 `deep` 切换到 `orchestrator` 或选择另一模型后，
不需要再发送普通 prompt，也不会改写那条历史消息。确切的触发类型、Goal revision、anchor、
Agent、provider 与模型会落盘到 `session.turn.started.1`。

`session/load` 与 `session/resume` 会重建会话运行时并自动恢复 active 根 Goal，不需要
额外发送一条提示词。对于 0.6.0 已写入 active Goal、但没有 user message 的会话，同一
恢复路径会先补齐 durable user anchor 再续跑；压缩后保留历史从 assistant 消息开始的
会话也使用同一修复。

`/goal budget <正整数 token|none>` 修改单个 Goal 的显式上限。`goal_update` 的
`in_progress` 与 `active` 仅用于幂等确认一个已经 active 的 Goal；paused 或 blocked Goal
仍必须由用户执行 `/goal resume`，或明确选择原生 **Resume goal / Keep paused** 中的恢复。
中断继续保持暂停；新普通输入、跳过选择或记录后台报告都不构成恢复授权。
事务校验 Goal ID/revision，并保留 Plan、审批、认证、未知副作用和预算门禁；
已经处理的用户输入不会重投。

## 进程环境与代理

编辑器在 custom Agent `env` 中设置的 `HTTP_PROXY`、`HTTPS_PROXY`、`ALL_PROXY` 与
`NO_PROXY` 属于 `zuno acp` 进程环境，因此会统一作用于 provider、OAuth、远端 MCP、
远端目录、`webfetch` 与 `web_search` 等普通会话请求。某个 ACP stdio MCP 声明里的
`env` 只属于那个子进程，不会改写 Zuno 或其他会话流量。

`webfetch` 仍会对每个目标和重定向在本地解析、校验全部 IP，再通过选中的代理连接已校验
IP，并保留原始 Host/TLS SNI。代理失败不会静默直连；只有 `NO_PROXY` 可以为匹配目标
选择环境级直连。

## 会话级 MCP server

Zuno 公布标准 ACP MCP 的 stdio 与 Streamable HTTP 支持；旧式 SSE 仍不支持。`session/new`、`session/load` 与 `session/resume` 必须为该会话提供完整的 `mcpServers` 列表。load/resume 绝不会复用上一次请求留下的进程资源。

声明会在会话发布之前完整校验：

- 名称必须满足 `[A-Za-z0-9_-]{1,32}`，否则稳定 slug 化并追加 8 位 digest；规范化后重名会被拒绝；
- stdio command 必须是绝对路径，并以会话目录作为 cwd；
- HTTP endpoint 必须是绝对 HTTP(S) URL；
- environment 与 header 条目严格校验，包括 header 名称大小写无关的重复项。

每个 ACP session 拥有隔离的 profile bundle。全部 server 都必须连接并完成工具发现，工具才会原子发布；部分启动按逆序关闭。session close、load 失败、进程退出与 profile replacement 使用同一条精确 disposer 路径。

客户端 MCP command、environment 值与 HTTP header 只存在于进程内，不写入会话数据库或诊断。工具 schema 与真实工具 attempt 仍遵循普通的持久工具规则。

## 用法

```sh
zuno acp [OPTIONS]
```

## 选项

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `--check` | 验证生产 ACP 适配器可用，然后退出 | |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | 选择受限 Shell 无法部署时的处理方式。可选值：`deny`、`run-unconfined` | `deny` |
| `--sandbox-backend <BACKEND>` | 为本次调用选择 Shell 执行后端；`native` 不是沙箱隔离。可选值：`auto`、`native` | 随平台解析 |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

## 示例

确认这份构建中生产 ACP 适配器可用，然后退出。

```sh
zuno acp --check
```

在 stdin 与 stdout 上提供协议服务，也就是编辑器启动它的方式。

```sh
zuno acp
```

一边提供协议服务，一边把诊断信息镜像到 stderr，使 stdout 上的协议分帧保持完整。

```sh
zuno acp --print-logs --log-level DEBUG
```

## 参见

- [全局选项](/zh/cli/global-options)
- [zuno serve](/zh/cli/serve)
- [Zed ACP 集成](/zh/guide/editors)
- [日志](/zh/operate/logging)
