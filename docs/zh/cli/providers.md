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
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

### zuno providers login

用某个 Provider 已实现的登录方式之一完成认证。

```sh
zuno providers login [OPTIONS] [TARGET]
```

| 参数 | 说明 |
| --- | --- |
| `[TARGET]` | Provider id/名称，或一个实现了 `/.well-known/zuno` 的 HTTPS URL。在终端中省略它可交互选择 |

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `-p`, `--provider <PROVIDER>` | Provider id 或显示名称，作为位置参数目标的替代 | |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `-m`, `--method <METHOD>` | 由 `zuno auth methods <provider>` 显示的方式 id。在终端中省略它可在有多种方式时进行选择 | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

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
