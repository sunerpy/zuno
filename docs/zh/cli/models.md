# zuno models

`zuno models` 报告通过本机已配置的 Provider 可触达的模型。用它来确认
[`zuno run`](/zh/cli/run) 与 [`zuno tui`](/zh/cli/tui) 期望的那个确切 `provider/model`
标识符，也用它来检查刚认证过的某个 Provider 是否真的暴露了你以为的东西。

模型目录是有缓存的。添加 Provider 之后，或者某个新发布的模型还没出现时，加上 `--refresh`。

## 用法

```sh
zuno models [OPTIONS] [PROVIDER]
```

## 参数

| 参数 | 说明 |
| --- | --- |
| `[PROVIDER]` | |

## 选项

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--verbose` | | |
| `--refresh` | | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | 选择受限 Shell 无法部署时的处理方式。可选值：`deny`、`run-unconfined` | `deny` |
| `--sandbox-backend <BACKEND>` | 为本次调用选择 Shell 执行后端；`native` 不是沙箱隔离。可选值：`auto`、`native` | `auto` |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

## 示例

列出所有已配置 Provider 上可用的每个模型。

```sh
zuno models
```

把列表收窄到一个 Provider。

```sh
zuno models openai
```

当你需要的不只是模型标识符时，显示详细视图。

```sh
zuno models --verbose
```

在认证了新 Provider 之后重新拉取目录，而不是读缓存。

```sh
zuno models --refresh
```

## 参见

- [全局选项](/zh/cli/global-options)
- [zuno providers](/zh/cli/providers)
- [zuno run](/zh/cli/run)
- [Provider 参考](/zh/config/providers)
- [配置参考](/zh/config/reference)
