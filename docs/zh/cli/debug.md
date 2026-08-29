# zuno debug

`zuno debug` 回答的是"这套安装实际认为什么是真的？"它报告解析后的路径与配置、为某次
Provider 请求发送的确切提示、生效的权限规则集、某个沙箱模式是否可部署、文件搜索后端看到
什么、语言服务器报告什么，以及快照存储中持有什么。

这些都是用于诊断的读取并报告的面。当行为与配置不一致、当权限提示出现在你没预料到的地方，
或者当某个约束模式在运行时失败时，就该找它们。

## 用法

```sh
zuno debug [OPTIONS] <COMMAND>
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
| [`paths`](#zuno-debug-paths) | |
| [`config`](#zuno-debug-config) | |
| [`agent`](#zuno-debug-agent) | |
| [`prompt`](#zuno-debug-prompt) | |
| [`permissions`](#zuno-debug-permissions) | |
| [`skill`](#zuno-debug-skill) | |
| [`sandbox`](#zuno-debug-sandbox) | |
| [`rg`](#zuno-debug-rg) | |
| [`lsp`](#zuno-debug-lsp) | |
| [`snapshot`](#zuno-debug-snapshot) | |
| `help` | 打印本消息或给定子命令的帮助 |

### zuno debug paths

```sh
zuno debug paths [OPTIONS]
```

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | 选择受限 Shell 无法部署时的处理方式。可选值：`deny`、`run-unconfined` | `deny` |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

### zuno debug config

```sh
zuno debug config [OPTIONS]
```

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | 选择受限 Shell 无法部署时的处理方式。可选值：`deny`、`run-unconfined` | `deny` |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

### zuno debug agent

```sh
zuno debug agent [OPTIONS] <NAME>
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
| `--sandbox-on-unavailable <ACTION>` | 选择受限 Shell 无法部署时的处理方式。可选值：`deny`、`run-unconfined` | `deny` |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

### zuno debug prompt

```sh
zuno debug prompt [OPTIONS]
```

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `--session <ID>` | 应显示其提示回执的 session；默认为最新的回执 | |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--step <N>` | 所选 session 内从 1 开始计数的 Provider 请求步骤 | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--show-sensitive` | 包含模型可见的指令、AGENTS、Skill 与记忆内容 | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | 选择受限 Shell 无法部署时的处理方式。可选值：`deny`、`run-unconfined` | `deny` |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

`--show-sensitive` 会原样打印指令、AGENTS、Skill 与记忆内容。把那份输出粘贴进工单之前，
按敏感信息对待。

### zuno debug permissions

```sh
zuno debug permissions [OPTIONS]
```

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | 选择受限 Shell 无法部署时的处理方式。可选值：`deny`、`run-unconfined` | `deny` |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

### zuno debug skill

```sh
zuno debug skill [OPTIONS]
```

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | 选择受限 Shell 无法部署时的处理方式。可选值：`deny`、`run-unconfined` | `deny` |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

### zuno debug sandbox

```sh
zuno debug sandbox [OPTIONS]
```

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `--mode <MODE>` | 要探测的沙箱策略；受限模式会验证 bubblewrap 的部署情况。可选值：`read-only`、`workspace-write`、`danger-full-access` | `workspace-write` |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--network <NETWORK>` | 要验证的网络权限。受约束模式默认为 deny，danger-full-access 默认为 allow。可选值：`deny`、`allow` | |
| `--check` | 当所请求的策略不可部署时以失败状态退出 | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | 在这次受信调用策略下报告降级解析。可选值：`deny`、`run-unconfined` | `deny` |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

报告会区分请求与实际的模式/网络权限，给出解析类型、降级资格和类型化原因。`ready` 字段与
`--check` 始终保持严格：具备降级资格不会让请求的约束部署通过。

### zuno debug rg

```sh
zuno debug rg [OPTIONS] <COMMAND>
```

| 嵌套命令 | 说明 |
| --- | --- |
| [`files`](#zuno-debug-rg-files) | |
| [`search`](#zuno-debug-rg-search) | |
| `help` | 打印本消息或给定子命令的帮助 |

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | 选择受限 Shell 无法部署时的处理方式。可选值：`deny`、`run-unconfined` | `deny` |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

#### zuno debug rg files

```sh
zuno debug rg files [OPTIONS]
```

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `--query <QUERY>` | | |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--glob <GLOB>` | | |
| `--limit <LIMIT>` | | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | 选择受限 Shell 无法部署时的处理方式。可选值：`deny`、`run-unconfined` | `deny` |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

