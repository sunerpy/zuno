# zuno session

Session 是持久的。每一条提示、工具结果和可能改变模型请求的报告都会写入本地存储，这意味着
存储会不断增长，最终需要检视与清理。`zuno session` 就是这个面：它列出已有内容、按年龄清理，
删除某个确切的 session，并检查有明确证据的历史执行门禁缺陷。

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
| `--sandbox-backend <BACKEND>` | 为本次调用选择 Shell 执行后端；`native` 不是沙箱隔离。可选值：`auto`、`native` | 随平台解析 |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

## 子命令

| 子命令 | 说明 |
| --- | --- |
| [`list`](#zuno-session-list) | |
| [`prune`](#zuno-session-prune) | |
| [`delete`](#zuno-session-delete) | |
| [`repair`](#zuno-session-repair) | 检查或修复一条有明确证据的历史误阻塞 |
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
| `--sandbox-backend <BACKEND>` | 为本次调用选择 Shell 执行后端；`native` 不是沙箱隔离。可选值：`auto`、`native` | 随平台解析 |
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
| `--sandbox-backend <BACKEND>` | 为本次调用选择 Shell 执行后端；`native` 不是沙箱隔离。可选值：`auto`、`native` | 随平台解析 |
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

下面两个选项必须且只能选一个。删除 session 会决定由它派生的 Experience 记录的归属，
因此本命令不做猜测。

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `--keep-derived-experiences` | 保留 Experience 记录，并把它们与被删除的 session 解除关联 | |
| `--cleanup-derived-experiences` | 准备经过复核的 Memory/Skill 撤回，并遗忘派生的 Experience 记录 | |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | 选择受限 Shell 无法部署时的处理方式。可选值：`deny`、`run-unconfined` | `deny` |
| `--sandbox-backend <BACKEND>` | 为本次调用选择 Shell 执行后端；`native` 不是沙箱隔离。可选值：`auto`、`native` | 随平台解析 |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

### zuno session repair

```sh
zuno session repair <SESSION_ID> --input <INPUT_ID> --dry-run
zuno session repair <SESSION_ID> --input <INPUT_ID> --apply --expected-revision <N>
```

默认只检查：只读打开已有的 format-15 数据库，不迁移、不调用模型。`--apply` 必须提供
刚检查的**执行状态 revision**，不能与 `--dry-run` 同用，并且需要独占、离线访问数据库。

| 选项 | 含义 |
| --- | --- |
| `--input <INPUT_ID>` | 要检查的确切已保存输入 |
| `--dry-run` | 只检查，也是默认行为 |
| `--apply` | 重新核验后执行有界原生修复 |
| `--expected-revision <N>` | 与 `--apply` 一起使用的正数执行状态 revision |

仅当普通用户输入已经 consumed、回执仍为 recorded 且从未 applied，并且结构化事件链
证明它继承了旧版「可重试错误被误写为 blocked」缺陷时，才允许修复。活跃执行、未知结果、
真实审批／Plan／认证／预算／Goal 门禁、变化的证据或无法证明的历史均会被拒绝。
应用前应关闭占用该数据库的全部 Zuno 宿主并备份数据库，不只是目标会话；
此命令不能绕过这些检查。

成功应用仅排入一个携带原输入 ID 的审计恢复控制，不把 consumed 用户行重新入队，
不重放失败工具，不恢复旧 Goal，也不声称模型已经处理。通过正常原生客户端重新打开会话
后处理该控制；查看原输入的回执，不要重复发送文本。
所有权与恢复约定见[持久状态](/zh/guide/durable-state)。

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

按 id 删除一个 session，并保留它产生的 Experience 记录。

```sh
zuno session delete ses_1a2b3c --keep-derived-experiences
```

改为遗忘派生的 Experience 记录需要一个在线的 learning profile，因此请在 TUI 中执行并选择
`clean learning`，或通过 ACP 传入 `cleanupDerivedExperiences=true`。在命令行上，只有当该
session 子树没有产生任何 Experience 记录时它才会成功。

```sh
zuno session delete ses_1a2b3c --cleanup-derived-experiences
```

## 参见

- [全局选项](/zh/cli/global-options)
- [zuno run](/zh/cli/run)
- [zuno db](/zh/cli/db)
- [Session 保留](/zh/operate/session-retention)
- [Harness 运行时](/zh/operate/harness-runtime)
