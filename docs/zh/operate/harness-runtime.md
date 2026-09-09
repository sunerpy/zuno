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

部署可以用 `runtime.max_component_stop_ms` 给每一次等待设上限，运行时按
`min(声明值, 上限)` 生效；省略或 `0` 表示本机不设上限，这也是默认行为。上限只缩短等待：
disposer 仍被脱离而不是被取消，被夹取的进程树回收仍会在超时之后继续。

## Agent 与提示词契约

Agent 具有显式的正向职责、负向委派边界、权限以及结构化输出预期。

内置 Agent 的分工：`build` 负责端到端交付，`plan` 是只读规划，`deep` 承担困难的跨领域实现且不再递归委派。

`runtime.execution` 还会按最终工具快照为内置与自定义 Agent 生成简短降级规则：首选工具
限速、不可用或暂时失败时，不原样重复调用。`tool_search` 可见时可以发现另一个已经授权的
已连接工具，包括 `google_search`；Shell 可见时，GitHub 优先使用已安装的 `gh`，仓库搜索
优先使用 `rg`，而不是先写原始 `curl` 或手工遍历。不存在的能力不会出现在指导中，降级也
不能扩大权限。

根回合对已连接 MCP schema 使用渐进式披露：调度器保留可执行实现，`tool_search`
只搜索紧凑元数据，匹配项从下一次 provider step 起按单调 revision 扩展确切工具快照。
成功的 `tool_search` 结果同时是会话的持久暴露账本。后台报告重建宿主、进程重启或客户端
重新挂载时，会在第一次 provider 请求之前恢复这些 id，并与当前仍连接、权限仍可见的目录
取交集；已经移除的能力不会因此重新获得权限。
Agent 的确切 `tools` 允许列表会立即公开其中点名的 MCP schema；子级仍受父级 Attempt
中已持久化的确切 schema 上限约束，不能搜索出更大的权限面。ACP session-local
`mcpServers` 在严格连接门禁后也立即公开，但同一目录中的宿主配置 server 仍延迟发现；
Catalog 会把这个会话边界传递到子回合与后台续跑。

每个新工具 part 还会保存准入该调用的 provider-visible schema identity。组装下一次请求时，
只对更早 turn 的保留历史与当前 hook 后的工具定义对账；当前 turn 刚产生的调用始终保留原生
配对，以便未知或被拒绝的调用仍能收到协议完整的 tool result。对更早历史，声明一致时保留原生
tool-use/result 协议。新的 replay hash 只在 schema 与子 schema 位置移除 description、
title、examples、comment、default 等纯注解键，保留 required、type、enum 与其他取值约束。
参数名、定义名，以及 const、enum 或未知扩展值中的对象均保持原样，即使它们的键也叫
description、title 或其他注解名称。没有 replay hash 的旧记录
仍要求 description 与 schema hash 完全一致。工具缺失、结构性 schema 变化或持久 identity
无法读取时，只在本次请求中降级为有界惰性 JSON 文本：arguments 与 output 会在序列化 fallback
对象前按 UTF-8 边界限制单字段和整次请求大小。数据库里的原记录不会被改写，宿主也不会为了
重放而把当前不可执行的旧工具重新宣传成可调用能力。旧版本没有 identity 的记录会先按
assistant message 从不可变 provider-request Attempt 中恢复确切 hash；若这份证据也不存在，
即使当前存在同名工具也会降级，而不会把旧调用静默绑定到新 schema。无工具的内部压缩请求
采用更严格的同一原则：工具调用和结果以有界的惰性 JSON 文本进入摘要模型，绝不会在没有
声明的情况下继续使用原生函数协议。
降级到不认识新增 `replaySchemaSha256` 字段的旧 Zuno 时，会失败关闭为惰性历史；
持久调用本身不会损坏或被改写。

Goal、Plan 与 Todo 状态工具使用 `AuthoritativeState` 历史策略。旧声明不兼容时，历史调用与
结果会被省略，不转成模型可见的惰性说明；当前状态由每次请求同源生成的 `runtime.work_state`
提供。通用工具继续采用精确声明 fallback。历史修复 notice 使用 Diagnostic audience，按会话、
context epoch、工具及新旧 identity 去重，只写结构化日志，不进入 ACP thought、TUI 对话或
HTTP 事件历史。

