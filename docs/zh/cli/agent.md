# zuno agent

Agent 各自携带自己的提示、模型、权限与委派边界。`zuno agent` 就是检查当前配置链实际解析
出什么的那个面 —— 在全局定义与项目定义相互重叠时这尤其重要。

`list` 报告从这里可见的 Agent，并逐个给出本构建为其强制执行的权限规则。编写 Agent 就是在
`.zuno/agent/` 下写一个 Markdown 文件；该文件接受的 front matter 见
[自定义 Agent](/zh/config/custom-agents)。

## 用法

```sh
zuno agent [OPTIONS] <COMMAND>
```

## 选项

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | 选择受限 Shell 无法部署时的处理方式。可选值：`deny`、`run-unconfined` | `deny` |
| `--sandbox-backend <BACKEND>` | 为本次调用选择 Shell 执行后端；`native` 不是沙箱隔离。可选值：`auto`、`native` | `auto` |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

## 子命令

| 子命令 | 说明 |
| --- | --- |
| [`list`](#zuno-agent-list) | 列出当前配置链解析出的 Agent |
| `help` | 打印本消息或给定子命令的帮助 |

### zuno agent list

```sh
zuno agent list [OPTIONS]
```

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | 选择受限 Shell 无法部署时的处理方式。可选值：`deny`、`run-unconfined` | `deny` |
| `--sandbox-backend <BACKEND>` | 为本次调用选择 Shell 执行后端；`native` 不是沙箱隔离。可选值：`auto`、`native` | `auto` |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

## 示例

列出从当前配置链解析出的 Agent。

```sh
zuno agent list
```

写出 `.zuno/agent/reviewer.md` 之后，检视该 Agent 完全解析后的定义。

```sh
zuno debug agent reviewer
```

## 参见

- [全局选项](/zh/cli/global-options)
- [zuno run](/zh/cli/run)
- [zuno tui](/zh/cli/tui)
- [zuno debug](/zh/cli/debug)
- [配置参考](/zh/config/reference)
- [编排](/zh/guide/orchestration)
