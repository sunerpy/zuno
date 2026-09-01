# Zuno 是什么？

Zuno 是一个以 Rust 可执行文件交付的本地编程 Agent。它可以检查仓库、修改文件、执行命令、
委派有边界的工作，并汇报已经完成的验证。

它重点解决的是工作的连续性：Provider 请求失败、进程退出或切换客户端后，任务仍应保留
已记录的状态和权限边界。

## 持久工作状态

Zuno 会话以事件形式存储在 SQLite 中。用户输入、组装后的 Prompt、工具结果、重试通知和
子 Agent 报告，都会在下一次模型请求需要它们之前落盘。

Goal、Plan、Todo 和后台 Job 是彼此独立的持久记录：

- Goal 定义结果和可选预算；
- Plan 记录有顺序的实施步骤；
- Todo 跟踪更小的工作项；
- Job 跟踪委派 Agent 和后台命令。

可恢复的 Provider、流、网络和数据库错误使用持久化重试时间。Zuno 重启后会重建该时间，
而不是重新开始一套重试循环。工具若在副作用附近出现不确定结果，只会记录并等待检查，
不会被机械重放。

参见 [Goal、Plan 与 Todo](/zh/guide/durable-state)和
[会话与回合](/zh/guide/sessions)。

## Agent 决定权限边界

所选 Agent 决定本次运行有哪些工具和委派路径。

| Agent | 用途 |
| --- | --- |
| `orchestrator` | 默认主 Agent，拆解工作并验证委派结果 |
| `build` | 端到端实现与验证 |
| `plan` | 只读调查与规划 |
| `deep` | 困难的跨领域实现，不递归委派 |

其他专职 Agent 负责探索、资料检索、评审、视觉检查和聚焦修复。Agent 契约可以移除工具或
委派对象，但不能取得超过父运行时的权限，因此子会话无法获得父级没有的工具。

Council 和 Workflow 的拓扑来自配置：席位、quorum、并发、路由、截止时间和重试策略都在
模型提出问题之前确定。

参见 [Agent](/zh/guide/agents)和[编排与委派](/zh/guide/orchestration)。

## Shell 执行有多道独立门禁

Shell 请求依次经过工具参数校验、权限策略、命令风险检查和执行后端。这些控制彼此不能替代。

在 Linux 上，`read-only` 和 `workspace-write` 使用 bubblewrap、capability drop 与 seccomp。
请求的约束无法部署时，Zuno 默认拒绝命令。受信配置可以直接选择
`danger-full-access`，也可以只在符合条件的沙箱不可用错误下允许
`workspace-write` 降级。只读 Agent 不使用该降级。

macOS 与 Windows 尚未实现受限后端；具备写能力的 Agent 可以在这些平台显式选择原生执行。

参见[权限与沙箱](/zh/guide/permissions)。

## 一套运行时，多个客户端

TUI、headless runner、ACP server 和 HTTP server 使用相同的会话命令、持久事件、inbox 和
projection。客户端断开不会产生另一套 Agent 生命周期。

运行时由类型化 Rust `Component` 组合。组件通过 `HarnessProfile` 注册服务和副作用；
替换 Profile 时，新 Profile 完整校验后才会发布，挂载失败则按逆序回滚。

扩展可以提供 Agent、Workflow、Skill、WASI Component 或受控进程工具。Zuno 不加载 Rust
动态库。`glob` 与 `grep` 使用外部的 `rg`（ripgrep）14 或更新版本；缺少它不会阻止
Zuno 或其他无关工具启动，只有调用搜索工具时才会报告该能力不可用。

参见 [Harness 运行时](/zh/operate/harness-runtime)、[插件与扩展](/zh/guide/plugins)以及
[Harness 对比](https://github.com/sunerpy/zuno/blob/main/docs/design/harness-comparison.md)。

## 当前边界

- Zuno 仍处于 0.x 早期开发阶段；数据和扩展格式会通过文档化的版本与迁移边界演进。
- 它是本地 CLI 与 Server，不是托管式编程服务。
- 它使用 Zuno 自己的配置和协议，不提供其他编程 Agent 的兼容层。
- 受限 Shell 目前只在 Linux 上实现。
- Provider 与模型需要显式配置。

## 从哪里开始

| 目标 | 页面 |
| --- | --- |
| 安装并运行 Zuno | [快速开始](/zh/guide/quick-start) |
| 理解会话和恢复 | [会话与回合](/zh/guide/sessions) |
| 选择 Agent | [Agent](/zh/guide/agents) |
| 配置命令权限 | [权限与沙箱](/zh/guide/permissions) |
| 接入 Provider | [Provider 与凭据](/zh/config/providers) |