`prepare_request` hook 仍然只能缩小已锁定的工具集合；新增、替换或重复 schema 会在发送前
失败。若 hook 删除的是保留历史仍需的声明，引擎采用上面的角色感知降级，而不是以 hook
错误终止回合。当前 turn 刚产生的调用不参与这项历史修复。历史
`ToolUse`/`ToolResult` 块另有一份按出现次序与 role 锁定的快照：hook 仍可修改普通文本，
但不能新增、删除、替换、复制、重排、拆分、在原生调用与结果之间插入另一条消息，或改变
工具协议历史的 role。持久 identity 失败与 hook 后声明删除会先合并，再执行一次按 occurrence
排序的 fallback 投影，因此混合并行批次仍保持持久结果顺序。

仓库与用户的规则文件要么整份进入 Prompt，要么不进入。宿主无法读取的本地规则文件仍会在第一
次 provider 请求前以类型化错误停止本轮，并点名文件与修复方式。一个内容完整但超出指令预算
（64 KB 与模型 context window 四分之一取较小值）的条目则整份跳过。宿主发出
`warning` 级 `instruction.not_in_force` notice，包含来源、字节数、预算与剩余空间，然后继续
考虑后续更小的独立条目；规则文件绝不截断，超大 `AGENTS.md` 也不会阻止 ACP、TUI、server 或
CLI 启动。无法抓取的远程规则来源使用同一类非致命 notice，因为网络可用性不应决定 Agent 能否
运行。

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

自动 Goal continuation 即使没有新的用户消息，也属于一个新的 provider turn。Zuno 会在该 turn 的第一条 assistant 行上只保存 prompt receipt 引用，而不是再复制一份提示词；当两个 assistant 响应在 Responses `input` 中本来会直接相邻时，引擎会把 receipt 中 hook 后实际发送的 developer 项解析成后一个请求消息上的结构化 `ResponsesInputBoundary` sidecar。OpenAI、OpenAI-compatible 与 Bedrock Mantle/Runtime 共用同一 Responses cursor，把这些标准输入项投影到两个 assistant 输出之间，再重放后一轮的推理信封。通用 `Message` 内容、Chat Completions、Anthropic Messages 与 Bedrock Converse 不携带该 sidecar；空边界的序列化与普通消息完全一致。旧版本写入的行会沿持久化的 `assistantMessageID -> promptReceiptID -> actualProviderProjection.developer` 证据链恢复同一 receipt；只有没有 hook 改写时才回退到 `providerProjection`。恢复时会剥离稳定的 runtime policy 前缀，因此历史边界只包含原始 turn context、memory 与 request hook context。这个过程不会伪造 user 消息或工具结果。

压缩摘要和旧 session export/import 可能确实没有 receipt。此时 Zuno 只扣留歧义 assistant 输出组中的封装 capsule，保留文本、工具调用和真实工具结果，让会话以较低推理连续性继续。每条 Responses 请求构造路径仍保留最后一道本地校验；若共享修复后仍存在畸形分组，它只报告消息索引，不会渲染不透明 token。

重放是有作用域的，而且作用域在组装请求时生效，不是在写行时生效。信封只会被重放给产出它的那条 assistant 消息上记录的目录 provider 与模型，并且只在它比 `reasoningReplayMaxAge` 更新时重放。其他情况下它只在这一次请求的内存里被扣留，持久行保留原密文，因此换回原模型即可恢复重放。标题、摘要、压缩、反思与 Council 请求运行在其他模型上，完全不会收到信封，这也让压缩转录中不含 provider 状态。

