# 工具

工具是让模型真正去做事、而不是描述做事的东西。在 Zuno 中，一个工具携带三项互相独立的声明：它有什么副作用、是否可以被重复执行、以及是否可以与其他调用重叠。把这三者分开，才能在实现安全并行的同时不让重试变得危险。

## 默认工具面

默认对模型可见的工具面刻意保持精简：

| 工具 | 用途 | 副作用 |
| --- | --- | --- |
| `read` | 读取一个文件或目录 | 只读 |
| `glob` | 按模式查找文件 | 只读 |
| `grep` | 搜索文件内容 | 只读 |
| `write` | 创建文件，或有意整体替换一个文件 | 有副作用 |
| `apply_patch` | 局部的、经上下文校验的源码编辑 | 有副作用 |
| `shell` | 在当前沙箱下运行命令 | 有副作用 |
| `bg` | 检查或取消后台命令 | 只读检查；`cancel` 有副作用 |
| `task` | 把一个有界目标委派给另一个 Agent | 委派型 |
| `job` | 检查后台 job 状态 | 只读 |
| `webfetch` | 获取一个 URL | 只读 |
| `web_search` | 批量网络搜索 | 只读 |
| `skill` | 发现并加载可复用指令 | 只读 |
| `question` | 在 Plan 中向用户提出结构化澄清问题 | 用户中介型 |

持久工作状态会额外加入 `plan_get`、`plan_update`、`todo_get`、`todo_update` 以及 `goal_get`/`goal_update`。启用记忆时会出现 `memory_propose`，当前 Agent 能够触达时会出现 `council_run`。

当已连接的 MCP 工具通过能力与权限过滤后，Zuno 仍保留其可执行实现，但默认不把所有
完整 JSON schema 注入模型请求。此时会条件性出现 `tool_search`：它搜索紧凑元数据，
匹配项从下一次模型 step 开始进入可见工具集。这样无需在每次请求中支付所有外部服务的
提示词成本。参见 [MCP server](/zh/guide/mcp)。

可选的连续性工具只有在 `continuity` 开启，并且通过最终工具与权限过滤后才会出现：

| 工具 | action | 作用域 |
| --- | --- | --- |
| `history` | `list_windows`、`list_items`、`read_item`、`search_contents` | 当前会话的规范化证据 |
| `notes` | `list_files_by_prefix`、`read_file`、`search_contents`、`append_to_file`、`write_file` | 当前会话与 Agent 的逻辑文档 |

History 只把成功压缩作为窗口边界。返回内容排除 reasoning、加密值、合成的内部提示
正文和二进制附件字节，并且只能作为数据而不是指令处理。

Notes 从不暴露宿主路径。每个作用域最多 100 个文档，单文档最多 256 KiB，总计最多
1 MiB。两个写 action 都必须携带精确的 `expected_revision`；只有新建文档时使用
`0`。可信 `call_id`、请求摘要和 revision 让重复投递保持幂等，同时拒绝过期的并发写入。

宿主分类器决定请求是否需要持久战略 Plan，但不会生成用户可见的通用步骤。模型用
`plan_update action=create` 创建首个 Plan 或替换新目标；`patch` 只修改指定 id，
`append` 追加由宿主生成 id 的步骤，`push` 打开聚焦子 Plan，`pop` 不重传整份 Plan
而只恢复精确父 Plan。所有已有 Plan 修改都必须带当前 `expected_revision`；
`completed` 和 `superseded` 都是终态。

成功交付前，durable reconciliation driver 会检查 Plan、Todo、Job、Goal、工具结果与
验证记录。普通会话最多执行两次对账续跑，仍不一致则进入 typed `PlanUnreconciled`
人工等待，而不是声称完成。禁用 `plan_update` 会阻止模型创建或修改；已有 Plan 仍会
持久化、投影并恢复。

完整的开启/关闭、profile 覆盖、权限、revision 与重启说明见
[History 与 Notes 连续性配置](/zh/config/continuity)。

`edit`、`execute` 和 `lsp` 作为已注册的槽位存在，但不属于默认工具面。`edit` 仍可供显式构造的 profile 使用；默认的编辑路径是 `apply_patch` 加 `write`。

## `apply_patch` 冲突恢复

`apply_patch` 使用稳定的 SHA-256 读取凭据与读取代次，而不是信任模型记忆中的文本。
已有文件作为 `update`、`delete` 或 `move` 的源时必须先读取；已经存在的 move 目标也必须
先读取。工具在同一把 mutation lock 内完成全部文件的预检，全部通过后才写入，因此任一
预检冲突都会保证零写入。

