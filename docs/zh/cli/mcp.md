# zuno mcp

Model Context Protocol 服务器用 Zuno 自身不附带的工具与资源来扩展它。`zuno mcp` 负责注册
这些服务器、处理其中需要认证的那些，并探测行为异常的服务器，好让你看出故障是在服务器里
还是在注册配置里。

`add` 既接受一个本地命令（写在 `--` 之后），也接受一个远程 URL，并且能把环境变量和
HTTP 头透传给它。

## 用法

```sh
zuno mcp [OPTIONS] <COMMAND>
```

## 选项

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

## 子命令

| 子命令 | 说明 |
| --- | --- |
| [`add`](#zuno-mcp-add) | |
| [`list`](#zuno-mcp-list) | |
| [`auth`](#zuno-mcp-auth) | |
| [`logout`](#zuno-mcp-logout) | |
| [`debug`](#zuno-mcp-debug) | |
| `help` | 打印本消息或给定子命令的帮助 |

### zuno mcp add

```sh
zuno mcp add [OPTIONS] [NAME] [-- <SERVER_COMMAND>...]
```

| 参数 | 说明 |
| --- | --- |
| `[NAME]` | |
| `[SERVER_COMMAND]...` | |

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `--url <URL>` | | |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--env <ENV>` | | |
| `--header <HEADER>` | | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

### zuno mcp list

```sh
zuno mcp list [OPTIONS]
```

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

### zuno mcp auth

```sh
zuno mcp auth [OPTIONS] [NAME] [COMMAND]
```

| 参数 | 说明 |
| --- | --- |
| `[NAME]` | |

| 嵌套命令 | 说明 |
| --- | --- |
| `list` | |
| `help` | 打印本消息或给定子命令的帮助 |

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

`zuno mcp auth list` 只接受每个命令共有的那些选项。

```sh
zuno mcp auth list [OPTIONS]
```

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

### zuno mcp logout

```sh
zuno mcp logout [OPTIONS] [NAME]
```

| 参数 | 说明 |
| --- | --- |
| `[NAME]` | |

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

### zuno mcp debug

```sh
zuno mcp debug [OPTIONS] <NAME>
```

| 参数 | 说明 |
| --- | --- |
| `<NAME>` | |

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

## 示例

列出本安装知道的 MCP 服务器。

```sh
zuno mcp list
```

注册一个作为子进程启动的本地服务器，把它的命令写在 `--` 之后。

```sh
zuno mcp add my-server -- npx -y @modelcontextprotocol/server-filesystem /srv/data
```

注册一个远程服务器，并在每个请求中发送 bearer token。

```sh
zuno mcp add remote-server --url https://mcp.example.com --header "Authorization: Bearer $MCP_TOKEN"
```

某个已注册服务器的工具没有出现时，探测它。

```sh
zuno mcp debug my-server
```

## 参见

- [全局选项](/zh/cli/global-options)
- [zuno plugin](/zh/cli/plugin)
- [zuno providers](/zh/cli/providers)
- [配置参考](/zh/config/reference)
- [插件](/zh/guide/plugins)
