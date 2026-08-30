# zuno agent

Agent 各自携带自己的提示、模型、权限与委派边界。`zuno agent` 就是编写 Agent、以及检查当前
配置链实际解析出什么的那个面 —— 在全局定义与项目定义相互重叠时这尤其重要。

`create` 写出一个新的 Agent 定义；`list` 报告从这里可见的 Agent。

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
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

## 子命令

| 子命令 | 说明 |
| --- | --- |
| [`create`](#zuno-agent-create) | |
| [`list`](#zuno-agent-list) | |
| `help` | 打印本消息或给定子命令的帮助 |

### zuno agent create

```sh
zuno agent create [OPTIONS]
```

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `--path <PATH>` | | |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--description <DESCRIPTION>` | | |
| `--mode <MODE>` | 可选值：`all`、`primary`、`subagent` | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--permissions <PERMISSIONS>` | 别名：`--tools` | |
| `-m`, `--model <MODEL>` | | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | 选择受限 Shell 无法部署时的处理方式。可选值：`deny`、`run-unconfined` | `deny` |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

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
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

## 示例

列出从当前配置链解析出的 Agent。

```sh
zuno agent list
```

在显式路径上创建一个 subagent 定义，带说明并固定模型。

```sh
zuno agent create --path .zuno/agent/reviewer.md --mode subagent --description "Review diffs for regressions" --model openai/gpt-5
```

创建之后检视某个 Agent 完全解析后的定义。

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
