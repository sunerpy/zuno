# 会话与回合

会话（session）是一个由 SQLite 支撑的持久工作单元。回合（turn）是该会话内穿过 Agent 循环的一次执行。两者都是带持久状态的运行时概念，而不是展示细节，这正是为什么一个 Zuno 会话在创建它的进程消失之后，仍然可以被续跑、重放和检查。

## 模型

| 概念 | 它是什么 | 存在于何处 |
| --- | --- | --- |
| Session | 一段持久的对话及其工作状态 | 一条 `session` 行加上它的事件 |
| Turn | 一次用户输入被推进到终态 | 回合作用域的运行时状态加上持久事件 |
| Event | 任何模型可见的东西：提示词、工具结果、报告、重试 | 会话事件日志 |
| Inbox | 已准入但尚未提升的输入的 FIFO 队列 | 持久的 inbox 行 |
| Prompt receipt | 一次 provider 请求所对应的确切组装提示词 | 带分段 id 与摘要的持久收据 |

子会话是真实的会话。委派会为每个子级创建一个，拥有自己的事件、用量和血缘。`zuno session list` 默认隐藏它们；`--no-roots` 会包含它们。

## 惰性物化

一个交互式新会话在准备时不插入任何行。它的进程本地身份在模型、Agent、MCP 和主题变更之间保持稳定，但打开、浏览或离开欢迎界面都不会创建任何持久内容。第一次面向模型的提交会在一个事务中插入该会话及其用户消息。

终端应用中的 `/new` 会在同一次激活中选择另一个已准备好的会话。它直接打开一个空的对话外壳，并不绕过这条边界。

## 持久输入

每一个模型可见的外部输入，都会在尝试执行*之前*，于同一个 SQLite 事务中被准入到事件日志和持久 inbox。跨活跃回合、空闲会话、重启和相互竞争的 driver，真相来源是 inbox，而不是某个进程内通道。

Driver 按 FIFO 顺序提升输入。提升是事务性的，并且可以针对某一个输入标识符，用于实现一次实时软中断。格式错误的输入会记录一条会话错误，不会让后续队列项被搁死。

用户提示词与子 Agent 报告共用同一套协议：

- 活跃的父级收到一次软中断，并在下一个工具安全点提升该报告。
- 如果报告错过了最后一个安全点，唤醒协调器会等活跃 lease 结束，然后在该输入仍处于待处理状态时启动另一个回合。
- 空闲的父级会被立即认领并驱动。
- 重启后的进程会从持久 inbox 恢复待处理的报告。

这就是为什么带 `reportDelivery: nextStep` 的后台委派不会因轮询竞态而丢掉报告：结算、准入和唤醒是一个事务序列。

根会话之间的 peer message 使用同一条持久准入边界。仅根会话可见的
`session_message` 工具会向目标 inbox 写入类型化 `sessionMessage`。它可以发送给同一项目
中的另一个根会话，或当前 root 的后代；child 不能发送，一个 root 也不能直接控制另一个
root 的 child。渲染文本会标出来源 session、Agent 与标题，并明确 peer context 不是用户
授权。

TUI、HTTP server 与 ACP driver 都把这种输入识别为模型可见文本。活跃目标在安全点收到软
输入；空闲且有常驻运行时的目标启动一个回合；离线目标则保留 queued 行，直到再次加载。

对于通过 SSH 使用的 TUI，`zuno tui --background` 提供这个常驻所有者。只绑定 loopback 的
supervisor 会保留 PTY、TUI 进程、scrollback 与活跃回合，而每个 SSH 客户端只是
attachment。`Ctrl+]` 用于 detach；`--background-shutdown` 是停止全部 retained child 的
显式生命周期边界。

## 提示词溯源

提示词组装使用稳定的分段标识符、确切来源、有序内容和内容摘要。Hook 之后的提示词在 provider 请求之前落盘，因此模型看到了什么是一个持久事实，而不是事后重构。

```sh
zuno debug prompt --session ses_1a2b3c --step 2
zuno debug prompt --session ses_1a2b3c --step 2 --show-sensitive
```

`--show-sensitive` 会原样打印指令、AGENTS、skill 和记忆内容。请把它当作敏感输出对待。

## 压缩

当历史接近模型窗口时，Zuno 会压缩较早的对话，而不是让请求失败。

```json
{
  "compaction": {
    "auto": true,
    "threshold_percent": 80,
    "tail_turns": 2,
    "reserved": 12000
  }
}
```