#### zuno debug rg search

```sh
zuno debug rg search [OPTIONS] <PATTERN>
```

| 参数 | 说明 |
| --- | --- |
| `<PATTERN>` | |

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `--glob <GLOB>` | | |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--limit <LIMIT>` | | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | 选择受限 Shell 无法部署时的处理方式。可选值：`deny`、`run-unconfined` | `deny` |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

### zuno debug lsp

```sh
zuno debug lsp [OPTIONS] <COMMAND>
```

| 嵌套命令 | 说明 |
| --- | --- |
| [`diagnostics`](#zuno-debug-lsp-diagnostics) | |
| [`symbols`](#zuno-debug-lsp-symbols) | |
| [`document-symbols`](#zuno-debug-lsp-document-symbols) | |
| `help` | 打印本消息或给定子命令的帮助 |

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | 选择受限 Shell 无法部署时的处理方式。可选值：`deny`、`run-unconfined` | `deny` |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

#### zuno debug lsp diagnostics

```sh
zuno debug lsp diagnostics [OPTIONS] <FILE>
```

| 参数 | 说明 |
| --- | --- |
| `<FILE>` | |

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | 选择受限 Shell 无法部署时的处理方式。可选值：`deny`、`run-unconfined` | `deny` |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

#### zuno debug lsp symbols

```sh
zuno debug lsp symbols [OPTIONS] <QUERY>
```

| 参数 | 说明 |
| --- | --- |
| `<QUERY>` | |

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | 选择受限 Shell 无法部署时的处理方式。可选值：`deny`、`run-unconfined` | `deny` |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

#### zuno debug lsp document-symbols

```sh
zuno debug lsp document-symbols [OPTIONS] <URI>
```

| 参数 | 说明 |
| --- | --- |
| `<URI>` | |

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | 选择受限 Shell 无法部署时的处理方式。可选值：`deny`、`run-unconfined` | `deny` |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

### zuno debug snapshot

```sh
zuno debug snapshot [OPTIONS] <COMMAND>
```

| 嵌套命令 | 说明 |
| --- | --- |
| [`track`](#zuno-debug-snapshot-track) | |
| [`patch`](#zuno-debug-snapshot-patch) | |
| [`diff`](#zuno-debug-snapshot-diff) | |
| `help` | 打印本消息或给定子命令的帮助 |

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | 选择受限 Shell 无法部署时的处理方式。可选值：`deny`、`run-unconfined` | `deny` |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

#### zuno debug snapshot track

```sh
zuno debug snapshot track [OPTIONS]
```

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | 选择受限 Shell 无法部署时的处理方式。可选值：`deny`、`run-unconfined` | `deny` |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

#### zuno debug snapshot patch

```sh
zuno debug snapshot patch [OPTIONS] <HASH>
```

| 参数 | 说明 |
| --- | --- |
| `<HASH>` | |

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | 选择受限 Shell 无法部署时的处理方式。可选值：`deny`、`run-unconfined` | `deny` |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

#### zuno debug snapshot diff

```sh
zuno debug snapshot diff [OPTIONS] <HASH>
```

| 参数 | 说明 |
| --- | --- |
| `<HASH>` | |

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | 选择受限 Shell 无法部署时的处理方式。可选值：`deny`、`run-unconfined` | `deny` |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

## 示例

报告本安装解析出的数据、配置与日志路径。

```sh
zuno debug paths
```

当某个工具调用被意外拦下时，打印生效的权限规则集。

```sh
zuno debug permissions
```

验证某个约束模式是否可部署，并在不可部署时让命令失败。

```sh
zuno debug sandbox --mode read-only --check
```

检视为某个具体 Provider 请求步骤实际发送的提示。

```sh
zuno debug prompt --session ses_1a2b3c --step 2
```

## 参见

- [全局选项](/zh/cli/global-options)
- [zuno agent](/zh/cli/agent)
- [zuno session](/zh/cli/session)
- [zuno plugin](/zh/cli/plugin)
- [配置参考](/zh/config/reference)
- [Harness 运行时](/zh/operate/harness-runtime)
- [日志](/zh/operate/logging)