每个前台 `session.provider.request` 事件都会记录 `reasoningReplay`、`replayedReasoningCapsules`、`withheldReasoningCapsules`、`restoredReasoningReplayBoundaries` 与 `withheldAmbiguousReasoningCapsules`。这些字段就是重放确实生效的依据：如果一个会话从第二个请求起报告的重放信封数仍然是零，那它就没有在重放，无论端点怎么声称。重放计数就是 adapter 真正放到链路上的数量，因此被配对规则丢弃的信封会计入被扣留数，绝不会算作一次重放。恢复边界数表示本次请求从 prompt receipt 重建了多少条旧 assistant 行的准确 developer 后缀；歧义扣留数是因边界无法证明而降级的子集。请求事件只记录计数，不记录信封或 developer 文本。

信封本身是不透明的 provider 密文，作为会话内容保存：它存放在推理 part 的 `metadata.providerReasoning` 中，会由 HTTP messages 端点返回，还会随一个完整携带密文的流事件转发：服务器 SSE 流上的类型是 `provider.reasoning.item`，`zuno run --json` 打印的是 `provider_reasoning_item`，两者的密文都在 `encryptedContent` 字段里。它是后续请求需要的持久状态，因此不会被脱敏；能读取某个会话的消息或事件流，就等于能读取它的信封。

## 可审计的记忆与反思

常驻记忆与用户学习默认启用。自动学习优先使用显式
`learning.extractor_model`，否则依次使用当前 provider 下可达的
`small_model` 与当前会话模型，不会打开另一个 provider 或改写配置。只有项目范围且
置信度至少为 `0.9` 的候选会自动写入；全局与低置信候选继续等待评审，并且所有已应用
变更都可以撤销。

自动学习没有额度百分比、每日 token 或金额预算。后台仍受六小时空闲、每次最多两个
任务、资格检查、去重与最多三次尝试约束；真实 provider 限流会保留 typed
`Retry-After`。新会话固化新的自动默认值，已有 `/memories` 会话策略不会被追溯改写。

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
或修改新的战略步骤。宿主只执行模式与持久状态策略：显式 `plan` 协作模式为
`Required`，已有 active Plan 为 `Maintain`，普通 Work 输入为 `Optional`，没有 active
Plan 的宿主生成输入或空输入为 `Atomic`，隐藏工具为 `Unavailable`。图片、resource、
selection、branch diff 和多文本块在 Work 模式也只是结构化上下文，不会自动强制 Plan。

宿主不再解析「修改」「修复」「全部」、问句、确认语或其他中英文关键词来猜复杂度。Work
模式由模型依据完整会话判断：只有真正多步骤且需要协调、顺序、委派或恢复价值时才使用
Plan；直接完成简单或单步任务，绝不创建单步骤 Plan。因此「OK，你修改下吧」无需专门词表，
复杂任务仍可由模型主动建立 durable Plan。模型使用
`create / patch / append / push / pop` 操作维护 Plan，step id 由宿主生成，已有 Plan
修改都受 `expected_revision` 保护。每次 mutation 之前都要立即调用 `plan_get` 并复制其
当前 revision；只有 `plan_get` 返回 `null` 后的首次 `create` 可以省略。
`plan_update`、`notes`、`history` 这类以操作为标签的
参数枚举以单个对象 schema 发送给 provider：`action` 属性枚举全部操作，也是 schema 中唯一
必填的字段；每个操作自身需要的字段由类型化反序列化器校验。同一字段在不同操作间形状不同时，
若只是某个操作把它变为可选，就发送可为空的形式；若只有描述不同，则按操作归属各自的描述；
形状确实不同时发送 `anyOf`。

工作状态工具还参与两项类型化引擎契约。读取会公布
`ToolProgressObservation`，其指纹只包含权威 Plan/Todo 状态，不包含自由文本 `intent`；
mutation 会公布 `ToolDynamicContextRefresh::WorkPlan` 或 `WorkItems`。若确实还要发送下一次
provider 请求，引擎把已提交连接交给宿主刷新器；CLI 宿主从 SQL 重新生成
Goal/Plan/Todo/Job 上下文，Plan mutation 还会把一次性的 Required 指令切换成 Maintain。

连续三次成功、单工具、同一工作状态指纹的读取会以 `StagnantToolLoop` 停止。第三次结果先
持久化并投影；换工具、失败/阻塞/中断、写入路径、continuation 或状态变化都会重置序列。
恢复策略是 Pause，活跃 Goal 记录 `no_progress`，不会自动重复同一付费读取。