`threshold_percent` 接受 `1..=100`，默认 `80`，作用于扣除模型输出配额与配置预留之后的可用窗口。`auto: false` 会关闭主动压缩，同时仍保留 `/compact` 可用。provider 确认的上下文超限失败仍然走有界恢复路径，那是对一次已经失败的请求做恢复，而不是主动阈值。

压缩改变的是 provider 对话边界。它不会删除持久的 Goal、Plan、Todo、Job、inbox、事件日志或提示词收据。这些会在下一次相关请求时从 SQLite 重新生成，包括一个有界的 `runtime.work_state` 分段 —— 每个集合上限 64 条，整体上限 16 KiB。

历史图像的字节不会进入压缩请求。取而代之的是一个稳定标签，例如 `[Attached diagram.png (image/png)]`，而原始的持久文件部分保持不动，以便做权威重放。

## 可选连续性

完整的配置示例与切换规则见
[History 与 Notes 连续性配置](/zh/config/continuity)。

启用 `continuity.history` 后，模型可以按成功压缩窗口检查当前会话的规范化证据。该工具
不会跨越 session 边界，也不会返回 reasoning、加密内容、合成内部提示正文或二进制
附件字节。

启用 `continuity.notes` 后，逻辑工作文档按 `session_id + Agent` 存储。它们属于 session
生命周期而不是宿主文件系统：删除 session 时级联删除，prune 会计数并移除，session
export/import 会保留 Notes 及其幂等 ledger。sanitize export 会脱敏文档身份与正文，
并移除该 ledger。

## 中断

硬中断是会话作用域的，并且在回合交接过程中是可线性化的。如果上一次运行的守卫已经释放，但一个已准入的后续输入还没获得自己的守卫，注册表会为下一个守卫布防，而不是丢弃这次中断：新回合以中断信号已置位的状态启动，发出终态中断事件，并且不发出任何 provider 请求。

确认之后，界面会保持停止状态可见，并抑制迟到的 provider 或工具输出，直到一个终态事件确立边界。持久化仍然照常执行。在取消之前已经完成的副作用仍然是一个已观测到的结果，绝不会被机械重放。

## 重试与恢复

可恢复的 provider、网络、流、SQLite 争用、Agent 步数上限以及符合条件的工具失败，会在等待之前先把一次指数退避重试落盘，因此进程重启后可以从 SQLite 重建截止时间。一个回合花完自己的 token、工具调用次数或墙上时间额度时不会重试：它以 `turn_budget` 暂停 Goal 并等待人工，因为下一回合只会以同样的方式花掉同样的额度。若花完的是 Goal 自己显式设定的 `token_budget`，Goal 则进入 `budget_limited`。

上游流在没有终止标记的情况下结束也算这类可恢复失败：Zuno 不会把被截断的回答当成模型已经
说完，而是丢弃这次已产生的部分输出并重放原样的请求。原生 provider 另有一道 330 秒的响应头
截止时间和 300 秒的流空闲上限（可用 `ZUNO_STREAM_IDLE_TIMEOUT_SECS` 调整），所以一个卡住
的请求会以类型化错误失败，而不是让会话无限等待。

```json
{
  "goal": {
    "retry": {
      "initial_delay_ms": 2000,
      "max_delay_ms": 300000,
      "jitter_percent": 20,
      "poll_interval_ms": 250
    }
  }
}
```

延迟是正值、有上限、带抖动，并且可被用户输入打断。有效的对端 `Retry-After` 会被夹到配置的上限，且绝不会被一个更早的本地延迟取代。重试决策来自带类型的错误，绝不来自渲染后的消息：认证失败与用户中断会暂停，而无效协议、损坏的持久状态和永久性配置失败会阻塞。

持久存储遵守同一套类型化划分。读取或记账 Goal 预算时遇到 SQLite 写者争用，或宿主写入 Plan 对账、人工请求等 Goal 所属状态时遇到争用，会落盘一条 `database_busy` 重试，Goal 保持活跃，因为别的写者持有锁是会自行消失的状况。读取或记账预算时遇到的其他任何数据库失败，会以 `usage_unknown` 停止回合并暂停 Goal，让人先检查数据库；此构建完全无法读取的持久状态会阻塞 Goal。重启后的进程读回的是落盘的 `goal_retry` 行，因此重启既不会丢掉也不会缩短这段等待。

