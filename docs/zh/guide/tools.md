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

`edit`、`execute` 和 `lsp` 作为已注册的槽位存在，但不属于默认工具面。`edit` 仍可供显式构造的 profile 使用；默认的编辑路径是 `apply_patch` 加 `write`。

`glob` 与 `grep` 驱动官方的 `rg` 可执行文件，Zuno 只贡献带类型的参数、取消、有界解码和稳定排序。必须有 ripgrep 14 或更新版本可用；缺失时工具运行时报启动错误，而不是静默回退到更慢的遍历器。

## 副作用分类

每次调用都归为四种副作用之一，而默认是最严格的那一种：

| 副作用 | 含义 | strict 模式下的批准 |
| --- | --- | --- |
| `ReadOnly` | 观察状态而不改变它 | 不需要 |
| `UserMediated` | 按设计需要人类输入 | 不是批准面 |
| `Delegating` | 运行携带自身副作用的子级工作 | 每次子级调用重新评估 |
| `SideEffecting` | 默认。改变状态或触达外部 | 需要一次新的批准 |

由于 `SideEffecting` 是默认值，一个未知的 harness 或 MCP 工具会失败即拒绝。混合型工具可以基于校验后的参数来分类：`bg list`、`bg output` 和 `bg wait` 是只读的，而 `bg cancel` 有副作用。

原生读取、`glob`、`grep`、skill 与会话与 job 检查、只读 LSP、MCP 资源读取、`webfetch` 和 `web_search` 不会收到额外的 strict 询问。Shell、文件写入、持久状态变更、委派、产品 Agent、扩展生命周期变更以及未知的 MCP 工具会收到。

## 重放策略

工具执行默认是至多一次。除非某个实现显式声明 `Safe`，否则继承 `Never`，而当前的安全集合都是只读或幂等的检查类操作：文件读取、glob、grep、skill 查找、会话搜索、job 状态、LSP 检查、goal 状态，以及网络搜索或获取。

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
- [Agent](/zh/guide/agents)
- [MCP server](/zh/guide/mcp)
- [Harness 运行时](/zh/operate/harness-runtime)