机器执行阶段单独持久化为 `DriverPhase`，不进入用户可见 Plan。阶段包括 `idle`、
`executing`、`reconciling`、`waiting_retry`、`waiting_background`、`paused`
与 `terminal`。最终回复前，`PlanReconciliationDriver` 只检查 Plan、Todo、Job、Goal、
后台观察器、工具结果与验证记录：
没有记录任何持久工作的会话在第一次回复后直接结束；已授权 Work 从 durable
`Recovery` token 继续。driver 把权威 revision 哈希为 progress fingerprint，连续三次
相同才以 typed `no_progress` 暂停，不制造通用人工确认。Work 模式的 `Optional` 决策不是已记录工作；没有产生任何 Plan、Todo 或 Job 的
请求视为已结算，不会为不存在的状态额外续跑。进程重启会继续原对账 cycle，不解析模型
自然语言判断“已经完成”。

若未完成的持久工作仍依赖一个运行中的 `backgroundPurpose: "remoteObserver"`，driver
进入 `waiting_background`，结束当前回合，不轮询，也不创建通用人工问题。活跃 Goal 在观察器仍运行时不会立即再次自动续跑；已有后台
完成 watcher 会在进程结算后把终态报告写入 inbox 并唤醒会话，后续回合重新查询远端权威状态，
再恢复普通对账。

ACP 通过会话级投影器订阅 `TurnHost::work_state_changes()`，而不是识别某个工具名。
每次唤醒都会读取权威 Plan 并发送完整的 stable-V1 更新；`(plan_id, revision)` 阻止
重复与旧 revision 覆盖新状态，Plan 被移除时发送空 entries 清除客户端旧面板。
实时变更、prompt 结束前 flush、load、resume、后台 continuation 与 host 重建共用同一
投影器。

根会话消息是独立的持久输入能力。只有没有 parent tool authority 的 turn 才能看到
`session_message`；执行时还会再次确认来源是 root。同项目 root 可以互发，root 也可发送给
自己的后代；其他 root 的 child、归档目标、自己与跨项目目标都会被拒绝。TUI、ACP 与 server
消费同一个 `sessionMessage` 形状；在线进程可在安全点 steer，否则输入保持 queued。

## 原生会话命令、压缩与硬中断

会话命令、上下文压缩与硬中断都是原生能力，不依赖模型配合。

prelude 会在第一次 provider 请求之前检查持久历史。长工具回合不会等到下一回合才再次
判断：同一回合第一次之后的每个 provider 请求，都会在发送前按同一套 compaction policy
主动检查上下文。检查优先使用上一响应由 provider 报告的上下文用量；没有可用报告时，
回退到当前刚组装完成的 prompt 估算。达到阈值会发出 info 级稳定 notice code
`context.compact`，并在写入 assistant checkpoint、prompt receipt 或 provider-request
记录之前返回 `TurnError::CompactionRequired`。宿主会在同一次 drive 内消费这个内部信号，
压缩持久历史、把落盘摘要投影给 live TUI/ACP，再直接重试；成功恢复不会产生终端 turn
failure、不会增加 `failed_turns`、不会安排 Goal retry，也不需要等待下一条用户输入、子任务
报告或 timer wake。单次 host drive 最多自动恢复五次；超过上限或压缩本身永久失败时会
类型化失败关闭，而不是循环。`compaction.auto: false` 不会向回合挂载主动阈值，因此也会
关闭这些请求间检查；手动压缩和 provider 明确报告的上下文上限恢复不受影响。

provider 明确报告的上下文上限错误使用
`CompactionTrigger::ContextLimit` 进入同一条即时宿主恢复路径，保留 provider 的 used/limit
字段，并受既有五次 context-compaction 预算约束。进程重启后的 Goal retry 只用于恢复此前
已持久化的 context-limit 失败，不再是进程内主动阈值越界的正常路径。

