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

## Agent 与提示词契约

Agent 具有显式的正向职责、负向委派边界、权限以及结构化输出预期。

内置 Agent 的分工：`build` 负责端到端交付，`plan` 是只读规划，`deep` 承担困难的跨领域实现且不再递归委派。

根回合对已连接 MCP schema 使用渐进式披露：调度器保留可执行实现，`tool_search`
只搜索紧凑元数据，匹配项从下一次 provider step 起按单调 revision 扩展确切工具快照。
Agent 的确切 `tools` 允许列表会立即公开其中点名的 MCP schema；子级仍受父级 Attempt
中已持久化的确切 schema 上限约束，不能搜索出更大的权限面。ACP session-local
`mcpServers` 在严格连接门禁后也立即公开，但同一目录中的宿主配置 server 仍延迟发现；
Catalog 会把这个会话边界传递到子回合与后台续跑。

## 扩展包与可执行插件宿主

扩展要么是显式 WASI 授权下的 WebAssembly 组件，要么是使用行分隔 JSON-RPC 的受限子进程。能力必须声明，不会被默认赋予。

详见 [插件与扩展](/zh/guide/plugins)；完整 WASI guest 与原生 Rust 实现路径见
[开发 Agent 与扩展](/zh/guide/extension-development)。

## 提示词溯源

**模型可见即被记录。** 每一个提示词分段、外部输入、工具结果、重试通知和子 Agent 报告，只要它能改变一次模型请求，就必须能从持久会话事件中重建。

提示词组装使用稳定的分段标识、确切来源、有序内容和内容摘要。实际经过 hook 之后的提示词在 provider 请求发出之前落盘。

## 可审计的记忆与反思

记忆写入是提议而非直接生效。候选进入待评审状态，由人决定是否提升为常驻记忆，并且可以撤销。

## 持久输入

用户提示词、steering 以及子 Agent 报告在执行前进入持久 FIFO 收件箱。`reportDelivery: nextStep` 必须完成子结果结算、准许父级输入并唤醒父级，且不存在轮询竞态。

图像入口在写入 inbox 前统一经过 `AttachmentStore`：规范化方向、像素与编码，原子发布当前数据库身份下的内容寻址对象，持久 part 只保存 `ImageAttachmentRef`。Provider 请求组装时才校验并内联对象；缺失或 digest 不符是永久持久状态失败，不回退原始路径。

## Plan 与 Work 状态迁移

持久的 Goal、Plan、Todo、收件箱和 job 状态控制续跑，而不是自然语言。「接下来我会……」
这类文字不构成进展。默认 profile 发布类型化的宿主 Planning capability；即使最终工具
过滤隐藏了 `plan_update`，宿主仍会创建、持久化并在重启后恢复 Plan。明确继续会维护
当前 Plan；新的实质性目标归档旧 Plan 并安装只含新目标步骤的新根 Plan。临时聚焦工作
通过 `action=push` 暂停父 Plan，子步骤全部完成后由 `action=pop` 精确恢复父 Plan 一次。
工作状态变化时立即更新，并在最终回复前对账。隐藏工具只会移除模型修改入口。

## 原生会话命令、压缩与硬中断

会话命令、上下文压缩与硬中断都是原生能力，不依赖模型配合。

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

可恢复的 provider、网络、流、SQLite 争用、回合预算和符合条件的工具失败，会在等待前先持久化一次指数退避重试。进程重启后从 SQLite 重建截止时间。

重试延迟是正数、有上限、带抖动，并且可被用户输入打断。有效的对端 `Retry-After` 会被限制到配置上限，且绝不会被更早的本地延迟替换。

重试决策使用类型化错误，而非渲染后的消息。认证失败与用户中断导致暂停；无效协议、损坏的持久状态和永久性配置失败导致阻塞。

## 原生搜索与 Shell 隔离

搜索把遍历委托给 ripgrep 本身，Zuno 不维护第二个 ripgrep 兼容的遍历器。缺少 `rg` 是工具运行时的启动错误，而不是静默回退。

