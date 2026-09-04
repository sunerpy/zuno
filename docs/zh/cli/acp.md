# zuno acp

`zuno acp` 在 stdin 与 stdout 上说 Agent Client Protocol。支持 ACP 的编辑器会把这个可执行
文件作为子进程启动，并在管道上交换带帧的消息，因此没有端口需要绑定，也没有 HTTP 面需要
加固。这是 Zed 以及其他 ACP 客户端的集成路径。

由于协议占用了 stdout，不要把那条流当作人类可读输出来读。只想确认适配器存在时用 `--check`，
用 `--print-logs` 把诊断信息路由到 stderr，那里不会破坏协议流。

编辑器只启动并持有一个进程。那个进程就是提供协议服务的进程，因此终止它就结束该会话，它的
管道也随之到达 EOF。参见[一次调用就是一个进程](/zh/cli/#一次调用就是一个进程)。

## Agent、Mode 与 Plan 投影

Agent selector 会显示 `plan`。`active_agent` 是唯一状态源：选择 `plan` 会自动切换到
Plan mode；选择 `build`、`orchestrator`、`deep` 或其他实现 Agent 会自动回到 Build
mode。反向切换 Mode 也会选择对应 Agent，并同时发送 `current_mode_update` 与
`config_option_update`，避免 Zed 的两个 selector 漂移。

空闲状态下切换 Agent、模型、Mode 或推理等级时，Zuno 仍会原子替换 turn host，
但如果解析后的 MCP server 集合与连接并发度没有变化，会保留该会话已经连接的 MCP
runtime，避免重复网络或子进程握手。结构性 MCP 配置发生变化时仍会重新连接。重配置
日志会记录锁等待、解析、关闭、打开和总耗时，但不会记录所选值或凭据。

会话已经在跑一个回合时到达的 `session/prompt`，会先被提交进持久输入 inbox，再被转向
进那个回合，因此模型能收到它，而正在进行的工作不会被打断。第二个请求以 JSON-RPC 错误
`-32001` 回答，其 `data` 报告 `admission`（`steered`、`queued` 或 `rejected`）、
`sessionId` 与持久的 `inputId`；流式输出与 `stopReason` 仍留在拥有该回合的那个请求上。
斜杠命令无法被转向，会以 `reason: "commandRequiresIdleSession"` 被拒绝，且不写入任何
持久内容；只有能解析到真实命令、Skill 或原生控制项的文本才算斜杠命令，因此仅以 `/`
开头的提示词会作为普通内容被接纳。在一个 prompt 请求返回之前用 `$/cancel_request`
撤回它，会取消该请求接纳的那条持久行，使被撤回的文本永不到达模型，并以 `-32800`
与 `data.admission: "withdrawn"` 回答该请求。完整形态见
[Zed ACP 集成](/zh/guide/editors)。

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

运维通知——无法抓取的远程规则文件（其规则本轮不生效，回合继续）、被 token、工具调用次数或墙上时间额度停下的回合、
预算策略要求的一次压缩——以带 `_meta.zuno.notice` 标记的 `agent_thought_chunk` 投影，
其中 `severity` 取 `info`、`warning` 或 `error`，`code` 是稳定的机器可读码，例如
`instruction.not_in_force`、`budget.compact`、`budget.token_budget`。客户端靠这个标记
把它们与模型输出区分开；它们永远不进入模型看到的对话记录。

运行中的 ACP 会话会订阅统一 Skill catalog generation。新增、修改、删除或重命名
Skill 后，会发送新的 `available_commands_update`，无需重启会话。

## Goal 续跑

`/goal <目标>` 是“原生控制 + 自主执行”，不是只返回一行 Goal JSON。Zuno 先持久化
类型化命令结果，随后立即通过共享 driver 推进 active Goal。新会话会把目标经由持久
inbox 准入为首个 user turn；字面的斜杠命令不会发送给 provider。

`session/load` 与 `session/resume` 会重建会话运行时并自动恢复 active 根 Goal，不需要
额外发送一条提示词。对于 0.6.0 已写入 active Goal、但没有 user message 的会话，同一
恢复路径会先补齐 durable user anchor 再续跑。

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
| `--sandbox-backend <BACKEND>` | 为本次调用选择 Shell 执行后端；`native` 不是沙箱隔离。可选值：`auto`、`native` | `auto` |
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
