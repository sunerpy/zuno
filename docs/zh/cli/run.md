# zuno run

`zuno run` 在没有终端 UI 的情况下驱动 harness。它从命令行或文件读取一条消息，运行到完成，
再把结果写到 stdout。这是在脚本、CI 任务和 git hook 中该用的形式 —— 那些场景里没有可
附着的交互式 session，而输出需要可被机器解析。

它可以开启一个全新 session、继续当前目录中最近的那个，或者指向一个确切的 session id。

## 用法

```sh
zuno run [OPTIONS] [message]...
```

## 参数

| 参数 | 说明 |
| --- | --- |
| `[message]...` | 要运行的消息。省略它则从 stdin 读取消息 |

## 选项

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `--command <COMMAND>` | 运行这个已配置的 command，并把消息作为它的参数 | |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `-c`, `--continue` | 继续本目录中最近的那个 session | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `-s`, `--session <SESSION>` | 在这个确切的 session 中运行 | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | 选择受限 Shell 无法部署时的处理方式。可选值：`deny`、`run-unconfined` | `deny` |
| `-m`, `--model <MODEL>` | 要使用的模型，写成 `provider/model` | |
| `--agent <AGENT>` | 要使用的 Agent | |
| `--format <FORMAT>` | 输出格式。可选值：`default`、`json` | `default` |
| `--show-reasoning` | 在稳定标记之间把 provider 提供的 reasoning delta 写到 stderr | |
| `-f`, `--file <FILE>` | 给消息附加一个文本或图片文件，可重复 | |
| `--title <TITLE>` | 新建 session 时使用的标题 | |
| `--dir <DIR>` | 运行所在的目录，默认为当前工作目录 | |
| `--variant <VARIANT>` | 请求的 reasoning 变体，用于发布了具名变体的模型。不能与 `--thinking` 组合 | |
| `--thinking` | 请求模型自身的默认 thinking 预算。不能与 `--variant` 组合 | |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

`--fork`、`--share`、`--attach`、`--port`、`--username`、`--password`、`--interactive`
与 `--auto` 在早先的版本里可以被接受，但每一次给出它们的调用都会被拒绝 —— 因为它们背后
没有任何实现。现在它们完全不再被接受：脚本传入其中之一会在解析阶段就失败，而不是在进程
启动之后。`--auto` 在交互面上有真正的归宿：`zuno tui --auto --prompt <message>`。

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

显式显示 provider 可见的推理进度，同时不污染最终答案流。

```sh
zuno run --show-reasoning "inspect the failure" > answer.txt 2> progress.txt
```

只显示 provider 明确给出的 reasoning delta；signed thinking 与 encrypted reasoning 永不渲染。每个区块使用 `<<<zuno:reasoning>>>` 与 `<<<zuno:end-reasoning>>>` 标记，即使流以错误结束也会闭合。`--show-reasoning` 不能与 `--format json` 组合；JSON 模式继续使用现有结构化事件流。

指向一个确切的 session，并为本轮换用另一个 Agent。

```sh
zuno run --session ses_1a2b3c --agent plan "what would a safe migration look like?"
```

## 参见

- [全局选项](/zh/cli/global-options)
- [zuno tui](/zh/cli/tui)
- [zuno session](/zh/cli/session)
- [zuno agent](/zh/cli/agent)
- [编排](/zh/guide/orchestration)
- [Harness 运行时](/zh/operate/harness-runtime)
