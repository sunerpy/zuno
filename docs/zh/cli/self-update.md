# zuno self-update

`zuno self-update` 用来自某个 GitHub release 的构建替换正在运行的可执行文件，并在安装前
对照发布的校验和验证下载内容。当 Zuno 是作为独立二进制文件安装、而不是通过包管理器安装
时，这就是受支持的路径。

该命令会就地覆盖可执行文件。想知道是否存在更新的 release 而不改动任何东西时用 `--check`；
只有当脚本必须在没有交互确认的情况下继续时才用 `--yes`。`--tag` 固定一个确切的 release，
`--force` 会重新安装一个并不更新的 release，这就是回退到某个已知可用版本的做法。

## 用法

```sh
zuno self-update [OPTIONS]
```

## 选项

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `--check` | 报告是否存在更新的 release，但不改动可执行文件 | |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--force` | 即使所选 release 并不更新，也重新安装它 | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--tag <vX.Y.Z>` | 安装一个显式的 release tag，而不是最新 release | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `-y`, `--yes` | 不经交互确认就替换可执行文件 | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | 选择受限 Shell 无法部署时的处理方式。可选值：`deny`、`run-unconfined` | `deny` |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

## 示例

报告是否存在更新的 release，同时不触碰可执行文件。

```sh
zuno self-update --check
```

更新到最新 release，交互式确认。

```sh
zuno self-update
```

不带提示地更新，适用于无人看管的维护任务。

```sh
zuno self-update --yes
```

重新安装一个确切的较早 tag，以回退一次糟糕的更新。

```sh
zuno self-update --tag v0.0.1 --force --yes
```

## 参见

- [全局选项](/zh/cli/global-options)
- [已排除的命令](/zh/cli/excluded)
- [自更新参考](/zh/operate/self-update)
- [迁移](/zh/operate/migration)
