# Harness 运行时

Harness 运行时是 Zuno 的核心：它决定一个回合如何组装、如何持久化、如何恢复，以及扩展在什么边界内运行。

::: warning 本页是导读，不是完整译文
英文版 [Harness Runtime](https://github.com/sunerpy/zuno/blob/main/docs/harness-runtime.md)
逐节覆盖运行时的每一处契约，是唯一权威来源。本页按相同章节顺序给出每一节要解决的问题和
关键结论，用于定位与建立整体认识。

不要用本页替代英文版做实现判断：精确的字段语义、状态机转换条件、重试与恢复的分类规则，
以及验收标准都只在英文版中完整表述。两者出现分歧时以英文版为准。
:::

## 运行时模型

一切都是原生组件。产品行为属于某个 `Component`、某个类型化服务，或某个 `AgentDriver`，而不属于一个不断膨胀的中心循环。

一项能力只有在接口、提供方与消费方三者齐备时才算完整。当这三个角色的生命周期不同时，应保持彼此分离。

注册是一种副作用。挂载操作返回的 disposer 精确移除它注册过的东西；profile 替换是事务性的，失败时按相反顺序回滚。

组件通过 `Component::stop_budget` 声明运行时等待其每个 disposer 的时长：默认的
`StopBudget::Runtime` 沿用运行时配置的停止超时；必须终止并回收进程树、排空 socket 或
等待 flush 的组件返回 `StopBudget::Bounded(时长)`，零值按 `Runtime` 处理。disposer 仍按
注册的相反顺序逐个运行，预算只限定运行时等待每一个的时长。超出预算的 disposer 记为
`TimedOut` 诊断但不会被取消：半途丢弃它会丢掉它正在回收的东西，比迟到的停止更糟，所以
它被脱离并在后台继续回收。

## Agent 与提示词契约

Agent 具有显式的正向职责、负向委派边界、权限以及结构化输出预期。

内置 Agent 的分工：`build` 负责端到端交付，`plan` 是只读规划，`deep` 承担困难的跨领域实现且不再递归委派。

根回合对已连接 MCP schema 使用渐进式披露：调度器保留可执行实现，`tool_search`
只搜索紧凑元数据，匹配项从下一次 provider step 起按单调 revision 扩展确切工具快照。
Agent 的确切 `tools` 允许列表会立即公开其中点名的 MCP schema；子级仍受父级 Attempt
中已持久化的确切 schema 上限约束，不能搜索出更大的权限面。ACP session-local
`mcpServers` 在严格连接门禁后也立即公开，但同一目录中的宿主配置 server 仍延迟发现；
Catalog 会把这个会话边界传递到子回合与后台续跑。

仓库与用户的规则文件要么整份进入 Prompt，要么不进入。宿主无法读取的本地规则文件，或者超出
指令预算（64 KB 与模型 context window 四分之一取较小值）的规则文件，会在第一次 provider
请求之前让本轮以类型化错误失败，错误点名文件与修复方式，超预算时还给出字节数与预算；这种
情况不会发出任何 notice，因为回合根本没有运行。过去的"记一条警告、然后不带该文件继续发请求"
行为已不存在：不带用户所写规则的回合不会运行。唯一的非致命情形是无法抓取的远程规则来源：
回合继续执行，但以 `warning` 级 notice `instruction.not_in_force` 报告哪个来源的规则本轮不生效。

## 扩展包与可执行插件宿主

扩展要么是显式 WASI 授权下的 WebAssembly 组件，要么是使用行分隔 JSON-RPC 的受限子进程。能力必须声明，不会被默认赋予。

详见 [插件与扩展](/zh/guide/plugins)；完整 WASI guest 与原生 Rust 实现路径见
[开发 Agent 与扩展](/zh/guide/extension-development)。

## 提示词溯源

**模型可见即被记录。** 每一个提示词分段、外部输入、工具结果、重试通知和子 Agent 报告，只要它能改变一次模型请求，就必须能从持久会话事件中重建。

提示词组装使用稳定的分段标识、确切来源、有序内容和内容摘要。实际经过 hook 之后的提示词在 provider 请求发出之前落盘。

## 加密推理重放

有些 Responses 端点会把一个步骤的推理封装成不透明信封，并绑定到单一模型、账户与会话。provider 用 `reasoningReplay: "encrypted"` 声明这项能力：它是端点选项，绝不是从 provider id 推断出来的规则。此后 Zuno 会为该 provider 的每个 Responses 请求加上 `include: ["reasoning.encrypted_content"]`，并在后续请求中逐字节回送每个封装项。只要请求解析到 Responses surface，这项声明就会生效：目录里的 `openai` provider 不需要任何声明就能到达那里，而端点来自 provider 选项的网关只有声明 `transport: "openai"` 搭配 `surface: "responses"` 才能到达。配置校验会按 provider 和按模型拒绝那些确实无法承载封装项的路由，并接受本来就会解析成 Responses 的配置。

默认值是 `off`：请求既不带 `include`，也不带任何封装项，包括同一会话在选项为 `encrypted` 时存下的信封。它并不表示请求字节与既有版本一致：下面的顺序修正对所有 Responses provider 生效，与该选项无关，因此先写文本再调用工具的一轮现在会先发文本项。每个重放的工具调用也会带上 provider 自己的 `arguments` 字节而不是重新序列化的结果，因为端点对它发出的那个字符串做指纹；而某个步骤的封装项后面没有任何输出时，这一项会被扣留而不是单独发出，并计入被扣留数而不算作一次重放。

封装信封属于持久状态，因此一个步骤会被持久化成带位置的 part 账本，而不是一段文本加上尾部堆积的工具调用。每个 part id 携带它在流中的位置 `prt_{turn}_{step}_{position}_{kind}`，且同一步骤的所有 part 共享 assistant 消息的创建时间，于是水合出来的顺序就是 provider 的产出顺序。一个先推理、写文本、调用工具、再推理、再调用第二个工具的步骤，会按同样的次序重放，每个信封都紧挨在它所解释的输出之前。这正是封装端点会校验的内容：顺序被打乱或只回送摘要都会在链路上被拒绝。

重放是有作用域的，而且作用域在组装请求时生效，不是在写行时生效。信封只会被重放给产出它的那条 assistant 消息上记录的目录 provider 与模型，并且只在它比 `reasoningReplayMaxAge` 更新时重放。其他情况下它只在这一次请求的内存里被扣留，持久行保留原密文，因此换回原模型即可恢复重放。标题、摘要、压缩、反思与 Council 请求运行在其他模型上，完全不会收到信封，这也让压缩转录中不含 provider 状态。

每个前台 `session.provider.request` 事件都会记录 `reasoningReplay`、`replayedReasoningCapsules` 与 `withheldReasoningCapsules`。这三个字段就是重放确实生效的依据：如果一个会话从第二个请求起报告的重放信封数仍然是零，那它就没有在重放，无论端点怎么声称。重放计数就是 adapter 真正放到链路上的数量，因此被配对规则丢弃的信封会计入被扣留数，绝不会算作一次重放。请求事件只记录这些计数，不记录信封本身。

信封本身是不透明的 provider 密文，作为会话内容保存：它存放在推理 part 的 `metadata.providerReasoning` 中，会由 HTTP messages 端点返回，还会随一个完整携带密文的流事件转发：服务器 SSE 流上的类型是 `provider.reasoning.item`，`zuno run --json` 打印的是 `provider_reasoning_item`，两者的密文都在 `encryptedContent` 字段里。它是后续请求需要的持久状态，因此不会被脱敏；能读取某个会话的消息或事件流，就等于能读取它的信封。

## 可审计的记忆与反思

记忆写入是提议而非直接生效。候选进入待评审状态，由人决定是否提升为常驻记忆，并且可以撤销。

## 持久输入

用户提示词、steering 以及子 Agent 报告在执行前进入持久 FIFO 收件箱。`reportDelivery: nextStep` 必须完成子结果结算、准许父级输入并唤醒父级，且不存在轮询竞态。

准入不与活跃回合租约竞争。同一个准入服务先提交 `session_input` 行，再决定它如何到达模型，因此每个界面（TUI、ACP、HTTP 与 `run` 宿主）对一条**已经持久**的输入只报告三种结果之一：调用方拿到独占回合租约并自己驱动该行；某个正在运行的回合以软中断接纳该行，并在下一个安全点提升它；或者该行保持待决，等下一次 FIFO 提升。先抢租约、抢不到就提前返回，正是那种「提示词丢失且没有任何持久痕迹」的做法，所以会话忙碌是准入的一种结果，而不是准入的失败。若调用方自己的驱动循环本就拥有该会话的每个回合，它根本不申请租约，只会收到 steered 或待决结果。

每个界面只解码自己能驱动的载荷形态，而每一种已发布形态只有一个解码器。驱动方无法渲染的待决行（HTTP 提示驱动遇到的排队 TUI 提交、终端驱动遇到的带自己 agent 与模型覆盖的 HTTP 请求体）会按 FIFO 顺序被跨过，保持待决交给拥有它的界面，而不是先提升再结算为 `failed`。已结算的异步报告与已回答的人工请求都是纯文本，所以每个提示驱动都会运行它们。没有任何写入方发布的载荷，出于同一个理由也保持待决：驱动方无法区分「无法识别的形态」与「自己本就不拥有的形态」，所以该行被保留并继续显示在队列里，而不是被销毁。因此在同一个会话上混用多个界面时，一条驱动方并不拥有的行不可能废掉该驱动方。

已结算的报告按批投递，而不是逐行投递。一次唤醒在同一个事务里认领父级当前所有待决报告，并在同一个回合里驱动它们：每条报告仍然生成自己的持久用户消息、自己的 `session.input.promoted` 与 `session.input.consumed` 事件，以及自己的 `message.data.taskReport`，只有 provider 请求是共享的。唤醒发现会话正忙时，同样把整批待决报告交给正在运行回合的下一个安全点；那个回合没来得及取走的报告保持待决，等下一次扫描。逐行各开一个回合的做法，会让同时结算的一次 fan-out 变成一串回合，而每个回合通报的都是批次内更晚的报告早已取代的状态。HTTP 提示驱动在同一个事务里认领同一个批次，因此在该界面上，持有三条已结算报告的会话同样只产生一个助手回合；而用户输入的提示词仍作为自己的请求运行，带自己的 agent 与模型覆盖。每个输入的飞行中租约仍按 `(session_id, input_id)` 保留：赢下回合的那次唤醒认领整批，输掉的唤醒发现自己那一行已被认领，直接返回而不驱动任何东西。

准入永不排在投递后面。后台命令的完成先作为持久 inbox 行准入，之后才请求回合；watcher 在同一轮里把排在它后面已经就绪的结算一并准入，因此同时结束的一组后台命令构成一个批次，重启后发现多条终态命令的进程也按一个批次投递。某条命令在投递回合已经运行时才结算，则立即准入并加入下一个批次——它的报告在结算之前无法存在。

渲染批次时报告按它们描述的工作分组。当一个批次里含同一个 job 或同一次后台执行的多条报告时，只有工作完成最晚的那条被呈现为该工作的当前状态，更早的在模型读到的文本里标注为已被取代。这个投影属于引擎而不属于某一个客户端，因此唤醒新开回合驱动的批次与唤醒交给正在运行回合的批次文本完全一致：无论更新的报告到达时父级是空闲还是繁忙，被取代的状态都读作已被取代，而现在与将来的每个客户端界面都从同一批持久行得到同一结论。Plan 对账以唤醒自己开出的那个回合所驱动批次里最新的报告为种子；被并入一个已经在运行的回合的批次作为持久用户输入进入该回合，而该回合保持它启动时的 planning 来源。分组只是对持久行的投影：不合并、不重排、不丢弃、也不改变任何 inbox 状态；每个工作单元只有一条报告的投递则完全按写入方原样呈现。已提升但持久 prompt 里没有模型可见文本的报告结算为 `failed` 并记录原因，而不是卡住排在它后面的报告。

图像入口在写入 inbox 前统一经过 `AttachmentStore`：规范化方向、像素与编码，原子发布当前数据库身份下的内容寻址对象，持久 part 只保存 `ImageAttachmentRef`。Provider 请求组装时才校验并内联对象；缺失或 digest 不符是永久持久状态失败，不回退原始路径。

提交转录回退（`revert_commit`）会删除暂存边界消息 `(time_created, id)` 之后的投影 `session_message` 行与旧表 `message` 行，清空会话的 context epoch，并把所有 `queued`、`steering`、`promoted` 的收件箱输入经常规取消迁移退役，每条各记一条 `session.input.cancelled`；已消费（consumed）的输入是不可变历史，不受影响。回退永不删除收件箱行。随后追加一条 `session.reverted` 事件，字段为：`sessionID`（字符串）、`messageID`（字符串，回退后仍是转录尾部的边界消息）、`marker`（对象，暂存的回退 JSON 原样，如 `{"messageID": "...", "files": []}`）、`boundaryTimeCreated`（i64 毫秒）、`removedMessageCount`（u64，删除的投影行数）、`removedLegacyMessageCount`（u64，删除的旧表行数）、`cancelledInputIDs`（字符串数组，按准入顺序）、`contextEpochCleared`（布尔）、`timeUpdated`（i64 毫秒）。所有键始终存在。

## Plan 与 Work 状态迁移

持久的 Goal、Plan、Todo、收件箱和 job 状态控制续跑，而不是自然语言。「接下来我会……」
这类文字不构成进展。默认 profile 发布类型化的宿主 Planning capability；即使最终工具
过滤隐藏了 `plan_update`，已有 Plan 仍会持久化、投影并在重启后恢复，但模型不能创建
或修改新的战略步骤。宿主分类器只判断 `Required / Maintain / Atomic / Unavailable`，
不会生成 `Establish scope / Execute / Integrate / Verify` 一类通用骨架。单句短问句
无论是否以问号结尾都归为 `Atomic`，包括疑问词位于句中的中文问法。模型使用
`create / patch / append / push / pop` 操作维护 Plan，step id 由宿主生成，已有 Plan
修改都受 `expected_revision` 保护。

机器执行阶段单独持久化为 `DriverPhase`，不进入用户可见 Plan。最终回复前，
`PlanReconciliationDriver` 只检查 Plan、Todo、Job、Goal、工具结果与验证记录：
没有记录任何持久工作的会话在第一次回复后直接结束；普通会话在持有未对账的持久工作时
最多续跑两次对账；仍不一致则进入 typed `PlanUnreconciled` 人工等待，不能以成功状态
交付。「是否需要 Plan」只是宿主的分类预测，不是已记录的工作，因此被判为 `Required`
却没有产生任何 Plan、Todo 或 Job 的请求视为已结算，不会再被续跑；一次被误分类的问题
只回答一次，不会为不存在的状态再花两个回合。进程重启会继续原对账 cycle，不解析模型
自然语言判断“已经完成”。

ACP 通过会话级投影器订阅 `TurnHost::work_state_changes()`，而不是识别某个工具名。
每次唤醒都会读取权威 Plan 并发送完整的 stable-V1 更新；`(plan_id, revision)` 阻止
重复与旧 revision 覆盖新状态，Plan 被移除时发送空 entries 清除客户端旧面板。
实时变更、prompt 结束前 flush、load、resume、后台 continuation 与 host 重建共用同一
投影器。

## 原生会话命令、压缩与硬中断

会话命令、上下文压缩与硬中断都是原生能力，不依赖模型配合。

自动压缩只在摘要落盘之后才咨询 auto-continue hook。该 hook 失败时会话保持
`Compacted`：摘要保留，不合成续跑回合（没有人投票就不授予续跑），失败原因随压缩结果以
`auto_continue_hook_failure` 记录并输出告警；会话不会被标记为失败，因此后续压缩不再被
`AlreadyFailed` 拒绝。工具 after-hook 失败同理：工具已经运行，结果保持自身状态与
`is_error`，hook 失败作为 `afterHookError` 元数据随结果返回，而不是被改写成一条会让模型
以为副作用没有发生、进而重复执行的裸错误。

## 原生 History 与 Notes

`zuno-continuity` 是通过 `ProfileBundle` 与 `ToolContributions` 挂载的原生组件，默认关闭。
`history` 只读取当前会话，并以成功压缩作为窗口边界；reasoning、加密值、合成内部提示
正文和二进制附件字节不会返回。`notes` 使用逻辑文档名，按 `session_id + Agent` 隔离。

Notes 每个作用域最多 100 个文档，单文档 256 KiB，总计 1 MiB。写入必须带精确
`expected_revision`，并用可信 `call_id`、请求摘要和 revision 做幂等与并发冲突保护。
读取采用 `Safe + ParallelSafe + ReadOnly`，写入采用
`Never + Exclusive + SideEffecting`；非法 action 按最严格策略失败即拒绝。

组件自有的 `session_note` 与 `session_note_operation` 是增量表。它们随
session 级联删除，并进入 session export/import、sanitize 与 prune。TUI、server、ACP
和 child turn 都消费同一套运行时工具快照，不拥有私有连续性逻辑。

## 持久 Goal 恢复

活跃的 Goal 会持续推进，直到它完成、被显式暂停或阻塞、达到预算上限，或遇到类型化的永久失败。

原生 `/goal <目标>` 创建或编辑成功后，会把这次宿主命令标记为完整的 idle edge，并立即
交给共享 Goal continuation driver。若会话还没有 user message，driver 会先把目标本身
通过持久 inbox 准入为首个 user turn anchor；字面的斜杠控制文本不会进入 provider。
目标变化也会同步活跃 Plan：多阶段目标归档此前可见 Plan 并安装绑定当前 `goal_id` 的新根
Plan；原子目标不改绑已终态的历史 Plan，属于上一个 Goal 的终态 Plan 会在目标变化时、或仍
可见时在下一次宿主规划决策时归档为已完成的历史，完成审计不会再拿它对账新 Goal。

ACP load/resume 会重建运行时、按请求重放持久投影，然后通过 detached continuation
observer 调度 active 根 Goal。恢复任务与普通 prompt 共用会话执行门，因此不会并发启动
第二个 Goal 回合；0.6.0 已落盘但未产生首个 user message 的 Goal 也会在此路径补齐并续跑。

可恢复的 provider、网络、流、SQLite 争用、Agent 步数上限和符合条件的工具失败，会在等待前先持久化一次指数退避重试。进程重启后从 SQLite 重建截止时间。

重试延迟是正数、有上限、带抖动，并且可被用户输入打断。有效的对端 `Retry-After` 会被限制到配置上限，且绝不会被更早的本地延迟替换。对端要求的延迟超过 180 秒同请求恢复期限时，回合以对端自身的类型化错误结束，Goal 级重试等待的是对端值按 `max_delay_ms` 截断后的结果，不再退回更短的本地退避。

重试决策使用类型化错误，而非渲染后的消息。认证失败与用户中断导致暂停；无效协议、损坏的持久状态和永久性配置失败导致阻塞。

读取或记账 Goal 预算时遇到 SQLite 争用（`SQLITE_BUSY`）会持久化一次 `database_busy` 指数退避重试，Goal 保持活跃，而不是以 `turn_budget` 暂停；其他数据库失败仍以 `usage_unknown` 停止回合并暂停 Goal；本构建无法读取的持久状态仍然阻塞。CLI 回合中的 Plan 对账驱动、human request 创建与重试上下文压缩标记路径也经同一 `GoalTerminalFailure::from_db_error` 规则分类：争用现在以 `database_busy` 重试，过去则以 `host_permanent` 阻塞。

`timeout`、`headerTimeout`、`chunkTimeout` 只有 OpenAI-compatible 传输会读取，其默认值是
330 秒响应头截止时间与 120 秒分片空闲上限。原生的 OpenAI、Anthropic、Google、Bedrock
四个 provider 不读这三个键：它们固定采用 330 秒响应头截止时间，不设整请求截止时间（一个
合理的长回合没有 provider 能事先知道的上限），分片阶段则沿用共享的 300 秒流空闲上限，
该上限由 `ZUNO_STREAM_IDLE_TIMEOUT_SECS` 对所有 provider 统一调整。原生请求在收到第一个
响应头之前卡住时，现在会在上限处以类型化错误失败，而不是一直等到用户中断。

流在没有终止标记的情况下结束，属于上游流不完整，而不是一次完成的回答。每个原生解码器都会
报出携带 `upstream_stream_incomplete` 的 `ProviderError::Stream`，因此它可重试并允许替换
已产生的部分输出：引擎发出 `RetryRollback`，丢弃被截断的流写出的内容，然后重放原样的请求。
一个终止标记就足够，所以只发 `finish_reason`、或只发 `[DONE]` 的 Chat Completions 流都算
正常完成。

- 被自身预算策略停下的回合以 `turn_budget` 暂停。额度属于单个回合，Goal 保留剩余的
  token 预算，但不会自动续跑：下一回合只会以同样的方式花掉同样的额度。这与 Goal 整体
  预算耗尽的 `budget_limited` 状态不同。
- 预算策略可以要求压缩而不是停止。这被归类为上下文上限失败并走同一条路径：压缩保留的
  历史，然后重试该回合。
- 每次 provider 请求前后都会咨询回合预算策略。默认 profile 发布的 `TurnAllowance` 把
  未设预算的 Goal 置于 8,000,000 token 的宿主默认额度之下，并可另设工具调用次数上限与
  墙上时间上限；这两道上限无论有没有 Goal 都生效，触顶时以 `tool_call_budget` 或 `time_budget` 停止。
  在用户显式设定的 Goal 预算下，用量不可测时以 `usage_unknown` 停止；宿主默认额度则继续执行，
  只按已计数的用量生效。上限优先于压缩请求或继续执行，但让位于 Goal 自身已产生的停止。
  `None` 不再等于无限自治：只有不发布 allowance、或显式发布 `TurnAllowance::UNLIMITED` 的
  profile 才没有上限。每次停止都是类型化的 `TurnError::BudgetLimited`，并以 `notice` 事件
  （code 为 `budget.<kind>`）投影给客户端；压缩请求的 code 为 `budget.compact`。

## 文件工具的路径权威

`read`、`write`、`edit`、`apply_patch` 在授权时解析路径一次，随后通过解析过程保留下来的
目录句柄执行操作。解析从授权边界开始 —— 工作区根目录，或一个被显式授予的外部目录 ——
逐段向下，打开每一段时都不跟随符号链接并要求它是目录，且绝不第二次解析这个名字。这条
性质是精确的：调用要么到达用户批准的那个目录对象，要么失败。最后一段是符号链接是唯一
有意的例外：它在授权之前被跟随一次，因此用户批准的是目标文件，而链接本身在写入之后
保留。

两个更弱的修法被否决了。只拒绝跟随最后一段没有任何作用，因为被替换掉的对象是中间目录。
在授权之后重新 canonicalize 仍然是「检查」与「使用」分离，窗口只是被挪了个位置。

机制按平台不同，保证也不相等：

| 目标平台 | 机制 | 窗口 |
| --- | --- | --- |
| Linux | 每一段都经 `/proc/self/fd/{fd}/{segment}` 打开，内核相对被固定的描述符解析 | 已关闭 |
| macOS | 用 `O_NOFOLLOW_ANY` 打开 `root/relative`，发布之前再核对文件身份 | 收窄：`rename` 无法携带该标志 |
| Windows | 带 `FILE_FLAG_OPEN_REPARSE_POINT` 逐段遍历，拒绝任何带 `FILE_ATTRIBUTE_REPARSE_POINT` 的分段，发布之前再核对卷序列号与文件索引 | 收窄 |
| 其他目标平台 | 同样的逐段遍历，改用 `symlink_metadata` 拒绝，发布前做同样的复核 | 收窄 |

只有 Linux 完全关闭了这个窗口，因为只有 Linux 提供了在不使用第一方 `unsafe`（本工作区
禁止）的前提下相对一个打开的描述符命名路径的办法。因此 `openat2`、`renameat` 与
`GetFinalPathNameByHandleW` 都没有使用。逐段拒绝 reparse point 同时覆盖 Windows 的目录
junction 与符号链接，这一点很重要，因为 junction 在那里才是更常见的攻击形态。

发布环节区分两种失败，因为它们不该拿到同一张收据。在「让新内容可见的那次 rename」之前
发生的失败，目标文件仍然保有它原本的字节，报为普通的工具失败。那次 rename 部分完成后
失败、或结果丢失，则报为 `Uncertain`，并带上已经生效的路径。这个结果不可重试、绝不重放：
它要求检查权威状态，与快照存储对一次半途而废的恢复所采用的规则相同。当目标是符号链接时，
替换还会拒绝长度超过 40 的链接链而不是继续跟随，因为到那个长度这已经是一个环，而不是
有意的重定向。

## 原生搜索与 Shell 隔离

搜索把遍历委托给 ripgrep 本身，Zuno 不维护第二个 ripgrep 兼容的遍历器。发现是惰性的，因此其他命令与工具不需要 ripgrep；只有在调用 `glob` 或 `grep` 时才要求 `PATH` 上有 14 或更新的主版本 `rg`（或由分发方随 Zuno 一起打包）。缺少或版本不受支持时是带类型的工具错误，而不是静默回退，也不会妨碍 Zuno 启动。

发现结果会被缓存，但不对称。成功的解析保留整个进程生命周期，因此 session 重新挂载与
子回合不会反复 spawn `rg --version`。失败只保留五秒，随后重新探测。这个不对称是有意的：
ripgrep 只支撑 `glob` 与 `grep`，所以在会话进行中安装它的用户必须能不重启 Zuno 就让这
两个工具可用；而一个在没装 `rg` 的机器上反复调用 `grep` 的模型，不能每次调用都 spawn
一次探测。并发的首批调用方之间只做一次探测，探测过程 panic 也不会让 ripgrep 永久不可用。

## Skill catalog 热更新

运行中的 Skill catalog 会保留规范用户根目录
`$XDG_CONFIG_HOME/zuno/skill` 与显式配置路径。根目录尚不存在时，只非递归监听最近的
已有父目录；目录出现后，消费任务会在 native watcher 回调之外安全地逐级收窄订阅，
并且只在精确根目录存在时开启递归监听。

Zuno 不监听 `~/.zuno` 或远端 Skill 缓存；缓存只在配置远端索引并实际下载时按需创建。
标准共享根 `~/.agents/skills` 只在启动时已经存在时监听，其他共享目录需要通过
`skills.paths` 显式配置以支持运行中安装。

Shell 执行受 OS 沙箱约束。`read-only` 与 `workspace-write` 都要求一个已验证的约束后端，不可用时拒绝启动而非降级。详见 [权限与沙箱](/zh/guide/permissions)。

Linux bubblewrap 后端的发现结果按进程缓存，键为规范化 workspace 加 helper 可执行文件；每次命中前重新校验可信 launcher、可信 `true` 与 helper 在磁盘上的身份，校验失败即逐出并重新探测；发现失败绝不缓存，缓存也不跨进程持久化。`zuno debug sandbox` 与部署报告绕过缓存，始终重新探测。

## 常驻进程约束

常驻进程在声明的约束内运行，其生命周期由运行时拥有。

Unix PTY 通过前台守护进程拥有进程组与终端前台切换。Windows ConPTY 则直接启动请求的
终端程序：不能把常驻 Job Object 守护器嵌入 ConPTY，否则交互输入与自然退出都可能无法
收敛。PTY 所有者持有直接子进程 PID，先应答并移除后端的一次性继承游标查询，再向客户端
转发终端输出；关闭 writer/master 后发布退出，显式停止通过 `taskkill /T` 终止完整
子进程树。

Windows 上的守护器不再轮询 `tasklist`，而是通过绝对路径
`%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe` 启动一个持有真实进程句柄、
等待父进程退出的助手，armed 之后 PID 复用无法冒充父进程。助手在 payload 启动之前武装：
无法启动时守护器以 125 关闭，payload 不会启动；助手启动后无判定地结束时只写一条诊断，
payload 继续运行并仅监督其自身退出，丢失的助手绝不视为父进程已死。

## 后台命令执行

后台命令有独立的生命周期与输出游标，父会话通过持久状态观察它们，而不是靠轮询。

每次执行都在子进程守护器之后运行，因此退出码 125、126、127 可能属于守护器而非命令：125
表示守护器自身失败、命令结局未知，shell 工具报告不确定结果且绝不重放；126、127 表示程序
从未启动，退出码被记录但没有 exit authority。只有捕获输出中出现守护器自身的诊断行时，
保留码才被读作守护器的判定，普通程序自行 `exit 125` 仍保留权威收据。信号致死时守护器在
自身重放同一信号，收据没有退出码，显示为「killed by a signal」而不是 `exit 1`。

## 后台子 Agent 与产品 Agent

工具执行默认是至多一次。`ToolReplayPolicy::Never` 是默认值；只有显式声明为只读或幂等的工具才可以声明 `Safe`。

副作用附近的超时或响应丢失属于结果不确定。这种情况会被持久化，要求检查权威状态，绝不机械重放调用。

`subagent_model_selection` 默认关闭。开启后，精确 model allowlist 会在 profile 激活时解析，并按 session 持久冻结为带 digest 的策略；`task` 才会出现可选 `model`/`effort`。续跑不能改变首次冻结的模型或强度。

## 并发网络搜索

`web_search` 接受一批查询，并在单查询 provider 之上拥有并发、取消、稳定排序、限流与 URL 去重。

## 网络出口

网络出口受沙箱的网络授权控制。`deny` 会创建私有网络命名空间并拒绝网络系统调用，而不是一条可被绕过的防火墙规则。

公开网页抓取使用独立 `PublicHttpClient`：只接受无凭据 HTTP(S)，遵循进程级
`HTTP_PROXY`、`HTTPS_PROXY`、`ALL_PROXY` 与 `NO_PROXY`，并且代理失败不会静默
改为直连。每次请求和每次重定向都重新解析、校验全部地址，通过代理连接已校验的目标 IP，
同时保留原始 Host/TLS SNI。公私混合 DNS、回环/私网/链路本地/CGNAT/保留地址，以及
IPv4-mapped IPv6 与 NAT64 中嵌套的非公开地址都会整体拒绝。

WebSearch 的带密钥 wire URL 不进入诊断。错误只保留 provider、scheme、host、path、状态与类别，reqwest cause 在进入错误链前移除 URL。

## 提示词工作流 V2 验收

提示词工作流 V2 的验收条件与证据记录在 [提示词与工作流指南](/zh/operate/prompt-workflow) 与 [设计文档](https://github.com/sunerpy/zuno/blob/main/docs/design/prompt-workflow-v2.zh-CN.md)。

## 构建一个 harness

优先通过 `ProfileBundle` 与 `HarnessProfile` 进行组合。部署选择与可调参数属于经过校验的 profile 或配置字段。

新行为使用文档化的扩展点。改变默认 Agent 循环需要在同一次变更中更新英文版 harness 运行时文档。

## 客户端界面

客户端界面消费持久事件、收件箱状态和投影。TUI、server、ACP 以及未来的 GUI 客户端不得获得私有的 Agent 循环行为。

原生文件修改的实时投影与历史 replay 也共用同一内容策略。`edit`、`write` 和
`apply_patch` 使用 `Editing files` 卡片；成功且有类型化状态时，可见内容只展示
新增/修改/删除 diff，完整原始结果保留在 `rawOutput`。成功但没有 diff 时保留简短文本。
写入前失败只展示可操作错误，不伪造 diff；部分写入或其他不确定结果保持 failed，
保留已观察到的路径/diff，并发布类型化 `uncertain` outcome。

`zuno run --show-reasoning` 只把 provider 明确提供的 reasoning delta 用稳定区块写入 stderr，最终答案继续只写 stdout；signed/encrypted reasoning 永不显示，且不能与 JSON 格式组合。

`zuno serve --browser-auth` 是显式的纯回环模式：单次 256-bit 启动 token 换取绑定 authority 的 30 天签名 Cookie；Basic Auth 与 Cookie 任一有效即可授权，Cookie 的非安全方法还要求精确 Origin。bootstrap query 在访问日志前被脱敏。

## 参见

- [Harness Runtime（英文完整版）](https://github.com/sunerpy/zuno/blob/main/docs/harness-runtime.md) —— 逐节权威契约
- [权限与沙箱](/zh/guide/permissions) —— 沙箱与权限的两个门禁
- [编排与委派](/zh/guide/orchestration) —— 委派边界与模型路由
- [Goal、Plan 与 Todo](/zh/guide/durable-state) —— 持久状态如何控制续跑
