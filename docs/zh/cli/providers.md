# zuno providers

`zuno providers` 管理本安装能触达哪些模型 Provider，以及它为这些 Provider 持有的凭据。
登录方式因 Provider 而异，所以流程是：先问某个 Provider 实现了哪些方式，用其中一种完成
认证，不再需要时把已存储的凭据移除。

凭据存储在本地。`logout` 会移除它们，交接机器或轮换账号之前该运行的就是这个。

## 用法

```sh
zuno providers [OPTIONS] <COMMAND>
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
| [`list`](#zuno-providers-list) | |
| [`methods`](#zuno-providers-methods) | 列出某个 Provider 已实现的登录方式 |
| [`login`](#zuno-providers-login) | 用某个 Provider 已实现的登录方式之一完成认证 |
| [`logout`](#zuno-providers-logout) | |
| `help` | 打印本消息或给定子命令的帮助 |

### zuno providers list

```sh
zuno providers list [OPTIONS]
```

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | 选择受限 Shell 无法部署时的处理方式。可选值：`deny`、`run-unconfined` | `deny` |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

### zuno providers methods

列出某个 Provider 已实现的登录方式。

```sh
zuno providers methods [OPTIONS] <PROVIDER>
```

| 参数 | 说明 |
| --- | --- |
| `<PROVIDER>` | Provider id 或显示名称 |

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | 选择受限 Shell 无法部署时的处理方式。可选值：`deny`、`run-unconfined` | `deny` |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

### zuno providers login

用某个 Provider 已实现的登录方式之一完成认证。

```sh
zuno providers login [OPTIONS] [TARGET]
```

| 参数 | 说明 |
| --- | --- |
| `[TARGET]` | Provider id/名称，或一个实现了 `/.well-known/zuno` 的 HTTPS URL。在终端中省略它可交互选择。URL 登录会运行由远端主机指定的程序，运行前会先显示并请求确认 |

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `-p`, `--provider <PROVIDER>` | Provider id 或显示名称，作为位置参数目标的替代 | |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `-m`, `--method <METHOD>` | 由 `zuno auth methods <provider>` 显示的方式 id。在终端中省略它可在有多种方式时进行选择 | |
| `--trust-remote-command` | 不经交互确认，直接运行 URL 的 `/.well-known/zuno` 文档所指定的程序。该程序及其参数由远端主机选择，并以你的权限运行：只对你已经信任的主机传入此选项。不传时，stdin 或 stderr 不是终端的 URL 登录会直接拒绝 | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | 选择受限 Shell 无法部署时的处理方式。可选值：`deny`、`run-unconfined` | `deny` |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

URL 目标会执行由远端主机选择的命令。`zuno auth login <url>` 接受指向任意主机的 `https://`，
或指向回环 IP 地址（如 `http://127.0.0.1:8080`、`http://[::1]:3000`）的纯 `http://`；
URL 不得携带 userinfo。仅以 `127.0.0.1` 开头的主机名（例如 `http://127.0.0.1.attacker.example`）、
`user@host` 形式以及 `http://localhost` 会在发起任何请求之前被拒绝。
Zuno 下载 `<url>/.well-known/zuno`，逐行、带引号地打印该文档指定的程序和每一个参数，
然后以 `Run this command` 询问，默认选中 `No`；按 Enter、Esc 或 Ctrl-C 即拒绝，
不运行、不存储任何东西。只有在确认过的命令成功退出后才会存储凭据。stdin 或 stderr 不是终端时，
登录会拒绝而不是提示；对已经信任的主机可传入 `--trust-remote-command`，
不经确认直接运行远端指定的命令。该选项对 provider 登录会被拒绝。

### zuno providers logout

```sh
zuno providers logout [OPTIONS] [PROVIDER]
```

| 参数 | 说明 |
| --- | --- |
| `[PROVIDER]` | |

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | 选择受限 Shell 无法部署时的处理方式。可选值：`deny`、`run-unconfined` | `deny` |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

## 示例

看看本安装知道哪些 Provider。

```sh
zuno providers list
```

在选定之前，先问某个 Provider 实现了哪些登录方式。

```sh
zuno providers methods openai
```

用显式指定的方式完成认证，而不走交互式选择器。

```sh
zuno providers login openai --method api-key
```

移除某一个 Provider 已存储的凭据。

```sh
zuno providers logout openai
```

## 参见

- [全局选项](/zh/cli/global-options)
- [zuno models](/zh/cli/models)
- [zuno mcp](/zh/cli/mcp)
- [已排除的命令](/zh/cli/excluded)
- [Provider 参考](/zh/config/providers)
- [配置参考](/zh/config/reference)
