# zuno plugin

扩展包贡献 Agent、workflow、Skill 与可执行工具。`zuno plugin` 从本地目录安装它们、以事务
方式替换已安装的包、移除它，并报告对某个给定目录而言哪些包实际处于活跃状态。

作用域在这里很关键。不带 `--project` 时包会全局安装；带上它时，包会安装到所选项目的
`.zuno` 目录之下，并且只在那里生效。`list` 会解析整条项目配置链，因此它是检查某个具体
checkout 真正加载了什么的方式。

## 用法

```sh
zuno plugin [OPTIONS] <COMMAND>
```

## 选项

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | 选择受限 Shell 无法部署时的处理方式。可选值：`deny`、`run-unconfined` | `deny` |
| `--sandbox-backend <BACKEND>` | 为本次调用选择 Shell 执行后端；`native` 不是沙箱隔离。可选值：`auto`、`native` | `auto` |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

## 子命令

| 子命令 | 说明 |
| --- | --- |
| [`list`](#zuno-plugin-list) | 列出对某一个目录活跃的包 |
| [`add`](#zuno-plugin-add) | 安装一个新的本地包 |
| [`update`](#zuno-plugin-update) | 以事务方式替换一个已安装的本地包 |
| [`remove`](#zuno-plugin-remove) | 移除一个已安装的包 |
| `help` | 打印本消息或给定子命令的帮助 |

### zuno plugin list

列出对某一个目录活跃的包。

```sh
zuno plugin list [OPTIONS]
```

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `--dir <DIR>` | 应被检视其项目配置链的目录 | |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | 选择受限 Shell 无法部署时的处理方式。可选值：`deny`、`run-unconfined` | `deny` |
| `--sandbox-backend <BACKEND>` | 为本次调用选择 Shell 执行后端；`native` 不是沙箱隔离。可选值：`auto`、`native` | `auto` |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

### zuno plugin add

安装一个新的本地包。

```sh
zuno plugin add [OPTIONS] <SOURCE>
```

| 参数 | 说明 |
| --- | --- |
| `<SOURCE>` | 包目录，或其 extension.json 清单 |

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `--project` | 安装到所选项目的 `.zuno` 目录之下，而不是全局安装 | |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--dir <DIR>` | 用于选定项目目标的目录 | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | 选择受限 Shell 无法部署时的处理方式。可选值：`deny`、`run-unconfined` | `deny` |
| `--sandbox-backend <BACKEND>` | 为本次调用选择 Shell 执行后端；`native` 不是沙箱隔离。可选值：`auto`、`native` | `auto` |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

### zuno plugin update

以事务方式替换一个已安装的本地包。

```sh
zuno plugin update [OPTIONS] <SOURCE>
```

| 参数 | 说明 |
| --- | --- |
| `<SOURCE>` | 包目录，或其 extension.json 清单 |

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `--project` | 安装到所选项目的 `.zuno` 目录之下，而不是全局安装 | |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--dir <DIR>` | 用于选定项目目标的目录 | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | 选择受限 Shell 无法部署时的处理方式。可选值：`deny`、`run-unconfined` | `deny` |
| `--sandbox-backend <BACKEND>` | 为本次调用选择 Shell 执行后端；`native` 不是沙箱隔离。可选值：`auto`、`native` | `auto` |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

### zuno plugin remove

移除一个已安装的包。

```sh
zuno plugin remove [OPTIONS] <ID>
```

| 参数 | 说明 |
| --- | --- |
| `<ID>` | 稳定的包 id |

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `--project` | 从所选项目的 `.zuno` 目录中移除，而不是从全局移除 | |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--dir <DIR>` | 用于选定项目目标的目录 | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | 选择受限 Shell 无法部署时的处理方式。可选值：`deny`、`run-unconfined` | `deny` |
| `--sandbox-backend <BACKEND>` | 为本次调用选择 Shell 执行后端；`native` 不是沙箱隔离。可选值：`auto`、`native` | `auto` |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

## 示例

报告对当前目录活跃的包。

```sh
zuno plugin list
```

不切换目录，检查另一个 checkout 解析出什么。

```sh
zuno plugin list --dir /srv/projects/api
```

从清单全局安装一个包。

```sh
zuno plugin add ./my-extension/extension.json
```

只为一个项目安装同一个包，编辑之后再就地替换它。

```sh
zuno plugin add --project --dir /srv/projects/api ./my-extension
zuno plugin update --project --dir /srv/projects/api ./my-extension
```

## 参见

- [全局选项](/zh/cli/global-options)
- [zuno mcp](/zh/cli/mcp)
- [zuno debug](/zh/cli/debug)
- [插件](/zh/guide/plugins)
- [配置参考](/zh/config/reference)
