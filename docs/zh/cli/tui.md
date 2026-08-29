# zuno tui

`zuno tui` 启动交互式终端应用，你可以在 session 运行过程中引导它、回答权限提示，并随着
持久 transcript 被写入而实时阅读。不给 `zuno` 任何子命令时它做的也是这件事，因此两种形式
可以互换。

用它的选项来预先选定模型或 Agent、恢复此前的工作，或者提交一条开场提示，让 session 在
启动后立即开始工作。

## 用法

```sh
zuno tui [OPTIONS]
```

## 选项

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `--prompt <PROMPT>` | 启动时提交这条提示，效果等同于手动输入并发送 | |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `-m`, `--model <MODEL>` | 要使用的模型，形式为 `provider/model` | |
| `--agent <AGENT>` | 要使用的 Agent | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `-c`, `--continue` | 继续本目录中最近的一个 session | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `-s`, `--session <SESSION>` | 在这个确切的 session 中对话 | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--auto` | 不询问就准入每一项未被显式拒绝的权限。上游自己的描述以 "(dangerous!)" 结尾，而且名副其实：这等于把权限提示处的人替换掉，于是默认规则集本来会停下来征询的工具调用会无人看管地继续执行 | |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

## 示例

在当前目录启动终端应用。

```sh
zuno tui
```

恢复本 checkout 中最近的 session，而不是打开一个新的。

```sh
zuno tui --continue
```

以指定模型启动，并且已提交一条开场提示。

```sh
zuno tui --model openai/gpt-5 --prompt "review the diff on this branch"
```

按 id 重新打开一个确切的 session，并把本次调用的 Shell 约束为只读。

```sh
zuno tui --session ses_1a2b3c --sandbox read-only
```

## 参见

- [全局选项](/zh/cli/global-options)
- [zuno run](/zh/cli/run)
- [zuno session](/zh/cli/session)
- [zuno agent](/zh/cli/agent)
- [配置参考](/zh/config/reference)
- [Harness 运行时](/zh/operate/harness-runtime)