变更冲突是类型化、可由模型修正、但绝不会自动重放的结果：

| 冲突 | 含义 | 必须执行的恢复动作 |
| --- | --- | --- |
| `ReadRequired` | 当前文件没有有效读取凭据 | 先读取错误中指定的资源，再构造变更 |
| `StaleRead` | 文件在读取后发生了变化 | 重新读取当前文件，并基于新内容重建操作 |
| `ContextMismatch` | hunk 与当前逻辑行不再匹配 | 读取指定 hunk 附近，使用新鲜且唯一的上下文生成更小补丁 |
| `IdenticalReplay` | 相同操作摘要再次作用于相同文件内容 | 不得原样重发；修改操作，或等待文件真实变化后重新读取 |

冲突会携带资源、操作摘要、当前内容摘要、适用时的 hunk 编号/标题，以及明确的
`requiredAction`。匹配以逻辑行为单位，同时保留原文件的 BOM、LF/CRLF 风格和末尾换行
状态。补丁语法错误仍属于 `InvalidArgs`。如果写入后才发生 I/O 或格式化失败，结果属于
`Uncertain`，列出已经观察到变更的路径，并要求先检查实际状态，禁止机械重放。

`webfetch` 只接受无凭据 HTTP(S) 目标。Zuno 会解析并校验全部地址，整体拒绝公私混合
DNS，固定已经校验的地址，并在最多五次重定向的每一跳重复校验。它遵循进程级
`HTTP_PROXY`、`HTTPS_PROXY`、`ALL_PROXY` 与 `NO_PROXY`，通过 HTTP、HTTPS、
SOCKS4 或 SOCKS5 代理连接已经校验的目标 IP，同时保留原始 Host 与 TLS SNI。
代理失败不会静默改为直连。默认超时 30 秒，单次最大 120 秒；超时错误会报告 route、
phase 与 elapsed。

`web_search` 不会把 provider 凭据或完整 wire URL 放进错误与日志。诊断只标识 provider、scheme、host、path、状态与错误类别，不包含 API key、认证头或完整 query。

`glob` 与 `grep` 驱动官方的 `rg` 可执行文件，Zuno 只贡献带类型的参数、取消、有界解码和稳定排序。必须有 ripgrep 14 或更新版本可用；缺失时工具运行时报启动错误，而不是静默回退到更慢的遍历器。

## Shell 退出状态能证明什么

管道的退出码就是最后一段的退出码。`cargo test | tail -5` 在测试失败时仍然退出
0，因为决定它的只有 `tail`。因此 Zuno 默认为每次 `shell` 调用启用
`set -o pipefail`：管道中任何一段失败，都是整条命令的失败。

`exitPolicy` 用来显式选择这一行为：

| 取值 | 效果 | 退出状态的权威性 |
| --- | --- | --- |
| `pipefail`（默认） | 管道中任何一段失败即命令失败 | 在 `bash`、`ksh`、`zsh` 上是权威的 |
| `all` | 另外在序列中第一条失败的命令处停止 | 权威 |
| `last` | 只由最后一段决定，即 POSIX 默认 | 推导得出 |

只有当命令本身允许某一段失败时，才应刻意选择 `last`。

每个结果都带一份验证凭据，说明运行了什么、判定结果、退出码，以及该退出码的覆盖
范围。这一区分正是重点：`derived` 状态会被记录，但不构成证据，因为该退出码来自
一个根本没看到失败的阶段。解释器同样重要。`set -o pipefail` 不属于 POSIX，而
`dash` 在被要求启用它时会直接退出、根本不运行命令；因此不在已知可用集合内的解释
器只报告 `derived`，并在凭据中写明自身。`sh` 被刻意视为未知，因为这个名字并不说
明背后是什么。PowerShell 没有对应机制，所以在那里只有 `all` 是权威的。

已存储的凭据由一个标识符寻址，它以 `[verification rcp_…]` 的形式出现在工具结果
中。能够满足"需要证据"的 Goal 完成判据的，是这个标识符，而不是对会话记录的回忆。

## 副作用分类

每次调用都归为四种副作用之一，而默认是最严格的那一种：

| 副作用 | 含义 | strict 模式下的批准 |
| --- | --- | --- |
| `ReadOnly` | 观察状态而不改变它 | 不需要 |
| `UserMediated` | 按设计需要人类输入 | 不是批准面 |
| `Delegating` | 运行携带自身副作用的子级工作 | 每次子级调用重新评估 |
| `SideEffecting` | 默认。改变状态或触达外部 | 需要一次新的批准 |