生成检查点和压缩后续跑使用独立的、任务中立的模板。`compaction/summary.md` 保存真实目标、
约束与决策、已验证进展、进行中的工作、阻塞项、下一步和引用；重复压缩时继承上一份有效
摘要，并按较新的明确指令纠正旧信息。只要请求保留了有效检查点，`compaction/continuation.md`
就作为原生 `runtime.compaction` 段加入请求，覆盖手动压缩、新用户输入和重启后的请求。
这一设计吸收 Codex 的上下文交接方式和 OpenCode 的增量摘要规则，具体任务及权限仍由
用户输入、当前模式和 Zuno 的持久状态决定，不限定为修复，也不新增用户授权。

宿主在同一次 drive 压缩后的第一个请求之前重新读取 Goal/Plan/Todo/Job。若本次驱动期间
Plan 已更新，一次性的“创建或替换 Plan”要求会转为维护当前 Plan；没有更新则保留原要求。
压缩标记不会取代真实用户 anchor、经验检索文本或已完成回合的学习窗口起点，旧版没有 `mode` 字段的标记也会
被正确排除。续跑通过原生运行时上下文完成，不合成另一条人类消息。

摘要请求包含解析后的 compaction agent 系统提示。hook 之后的完整请求、稳定段标识、
来源、摘要散列和运行上限会先记录为 `session.compaction.prompt`，摘要消息保存对应
receipt。辅助请求不会覆盖用于恢复 Skill 和执行来源的前台 prompt receipt。用量记录保留
分帧报告和缓存计数口径，单独记录的推理 token 不会再次计入可见输出。

只有非空、正常结束的响应才能发布检查点。重试回滚会清除该次尝试的残缺文本及用量；
缺少结束帧、长度截断、意外工具操作、超时或超出摘要字节上限都不会提交成功摘要。
响应流遵守 `compaction.timeout_seconds`、`compaction.max_summary_bytes` 和所属回合的
中断信号。取消会停止续跑，并保留后续重新压缩的能力。

创建标记与发布成功摘要分别使用原子事务，模型等待期间不持有事务。较新的失败或悬空
尝试不会撤销先前有效的边界；模型请求仅使用当前有效摘要，旧摘要和失败记录仍保留在库中。
下一次压缩显式传入上一份有效摘要，避免它落在原文保留区时漏掉更早的目标或并行工作。

工具调用配对信息和真实用户来源在文本转换之前保存，因此工具结果不会被计作用户回合，
压缩边界也不会拆散调用与结果；重复调用 ID 按前面的对应调用配对。较长的工具参数、输出
和推理使用有界首尾片段并标明省略，签名推理只保留标明性质的历史文本，加密推理不进入
摘要请求。原始持久证据以及保留区正常的 provider 工具协议保持不变。

自动压缩只在摘要落盘之后才咨询 auto-continue hook。该 hook 失败时会话保持
`Compacted`：摘要保留，不合成续跑回合（没有人投票就不授予续跑），失败原因随压缩结果以
`auto_continue_hook_failure` 记录并输出告警；会话不会被标记为失败，因此后续压缩不再被
`AlreadyFailed` 拒绝。工具 after-hook 失败同理：工具已经运行，结果保持自身状态与
`is_error`，hook 失败作为 `afterHookError` 元数据随结果返回，而不是被改写成一条会让模型
以为副作用没有发生、进而重复执行的裸错误。

### 工具取消的确定性

硬中断到来时，正在运行的工具会得到两秒的协作式清理窗口。在该窗口内结算，产生一次带类型的
`cooperative` 取消并保留工具自己的最终报告；窗口耗尽则强制中止该次调用，记为 `forced` 加
`uncertain`，重试前必须检查权威状态。`forced` 只表示宽限窗口已耗尽。

协作式取消并不自动等于结果确定。工作尚未得出结论就被停下的工具，会在其已结算结果的
`cancellation` metadata 键上声明这一点，dispatcher 也会把该次调用记为 `uncertain`，并把要求
检查权威状态的那句话追加到模型读到的报告里；没有作出声明的工具保持原有的确定读法，文本也不
改动。

