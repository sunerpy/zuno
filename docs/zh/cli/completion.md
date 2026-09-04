# zuno completion

`zuno completion` 根据 Zuno 当前的命令树生成补全。默认把脚本写到 stdout；使用
`--install` 时，Zuno 会把它原子写入当前用户确定的目录，并打印激活说明。命令绝不会
编辑 Shell profile。

## 用法

```sh
zuno completion [OPTIONS] <SHELL>
```

## 参数

| 参数 | 说明 |
| --- | --- |
| `<SHELL>` | 要生成的补全语法。可选值：`bash`、`elvish`、`fish`、`powershell`、`zsh` |

## 选项

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `--install` | 为当前用户安装，而不是把脚本写到 stdout | |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | 选择受限 Shell 无法部署时的处理方式。可选值：`deny`、`run-unconfined` | `deny` |
| `--sandbox-backend <BACKEND>` | 为本次调用选择 Shell 执行后端；`native` 不是沙箱隔离。可选值：`auto`、`native` | `auto` |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

## 安装路径

| Shell | 用户级路径 |
| --- | --- |
| Bash | `$XDG_DATA_HOME/bash-completion/completions/zuno`，通常为 `~/.local/share/bash-completion/completions/zuno` |
| Zsh | `~/.zsh/completions/_zuno` |
| Fish | `$XDG_CONFIG_HOME/fish/completions/zuno.fish`，通常为 `~/.config/fish/completions/zuno.fish` |
| PowerShell | `%LOCALAPPDATA%\zuno\completions\_zuno.ps1`；没有 `LOCALAPPDATA` 时使用 XDG data 路径 |
| Elvish | `$XDG_CONFIG_HOME/elvish/lib/zuno.elv`，通常为 `~/.config/elvish/lib/zuno.elv` |

路径按宿主原生路径处理，Unix 上可以包含非 UTF-8 部分。重复安装只替换补全文件；不会修改
`.bashrc`、`.zshrc`、PowerShell profile 或其他启动文件。

## 示例

只检查或临时 source 生成结果，不安装：

```sh
zuno completion bash
source <(zuno completion bash)
```

安装 zsh 补全，然后在 `compinit` 前把目录加入 `fpath`：

```sh
zuno completion zsh --install
fpath=(~/.zsh/completions $fpath)
autoload -Uz compinit && compinit
```

Fish 会自动发现已安装的用户补全：

```sh
zuno completion fish --install
```

PowerShell 安装会打印确切脚本路径。可在当前会话中 dot-source，或自行写入受管理的 profile：

```powershell
zuno completion powershell --install
. "$env:LOCALAPPDATA\zuno\completions\_zuno.ps1"
```

Elvish：

```sh
zuno completion elvish --install
use zuno
```

## 参见

- [安装](/zh/guide/installation)
- [CLI 参考](/zh/cli/)
- [全局选项](/zh/cli/global-options)
