# zuno completion

`zuno completion` 把某一个 Shell 的补全脚本打印到 stdout。它不写磁盘，也不编辑任何 Shell
配置，所以输出去哪儿由你决定：直接为当前 Shell source，或者保存到你的 Shell 已经会加载的
补全目录里。

## 用法

```sh
zuno completion [OPTIONS] <SHELL>
```

## 参数

| 参数 | 说明 |
| --- | --- |
| `<SHELL>` | 应输出其补全语法的 Shell。可选值：`bash`、`elvish`、`fish`、`powershell`、`zsh` |

## 选项

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | 选择受限 Shell 无法部署时的处理方式。可选值：`deny`、`run-unconfined` | `deny` |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

## 示例

把 bash 脚本打印到 stdout，在安装任何东西之前先检查它。

```sh
zuno completion bash
```

只为当前 bash Shell 启用补全，不触碰配置文件。

```sh
source <(zuno completion bash)
```

把 zsh 脚本安装到你的 Shell 已经会加载的补全目录中。

```sh
zuno completion zsh > ~/.zsh/completions/_zuno
```

把 fish 脚本安装到标准的用户补全目录中。

```sh
zuno completion fish > ~/.config/fish/completions/zuno.fish
```

## 参见

- [CLI 参考](/zh/cli/)
- [全局选项](/zh/cli/global-options)
- [FAQ](/zh/operate/faq)