由于 `SideEffecting` 是默认值，一个未知的 harness 或 MCP 工具会失败即拒绝。混合型工具可以基于校验后的参数来分类：`bg list`、`bg output` 和 `bg wait` 是只读的，而 `bg cancel` 有副作用。

原生读取、`glob`、`grep`、skill 与会话与 job 检查、`tool_search`、只读 LSP、MCP
资源读取、`webfetch` 和 `web_search` 不会收到额外的 strict 询问。Shell、文件写入、
持久状态变更、委派、产品 Agent、扩展生命周期变更以及未知的 MCP 工具会收到。

混合型工具按通过校验后的 action 决定策略。所有 `history` action 和三个 Notes 读取
action 都是 `ReadOnly`；Notes 追加与替换是 `SideEffecting`。缺少或未知的 Notes
action 会按 `SideEffecting` 失败即拒绝。

## 重放策略

工具执行默认是至多一次。除非某个实现显式声明 `Safe`，否则继承 `Never`，而当前的
安全集合都是只读或幂等的检查类操作：文件读取、glob、grep、skill 查找、当前会话
History、Notes 读取、job 状态、LSP 检查、goal 状态、外部工具元数据搜索，以及网络
搜索或获取。Notes 写入保持 `Never`。

| 策略 | 失败后的行为 |
| --- | --- |
| `Never` | 把结果落盘、下一步交给模型，并要求在任何新的变更之前先检查权威状态 |
| `Safe` | 可以在退避之后再次尝试 |

循环绝不会机械重放一次调用。副作用附近的超时或响应丢失是一个不确定的结果，包括外部效果可能在响应丢失前就已完成的情况。后续的恢复回合会收到一条指明重试尝试的通知，它来自 SQL，而不是对话记录里的一段散文。

这就是为什么一条超时的 shell 命令不会被简单地再跑一次。

## 并发

重叠是与重放安全性相互独立的一项声明。

| 策略 | 行为 |
| --- | --- |
| `Exclusive` | 默认。一道双向屏障：更早的重叠调用先结算，并且在它结算之前不启动任何更晚的调用 |
| `ParallelSafe` | 可以在配置的上限内与其他非独占调用重叠 |
| `IsolatedBackground` | 可以在前台路径之外运行时重叠 |

调度器按模型给出的顺序解析工具、校验参数、运行 hook 并请求权限。随后它在上限内执行连续的非独占调用，并按原始调用顺序落盘结果，与物理完成顺序无关。Shell、写入、未知的扩展工具，以及没有显式声明的 MCP 工具，仍然保持独占。

```json
{
  "concurrency": {
    "tool_calls": 8,
    "delegations": 8,
    "mcp_connections": 8,
    "lsp_requests": 4
  }
}
```

每个字段都接受 `1..=64`。把某一项设为 `1` 会让该层恢复串行行为。

## 启用与禁用工具

顶层 `tools` 映射是按工具名索引的逐工具开关：

```json
{
  "tools": {
    "webfetch": false
  }
}
```

这是可用性，不是授权。授权是 `permission.rules`，并且一个 Agent 还可以声明一份确切的 `tools` 允许列表，它替换而不是扩展默认工具面。参见[权限与沙箱](/zh/guide/permissions)和[自定义 Agent](/zh/config/custom-agents)。

## 拒绝是一种结果，不是失败

参数格式错误、工具不可用和权限拒绝，都会在追加对模型可见的错误结果之前，先发出一个带 `invalid_arguments`、`unavailable` 或 `denied` 的 dispatch-blocked 事件。持久状态保留 `outcome: "blocked"` 及阻塞类别，因此客户端可以说明请求的效果从未执行，而不是暗示它执行到一半失败了。

进程、传输和实现层面的失败仍然是 error 结果。读对话记录时值得记住这个区别：blocked 意味着什么都没发生。

## 输出上限

```json
{
  "tool_output": {
    "max_bytes": 51200,
    "max_lines": 2000
  }
}
```

以上是默认值。超出的输出会被截断，而不是被允许吃掉模型窗口。

## 参见

- [权限与沙箱](/zh/guide/permissions)
- [History 与 Notes 连续性配置](/zh/config/continuity)
- [Agent](/zh/guide/agents)
- [MCP server](/zh/guide/mcp)
- [Harness 运行时](/zh/operate/harness-runtime)