`shell` 同时承载两种读法，而区分它们的是服务是否把某个状态结算为命令自己的判定，而不
只是进程有没有退出：取消被处理之前就已完成并报告了自己判定的运行，报告该退出状态并拿到正常
完成的运行所应得的凭据，只标记为已取消而不是不确定。其余每一次被取消的运行都保留已捕获的
输出、携带没有 exit authority 的 unresolved 凭据，并且属于不确定——包括那些确实报告了数字的
情况：属于子进程守护器自身失败的 `exit 125`、在硬上限处被杀掉的运行，以及运行报告过但被结算
为并非命令自身结局的状态。报告出的退出码仍留在结果里，因为那正是终端会显示的内容，所以一次
不确定的取消也可能给出退出码；拒绝为它背书的是凭据。两种读法都绝不会被机械重放。

解析出的判定随 `ToolDispatchInterrupted` 运行时事件一起发布，而不只是写入持久记录，因此
SSE 的 `tool.dispatch.interrupted` 载荷、ACP session update 与 `zuno run` 发布的 `uncertain`
与重放会话从持久 metadata 重建出的判定一致。

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

Goal continuation 是一等的回合来源。准备阶段会捕获确切 Goal id 与 revision；provider
工作开始前若 revision 已变化，这份 continuation 会失效。回合执行身份独立地从当前 host
捕获 Agent、目录 provider 与目录模型。保留的 user 历史只提供因果 transcript anchor，
不授予权限；Zuno 不会为了让重配置后的 host 看起来像历史状态而改写它，其中旧的
Agent/模型字段也不能再路由自动 Goal 回合。普通用户回合仍使用自身消息里的身份。
自动 Goal continuation 的第一次 provider 请求也会从 SQLite 注入与普通回合同源的
`runtime.work_state`，不会依赖旧的状态工具调用恢复当前 Plan/Todo。

Goal 完成审计与 Plan 写入方共用同一个 step status 类型。`completed` 与 `superseded`
都是终态；缺失、未知或旧的 `cancelled` 值会明确按持久 Plan 损坏失败关闭。模型在一次
`goal_update` 中结算 criteria 并完成 Goal 时，两者位于同一事务；审计拒绝会同时回滚
checklist 与 Goal revision。

Engine 在解析当前身份之后、发送 provider 请求之前写入一条
`session.turn.started.1`。事件记录 `turnTrigger`、`anchorMessageID`、Agent、provider 与
模型；Goal 回合还记录 `goalID` 与 `goalRevision`。provider attempt 事件重复 Goal 的触发
类型与已解析身份，使重试证据本身也完整。Agent 或模型解析失败时，
`session.turn.rejected.1` 会在 Goal 被阻塞前记录请求身份与类型化失败，不会伪造 started
或 provider-attempt 事件。

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

重试延迟是正数、有上限、带抖动，并且可被用户输入打断。有效的对端 `Retry-After` 会被限制到配置上限，且绝不会被更早的本地延迟替换。同请求 recovery window 从首次可重试 provider 失败返回后开始；对端要求的延迟超过剩余窗口时，回合以对端自身的类型化错误结束，Goal 级重试等待的是对端值按 `max_delay_ms` 截断后的结果，不再退回更短的本地退避。本地退避若超过窗口，则以 `provider_retry_deadline` 结束，并持久化最后的结构化 provider code、恢复耗时和总耗时。

重试决策使用类型化错误，而非渲染后的消息。认证失败与用户中断导致暂停；无效协议、损坏的持久状态和永久性配置失败导致阻塞。

围绕不可重放副作用的超时或响应丢失以 `uncertain_side_effect` 暂停：恢复必须检查权威状态，
绝不自动再次调用该工具。这份义务落在工具记录上，而不是 pause 上——dispatcher 在让结果对
模型可见的同一条语句里写入 `state.outcome = "uncertain"` 与 `state.uncertain`（工具名、
call id、该调用报告已改动的路径、类型化 `cause` 取 `lost_outcome` 或 `interrupted`，以及
`observedAtMs`）。进程若死在这次写入之后、pause 行落盘之前，Goal 仍然拒绝运行：下一次
continuation 会查询当前目标下仍待处理的记录并再次暂停。`state.uncertain.reconciledAtMs`
缺失的时长，恰好等于这次检查被拖欠的时长；查询范围由 Goal 自己的 `created_at_ms` 界定，
所以新目标不会继承上一个目标的义务。