Shell 执行受 OS 沙箱约束。`read-only` 与 `workspace-write` 都要求一个已验证的约束后端，不可用时拒绝启动而非降级。详见 [权限与沙箱](/zh/guide/permissions)。

## 常驻进程约束

常驻进程在声明的约束内运行，其生命周期由运行时拥有。

Unix PTY 通过前台守护进程拥有进程组与终端前台切换。Windows ConPTY 则直接启动请求的
终端程序：不能把常驻 Job Object 守护器嵌入 ConPTY，否则交互输入与自然退出都可能无法
收敛。PTY 所有者持有直接子进程 PID，先应答并移除后端的一次性继承游标查询，再向客户端
转发终端输出；关闭 writer/master 后发布退出，显式停止通过 `taskkill /T` 终止完整
子进程树。

## 后台命令执行

后台命令有独立的生命周期与输出游标，父会话通过持久状态观察它们，而不是靠轮询。

## 后台子 Agent 与产品 Agent

工具执行默认是至多一次。`ToolReplayPolicy::Never` 是默认值；只有显式声明为只读或幂等的工具才可以声明 `Safe`。

副作用附近的超时或响应丢失属于结果不确定。这种情况会被持久化，要求检查权威状态，绝不机械重放调用。

`subagent_model_selection` 默认关闭。开启后，精确 model allowlist 会在 profile 激活时解析，并按 session 持久冻结为带 digest 的策略；`task` 才会出现可选 `model`/`effort`。续跑不能改变首次冻结的模型或强度。

## 并发网络搜索

`web_search` 接受一批查询，并在单查询 provider 之上拥有并发、取消、稳定排序、限流与 URL 去重。

## 网络出口

网络出口受沙箱的网络授权控制。`deny` 会创建私有网络命名空间并拒绝网络系统调用，而不是一条可被绕过的防火墙规则。

公开网页抓取使用独立 `PublicHttpClient`：只接受无凭据 HTTP(S)，直连且不使用环境代理；每次请求和每次重定向都重新解析、校验全部地址并进行 DNS pinning。公私混合 DNS、回环/私网/链路本地/CGNAT/保留地址，以及 IPv4-mapped IPv6 与 NAT64 中嵌套的非公开地址都会整体拒绝。

WebSearch 的带密钥 wire URL 不进入诊断。错误只保留 provider、scheme、host、path、状态与类别，reqwest cause 在进入错误链前移除 URL。

## 提示词工作流 V2 验收

提示词工作流 V2 的验收条件与证据记录在 [提示词与工作流指南](/zh/operate/prompt-workflow) 与 [设计文档](https://github.com/sunerpy/zuno/blob/main/docs/design/prompt-workflow-v2.zh-CN.md)。

## 构建一个 harness

优先通过 `ProfileBundle` 与 `HarnessProfile` 进行组合。部署选择与可调参数属于经过校验的 profile 或配置字段。

新行为使用文档化的扩展点。改变默认 Agent 循环需要在同一次变更中更新英文版 harness 运行时文档。

## 客户端界面

客户端界面消费持久事件、收件箱状态和投影。TUI、server、ACP 以及未来的 GUI 客户端不得获得私有的 Agent 循环行为。

`zuno run --show-reasoning` 只把 provider 明确提供的 reasoning delta 用稳定区块写入 stderr，最终答案继续只写 stdout；signed/encrypted reasoning 永不显示，且不能与 JSON 格式组合。

`zuno serve --browser-auth` 是显式的纯回环模式：单次 256-bit 启动 token 换取绑定 authority 的 30 天签名 Cookie；Basic Auth 与 Cookie 任一有效即可授权，Cookie 的非安全方法还要求精确 Origin。bootstrap query 在访问日志前被脱敏。

## 参见

- [Harness Runtime（英文完整版）](https://github.com/sunerpy/zuno/blob/main/docs/harness-runtime.md) —— 逐节权威契约
- [权限与沙箱](/zh/guide/permissions) —— 沙箱与权限的两个门禁
- [编排与委派](/zh/guide/orchestration) —— 委派边界与模型路由
- [Goal、Plan 与 Todo](/zh/guide/durable-state) —— 持久状态如何控制续跑
