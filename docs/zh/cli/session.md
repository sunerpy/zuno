# zuno session

Session 是持久的。每一条提示、工具结果和可能改变模型请求的报告都会写入本地存储，这意味着
存储会不断增长，最终需要检视与清理。`zuno session` 就是这个面：它列出已有内容、按年龄清理，
并删除某个确切的 session。

默认情况下列表的范围限定在当前 checkout，并且只显示根 session。由委派产生的子 session
在你主动要求之前是隐藏的。

## 用法

```sh
zuno session [OPTIONS] <COMMAND>
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
| [`list`](#zuno-session-list) | |
| [`prune`](#zuno-session-prune) | |
| [`delete`](#zuno-session-delete) | |
| `help` | 打印本消息或给定子命令的帮助 |

### zuno session list

```sh
zuno session list [OPTIONS]
```

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `--all-projects` | 列出所有项目的 session，而不只是本 checkout 的 | |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--project <PATH\|ID>` | 只列出一个项目，通过其 id 或其 worktree 路径指定 | |
| `--archived` | 在活跃 session 之外一并包含已归档的 session | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--roots` | 只看根 session。这是默认行为；传 `--no-roots` 可看子 session | |
| `--no-roots` | 包含默认被隐藏的子 session | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | 选择受限 Shell 无法部署时的处理方式。可选值：`deny`、`run-unconfined` | `deny` |
| `--sort <SORT>` | 按最后活动时间或创建时间排序。可选值：`updated`（`time_updated`，最后活动；上游 `listGlobal` 的顺序）、`created`（`time_created`） | `updated` |
| `-n`, `--limit <LIMIT>` | 限制为 N 个 session，最近的在前。默认为 100。别名：`--max-count` | |
| `--format <FORMAT>` | 输出格式。可选值：`table`、`json` | `table` |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

### zuno session prune

`--older-than <DAYS>` 是必需的。

```sh
zuno session prune [OPTIONS] --older-than <DAYS>
```

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `--older-than <DAYS>` | | |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--all-projects` | | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--project <PATH\|ID>` | | |
| `--by <BY>` | 可选值：`updated`、`created` | `updated` |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--archive` | | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | 选择受限 Shell 无法部署时的处理方式。可选值：`deny`、`run-unconfined` | `deny` |
| `--delete` | | |
| `--include-shared` | | |
| `--include-recent` | | |
| `--force` | | |
| `--yes` | | |
| `--format <FORMAT>` | 可选值：`table`、`json` | `table` |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

### zuno session delete

```sh
zuno session delete [OPTIONS] <SESSION_ID>
```

| 参数 | 说明 |
| --- | --- |
| `<SESSION_ID>` | |

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | 选择受限 Shell 无法部署时的处理方式。可选值：`deny`、`run-unconfined` | `deny` |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

## 示例

列出当前 checkout 最近的根 session。

```sh
zuno session list
```

以 JSON 形式包含子 session 与已归档 session，便于脚本过滤。

```sh
zuno session list --no-roots --archived --format json
```

查看一次 30 天的清理会涉及所有项目中的哪些内容，按创建时间排序，且不删除任何东西。

```sh
zuno session prune --older-than 30 --by created --all-projects
```

按 id 删除一个 session。

```sh
zuno session delete ses_1a2b3c
```

## 参见

- [全局选项](/zh/cli/global-options)
- [zuno run](/zh/cli/run)
- [zuno db](/zh/cli/db)
- [Session 保留](/zh/operate/session-retention)
- [Harness 运行时](/zh/operate/harness-runtime)