读取或记账 Goal 预算时遇到 SQLite 争用（`SQLITE_BUSY`）会持久化一次 `database_busy` 指数退避重试，Goal 保持活跃，而不是以 `turn_budget` 暂停；其他数据库失败仍以 `usage_unknown` 停止回合并暂停 Goal；本构建无法读取的持久状态仍然阻塞。CLI 回合中的 Plan 对账驱动、human request 创建与重试上下文压缩标记路径也经同一 `GoalTerminalFailure::from_db_error` 规则分类：争用现在以 `database_busy` 重试，过去则以 `host_permanent` 阻塞。

`timeout`、`headerTimeout`、`chunkTimeout` 只有 OpenAI-compatible 传输会读取，其默认值是
330 秒响应头截止时间与 120 秒分片空闲上限。原生的 OpenAI、Anthropic、Google、Bedrock
四个 provider 不读这三个键：它们固定采用 330 秒响应头截止时间，不设整请求截止时间（一个
合理的长回合没有 provider 能事先知道的上限），分片阶段则沿用共享的 300 秒流空闲上限，
该上限由 `ZUNO_STREAM_IDLE_TIMEOUT_SECS` 对所有 provider 统一调整。原生请求在收到第一个
响应头之前卡住时，现在会在上限处以类型化错误失败，而不是一直等到用户中断。

Provider 条目中的类型化 `retry` 块不是 SDK option。`max_attempts` 包含首次请求，
`recovery_window_ms` 则只在首次请求返回可重试错误后开始；rollback、退避和所有替代尝试
必须在窗口内完成。默认值是 3 次尝试、180 秒恢复窗口、2 秒初始延迟、30 秒最大延迟和
20% 抖动。策略随解析后的 provider 冻结，绝不会发送给上游。

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
- 多步骤工具回合中的主动上下文阈值同样走这条类型化路径。第一次之后的每次 provider
  请求都会在发送前检查：有 provider 报告时使用上一响应的上下文用量，否则使用当前
  assembled prompt estimate；触顶时发出 `context.compact`，压缩保留历史并重试。
  `compaction.auto: false` 会关闭这项主动触发。
- 每次 provider 请求前后都会咨询回合预算策略。默认 profile 发布
  `TurnAllowance::UNLIMITED`，不会为未设预算的 Goal 猜测 token 上限，也不会默认设置工具
  调用次数或墙上时间上限。`goal.default_token_budget` 可为没有自身 `token_budget` 的 Goal
  配置宿主兜底；Goal 自己的显式预算始终优先。自定义 profile 还可设置工具调用次数与墙上时间
  上限；两者无论有没有 Goal 都生效，触顶时以 `tool_call_budget` 或 `time_budget` 停止。
  在用户显式设定的 Goal 预算下，用量不可测时以 `usage_unknown` 停止；配置的宿主兜底额度
  则继续执行，只按已计数的用量生效。上限优先于压缩请求或继续执行，但让位于 Goal 自身已产生的停止。
  每次停止都是类型化的 `TurnError::BudgetLimited`，并以 `notice` 事件
  （code 为 `budget.<kind>`）投影给客户端；压缩请求的 code 为 `budget.compact`。

自动 Goal 续跑会检查 provider 真正保留的历史，而不是整个 message 表。若压缩边界从 assistant
消息开始，导致保留后缀里没有真实 user turn，宿主会在下一轮前把 Goal objective 重新持久化为
user anchor；合成的 compaction marker 永远不被当成用户授权。

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

运行中的 Skill catalog 只监听精确的项目 Skill 根目录，不把整个 worktree 作为监听根。
从会话目录到 worktree 的每一层，在未禁用项目配置时都会保留 `.zuno/skill` 逻辑根，
在未禁用外部 Skill 时都会保留 `.agents/skills` 逻辑根。规范用户根目录
`$XDG_CONFIG_HOME/zuno/skill` 与显式配置路径也会在尚不存在时保留。