引擎还会检测工作状态只读但没有进展的循环。`plan_get` 与 `todo_get` 会公布权威状态的
语义指纹；模型自由编写的 `intent` 不进入指纹。连续三次只调用一个读取工具且指纹相同，会以
类型化 `stagnant_tool_loop` 停止。活跃 Goal 会持久化为 `paused(no_progress)`，需要显式
resume；Plan 或 Todo 指纹变化会重置计数。

成功的 `plan_update` 与 `todo_update` 会使易变提示投影失效。下一次 provider 请求之前，
宿主从 SQLite 重新生成 Goal/Plan/Todo/Job 上下文。Plan 变更后，一次性的创建/替换指令也会
切换成维护指令，避免 `runtime.work_state` 在同一回合里继续描述旧 Plan。

对端要求的等待超过同请求恢复所剩时间时，结果同样由夹取决定。provider 层对同一个请求最多恢复 180 秒；超过这个窗口所剩时间的 `Retry-After` 既不会被睡过去，也不会被换成更短的本地退避。回合以对端的错误结束，Goal 级重试等待的是对端值按 `max_delay_ms` 截断后的结果。

## 续跑会话

```sh
zuno run --continue "now cap the page size at 100"
zuno run --session ses_1a2b3c "what changed?"
zuno run --session ses_1a2b3c --agent plan "what would a safe migration look like?"
```

续跑的会话会沿用它上次运行时的 Agent、模型与推理强度。所有重新打开会话的界面——`--continue`、`--session`、TUI 的 `/session` 选择器，以及 ACP 的 `session/load` 与 `session/resume`——都按同一顺序解析，因此显式参数仍然优先，配置只是最后的回退：

| 设置 | 优先级 |
| --- | --- |
| Agent | `--agent` 或选择器 > 会话上保存的值 > `default_agent` > `orchestrator` |
| 模型 | `--model` 或选择器 > 本进程中选定的 preset > 会话上保存的值 > 按配置路由 |
| 推理强度 | `--variant`、`--thinking` 或选择器 > 随会话模型一同保存的值 > 配置默认值 |

ACP stdio 的生命周期归启动它的编辑器所有。Zed 关闭或丢失该进程后，应重新启动
`zuno acp` 并按持久 session id 加载；旧 stdio transport 不会继续存在。稳定 ACP v1 以
load 作为权威重连路径。支持协商 resume 的客户端可以使用该操作，但 transport 重连不会替代
SQLite inbox、事件、Goal、Plan 与 prompt receipt 的恢复。background-TUI supervisor 只包装
TUI PTY，不包装 ACP stdio。

一句话概括：参数 > 会话 > 配置默认值。会话行记录的是它上次运行时的那一组值：TUI 中的模型、强度、preset 或 Agent 选择、ACP 的配置变更，以及会话创建，都会把模型引用（连同其 `variant`）写回，下一次续跑就从这里开始；`zuno run` 的参数只作用于那一次运行，不会改写会话行。指定一个与保存值*不同*的 Agent 时，模型会按配置重新路由，与在活动会话里切换 Agent 完全一致；要固定模型就传 `--model`。当保存的 Agent 已不在名册中，或保存的模型已不在目录里，续跑会回退到下一条规则，并以一条状态提示说明，而不是失败；模型不再声明的已保存强度也会以同类提示丢弃。

这个二进制不提供会话分叉，因此想探索一个替代方案又不污染想保留的那个会话时，就新起一个：`--continue` 与 `--session` 都不给，并给它一个 `--title`。

## 保留

存储会增长。列出、预览和清理都在 `zuno session` 下：

```sh
zuno session list
zuno session list --no-roots --archived --format json
zuno session prune --older-than 90
zuno session prune --older-than 90 --archive
zuno session delete ses_1a2b3c
```

既不带 `--archive` 也不带 `--delete` 时，prune 是一次无副作用的预览，其计数与随后真正执行的删除一致。`--archive` 设置一个可逆标记；`--delete` 不可逆，在这个二进制中没有撤销。在大规模执行任何一个之前，请先读[会话保留](/zh/operate/session-retention)。

## 参见

- [Goal、Plan 与 Todo](/zh/guide/durable-state)
- [History 与 Notes 连续性配置](/zh/config/continuity)
- [会话保留](/zh/operate/session-retention)
- [zuno session](/zh/cli/session)
- [Harness 运行时](/zh/operate/harness-runtime)
