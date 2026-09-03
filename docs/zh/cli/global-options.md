# 全局选项

有几个选项接在命令树的每一个命令上，而不是接在单独的处理器上。它们控制版本报告、日志
目标与详细程度、Shell 约束以及帮助输出，无论传给 `zuno` 本身还是传给任何子命令，行为都
完全一致。

因为它们处处都被接受，所以在这里集中记录一次，而不是在每个命令页面上重复。

它们都只作用于被传入的那一次调用。Zuno 读的是启动时解析出来的值，而不是运行中进程的环境，
所以它们不依赖被写回环境；在 Windows 上这也意味着 Zuno 启动的程序不会继承它们。
参见[一次调用就是一个进程](/zh/cli/#一次调用就是一个进程)。

## 用法

```sh
zuno [OPTIONS] [COMMAND]
```

## 选项

这些选项会出现在 `zuno` 以及每个子命令的 `--help` 输出中。

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`（最详尽的追踪细节）、`DEBUG`（详细诊断事件）、`INFO`（常规运行事件）、`WARN`（警告与错误）、`ERROR`（仅错误） | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | 选择受限 Shell 无法部署时的处理方式。可选值：`deny`、`run-unconfined` | `deny` |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

## 根调用选项

不带子命令运行 `zuno` 会启动交互式终端应用，因此根调用还额外接受
[`zuno tui`](/zh/cli/tui) 所接受的 session 与模型选择选项。

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `--prompt <PROMPT>` | 启动时提交这条提示，效果等同于手动输入并发送 | |
| `-m`, `--model <MODEL>` | 要使用的模型，形式为 `provider/model` | |
| `--agent <AGENT>` | 要使用的 Agent | |
| `-c`, `--continue` | 继续本目录中最近的一个 session | |
| `-s`, `--session <SESSION>` | 在这个确切的 session 中对话 | |
| `--auto` | 不询问就准入每一项未被显式拒绝的权限。上游自己的描述以 "(dangerous!)" 结尾，而且名副其实：这等于把权限提示处的人替换掉，于是默认规则集本来会停下来征询的工具调用会无人看管地继续执行 | |

## 示例

报告 `PATH` 上那个可执行文件的包版本。

```sh
zuno --version
```

在运行一次性提示时把日志同时输出到 stderr，这样无需打开本地日志存储也能看到失败信息。

```sh
zuno run --print-logs --log-level DEBUG "summarize the build failure"
```

把 Shell 约束为只读，仅作用于单次交互式 session，不改动配置。

```sh
zuno tui --sandbox read-only
```

当发生符合条件的约束后端不可用错误时，允许具备写能力的 Agent 使用宿主权限继续。
只读 Agent 仍会拒绝，受管策略也可以把这个选择改回 `deny`。

```sh
zuno run --sandbox-on-unavailable run-unconfined "run the local build"
```

在依赖某个约束模式之前，先验证它在本宿主上是否真的可部署。

```sh
zuno debug sandbox --mode read-only --check
```

## 参见

- [CLI 参考](/zh/cli/)
- [zuno tui](/zh/cli/tui)
- [zuno debug](/zh/cli/debug)
- [配置参考](/zh/config/reference)
- [日志](/zh/operate/logging)