根目录尚不存在时，只非递归监听最近的已有父目录；目录出现后，消费任务会在 native
watcher 回调之外安全地逐级收窄订阅，并且只在精确根目录存在时开启递归监听。ignore
过滤属于事件策略，不能替代有界的 native 注册；Zuno 不会为了发现未来的 Skill 根而
递归监听整个 worktree。

Zuno 不监听 `~/.zuno` 或远端 Skill 缓存；缓存只在配置远端索引并实际下载时按需创建。
标准共享根 `~/.agents/skills` 只在启动时已经存在时监听，其他共享目录需要通过
`skills.paths` 显式配置以支持运行中安装。

Shell 执行受 OS 沙箱约束。`read-only` 与 `workspace-write` 都要求一个已验证的约束后端，默认在不可用时拒绝启动而非降级；受信层可以用 `sandbox.onUnavailable: run-unconfined` 让具备写能力的请求在符合条件的不可用错误下降级，或用 `sandbox.backend: native` 让每个 Agent（包括只读 Agent）显式走原生后端并保留权限模式，持久化记录中的 `resolutionKind` 为 `trusted_native`。两者都不是沙箱隔离。详见 [权限与沙箱](/zh/guide/permissions)。

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

启动 `zuno` 本身不会往这棵树里插入任何进程，CLI 自身的启动也不是被守护的进程。客户端无论
启动 `zuno acp`、`zuno serve` 还是任何其他调用，在所有受支持平台上监督的都只有一个进程：
它启动的那个进程就是执行命令的进程，结束它就结束命令，命令的 `stdout` 与 `stderr` 也随之
到达文件末尾。启动过程把全局选项与 `ZUNO_*` 变量解析成一个进程内的值；Unix 上还会替换一次
自身镜像并保持相同的进程 ID，好让之后启动的程序继承这些值，没有镜像替换的平台则直接在调用方
派生的那个进程里分派命令，不会派生第二个进程。任何一次 `zuno` 调用都不会再启动第二个 `zuno`
来携带这份环境，因此启动 Zuno 永远不会武装上面那个 PowerShell 父进程监视助手，Windows
PowerShell 仍然只是那个守护器的后端依赖，而不是运行 CLI 的依赖。参见[一次调用就是一个进程](/zh/cli/#一次调用就是一个进程)。

## 后台命令执行

后台命令有独立的生命周期与输出游标，父会话通过持久状态观察它们，而不是靠轮询。

`bg` 工具为当前会话拥有的执行提供 `list`、`output`、`wait`、`cancel`，另有 `artifact`
用于读回被输出上限从该会话任一工具那里扣留的输出。`output`、`wait`、`artifact` 都接受可选的
`cursor` 与 `limit`，并返回下一个窗口的起始游标：`limit` 默认 16384 字节并被夹取到 51200
（默认输出字节上限的数值，不随配置的 `tool_output.max_bytes` 变化），超限是夹取而不是报错，
因此一次读回绝不会比内联结果给出更多字节。早于 2 MiB 常驻环形缓冲的游标会落到执行的磁盘
`.output` 文件上并报告 `fromDisk`，让已被丢弃的前缀重新可达，而不是把请求向前夹取。
`artifact` 是扣留通知点名的读回方式，也是唯一一种既不重跑产生这些字节的调用、又不让窗口再次
经过扣留它的输出上限的读法：隐藏 `bg` 的 agent 配置会让自己被扣留的输出只能靠
`accept_large_output: true` 重跑那次调用才能读到。

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

模型 provider 请求、Zuno 自管认证、目录、远程 instructions、远程 MCP 与 web tools
共用 `zuno-network`。AWS 凭据发现是有意的例外：`zuno-aws-auth` 把标准凭据链、刷新与
SigV4 签名交给 AWS Rust SDK；签名后的 Bedrock 模型请求仍通过 `zuno-network`，因此继续
遵循进程代理。IAM Identity Center、STS、web identity、容器凭据与 IMDS 等凭据 provider
流量由 AWS SDK 自己负责，并遵循所选 SDK 版本的 HTTP 配置。

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
