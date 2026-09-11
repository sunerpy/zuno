# zuno tui

`zuno tui` 启动交互式终端应用，你可以在 session 运行过程中引导它、回答权限提示，并随着
持久 transcript 被写入而实时阅读。不给 `zuno` 任何子命令时它做的也是这件事，因此两种形式
可以互换。

用它的选项来预先选定模型或 Agent、恢复此前的工作，或者提交一条开场提示，让 session 在
启动后立即开始工作。被恢复的 session（`--continue`、`--session`）会以它上次使用的 Agent、模型与推理强度打开；`--model` 与 `--agent` 在本进程内优先于保存的值。

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
| `--sandbox-on-unavailable <ACTION>` | 选择受限 Shell 无法部署时的处理方式。可选值：`deny`、`run-unconfined` | `deny` |
| `--sandbox-backend <BACKEND>` | 为本次调用选择 Shell 执行后端；`native` 不是沙箱隔离。可选值：`auto`、`native` | 随平台解析 |
| `--auto` | 不询问就准入每一项未被显式拒绝的权限。上游自己的描述以 "(dangerous!)" 结尾，而且名副其实：这等于把权限提示处的人替换掉，于是默认规则集本来会停下来征询的工具调用会无人看管地继续执行 | |
| `--background` | 在持久的本地 supervisor 中启动 TUI，并立即连接 | |
| `--attach <PTY_ID>` | 连接一个已经保留的后台 TUI | |
| `--background-list` | 列出后台 TUI；supervisor 不存在时不会自动启动 | |
| `--background-stop <PTY_ID>` | 停止并移除一个后台 TUI | |
| `--background-shutdown` | 停止本地后台 TUI supervisor 及其拥有的全部 PTY | |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

## 示例

在当前目录启动终端应用。

```sh
zuno tui
```

恢复本 checkout 中最近的 session，而不是打开一个新的，并沿用它上次使用的 Agent 与模型。

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

让 TUI 在 SSH 断开后继续运行。`Ctrl+]` 只断开本地终端，不停止后台 TUI：

```sh
zuno tui --background --session ses_1a2b3c
zuno tui --background-list
zuno tui --attach pty_01abc...
zuno tui --background-stop pty_01abc...
zuno tui --background-shutdown
```

supervisor 只绑定 loopback，生成随机 Basic-auth 密码，并把状态保存在权限受限的 Zuno
数据目录。`--background-list` 不会为了列空列表而启动 supervisor。正常退出 TUI 仍要求在
1.5 秒内连续按两次相同的 `Ctrl+C` 或 `Ctrl+D`；这会退出被保留的子进程，而 `Ctrl+]`
只负责 detach。

## 提问与恢复命令

在 TUI 中输入以下命令：

| 命令 | 行为 |
| --- | --- |
| `/questions` 或 `/questions list` | 查看待答请求、已保存草稿和已确认答案数 |
| `/questions open <request-id>` 或 `/questions <request-id>` | 带着持久化答案和草稿值重新打开指定请求 |
| `/resume` | 显式恢复当前会话中已暂停或已结束的 Work |

在问题表单中按 `Ctrl+S` 会保存持久草稿。即使全部填满，草稿仍然待答，草稿文本不会发给模型。
进程重启后打开同一会话，可恢复已经保存的草稿。只有明确提交才确认答案，并且只进入一次持久
FIFO；延期和空答复都不会产生模型输入。

Plan 批准必须明确选择 `approve`；Draft review 还需要非空的风险接受理由。高亮选项或保存
批准草稿都不会开始 Work。`/resume` 不会绕过 Plan 批准或指定的人类／外部事件等待；
Goal 本身不活跃时，先使用 `/goal resume`。

选择历史会话请用 `/session`、`/sessions` 或 `/continue`。完整的问题交互说明见
[终端应用](/zh/guide/tui)。

## 参见

- [全局选项](/zh/cli/global-options)
- [zuno run](/zh/cli/run)
- [zuno session](/zh/cli/session)
- [zuno agent](/zh/cli/agent)
- [配置参考](/zh/config/reference)
- [Harness 运行时](/zh/operate/harness-runtime)
