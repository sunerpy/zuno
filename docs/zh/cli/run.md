# zuno run

`zuno run` 在没有终端 UI 的情况下驱动 harness。它从命令行或文件读取一条消息，运行到完成，
再把结果写到 stdout。这是在脚本、CI 任务和 git hook 中该用的形式 —— 那些场景里没有可
附着的交互式 session，而输出需要可被机器解析。

它可以开启一个全新 session、继续当前目录中最近的那个、指向一个确切的 session id，或者
fork 一个已有 session，让原始 transcript 保持不被触碰。

## 用法

```sh
zuno run [OPTIONS] [message]...
```

## 参数

| 参数 | 说明 |
| --- | --- |
| `[message]...` | |

## 选项

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `--command <COMMAND>` | | |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `-c`, `--continue` | | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `-s`, `--session <SESSION>` | | |
| `--fork` | | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--share` | | |
| `-m`, `--model <MODEL>` | | |
| `--agent <AGENT>` | | |
| `--format <FORMAT>` | 可选值：`default`、`json` | `default` |
| `-f`, `--file <FILE>` | | |
| `--title <TITLE>` | | |
| `--attach <ATTACH>` | | |
| `-p`, `--password <PASSWORD>` | | |
| `-u`, `--username <USERNAME>` | | |
| `--dir <DIR>` | | |
| `--port <PORT>` | | |
| `--variant <VARIANT>` | | |
| `--thinking` | | |
| `-i`, `--interactive` | | |
| `--auto` | | |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

## 示例

运行单条消息并打印默认的人类可读结果。

```sh
zuno run "explain what changed in the last commit"
```

继续本目录中最近的 session，而不是新开一个。

```sh
zuno run --continue "now add tests for the new branch"
```

输出 JSON，让脚本解析结果，而不是去抓取格式化文本。

```sh
zuno run --format json "list the failing tests" > result.json
```

用指定的 Agent fork 一个已有 session，同时保持原始 transcript 完好。

```sh
zuno run --session ses_1a2b3c --fork --agent plan "what would a safe migration look like?"
```

## 参见

- [全局选项](/zh/cli/global-options)
- [zuno tui](/zh/cli/tui)
- [zuno session](/zh/cli/session)
- [zuno agent](/zh/cli/agent)
- [编排](/zh/guide/orchestration)
- [Harness 运行时](/zh/operate/harness-runtime)
