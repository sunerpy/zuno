# CLI 参考

Zuno 是单个可执行文件。这套 harness 暴露的每一项能力，从一次性提示到持久 session
的检视，都能通过 `zuno` 的某个子命令触达。不带子命令运行 `zuno` 会启动交互式终端应用，
因此 `zuno` 与 `zuno tui` 是等价的。

本参考是针对随发布的二进制文件生成的。每个页面都复现该命令实际接受的选项，以及二进制
文件自己报告的默认值。在 `--help` 中不带默认值的选项，文档中也不写默认值。

```sh
zuno --help
zuno <command> --help
```

有一小部分选项被每个子命令都接受。它们在[全局选项](/zh/cli/global-options)中集中记录一次，
而不是在每个页面上重复说明。

## 运行 Zuno

| 命令 | 用途 |
| --- | --- |
| [`zuno run`](/zh/cli/run) | 以非交互方式把一条消息送入 harness 运行，可选 JSON 输出。 |
| [`zuno tui`](/zh/cli/tui) | 启动交互式终端应用。也是不带命令时的默认行为。 |
| [`zuno serve`](/zh/cli/serve) | 启动无头服务器供外部客户端使用。 |
| [`zuno acp`](/zh/cli/acp) | 在 stdin 与 stdout 上提供 Agent Client Protocol 服务，用于编辑器集成。 |

## 管理状态

| 命令 | 用途 |
| --- | --- |
| [`zuno session`](/zh/cli/session) | 列出、按期清理与删除持久 session。 |
| [`zuno agent`](/zh/cli/agent) | 创建 Agent 定义，并列出当前解析出的 Agent。 |
| [`zuno db`](/zh/cli/db) | 对本地 session 数据库执行查询。 |
| [`zuno export`](/zh/cli/export) | 把配置、Skill、扩展与 Agent 写入一个可移植 bundle。 |
| [`zuno import`](/zh/cli/import) | 把一个可移植 bundle 还原到本机的用户环境中。 |

## Provider 与扩展

| 命令 | 用途 |
| --- | --- |
| [`zuno models`](/zh/cli/models) | 列出通过已配置 Provider 可触达的模型。 |
| [`zuno providers`](/zh/cli/providers) | 检视 Provider 并管理其存储的凭据。 |
| [`zuno mcp`](/zh/cli/mcp) | 注册、认证与调试 Model Context Protocol 服务器。 |
| [`zuno plugin`](/zh/cli/plugin) | 安装、替换、移除与检视扩展包。 |

## 维护

| 命令 | 用途 |
| --- | --- |
| [`zuno self-update`](/zh/cli/self-update) | 用一份经校验和验证的 GitHub release 替换正在运行的可执行文件。 |
| [`zuno debug`](/zh/cli/debug) | 检视路径、解析后的配置、提示、权限、沙箱与快照。 |
| [`zuno completion`](/zh/cli/completion) | 生成或安装 bash、elvish、fish、powershell 或 zsh 的 Shell 补全。 |

## 已排除

| 命令 | 用途 |
| --- | --- |
| [已排除的命令](/zh/cli/excluded) | `console`、`web`、`stats`、`github`、`pr`、`uninstall` 与 `generate` 之所以被注册，只是为了说明有什么替代它们。 |

## 参见

- [全局选项](/zh/cli/global-options)
- [配置参考](/zh/config/reference)
- [Provider 参考](/zh/config/providers)
- [编排](/zh/guide/orchestration)
- [日志](/zh/operate/logging)
- [FAQ](/zh/operate/faq)
