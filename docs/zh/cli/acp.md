# zuno acp

`zuno acp` 在 stdin 与 stdout 上说 Agent Client Protocol。支持 ACP 的编辑器会把这个可执行
文件作为子进程启动，并在管道上交换带帧的消息，因此没有端口需要绑定，也没有 HTTP 面需要
加固。这是 Zed 以及其他 ACP 客户端的集成路径。

由于协议占用了 stdout，不要把那条流当作人类可读输出来读。只想确认适配器存在时用 `--check`，
用 `--print-logs` 把诊断信息路由到 stderr，那里不会破坏协议流。

## 会话级 MCP server

Zuno 公布标准 ACP MCP 的 stdio 与 Streamable HTTP 支持；旧式 SSE 仍不支持。`session/new`、`session/load` 与 `session/resume` 必须为该会话提供完整的 `mcpServers` 列表。load/resume 绝不会复用上一次请求留下的进程资源。

声明会在会话发布之前完整校验：

- 名称必须满足 `[A-Za-z0-9_-]{1,32}`，否则稳定 slug 化并追加 8 位 digest；规范化后重名会被拒绝；
- stdio command 必须是绝对路径，并以会话目录作为 cwd；
- HTTP endpoint 必须是绝对 HTTP(S) URL；
- environment 与 header 条目严格校验，包括 header 名称大小写无关的重复项。

每个 ACP session 拥有隔离的 profile bundle。全部 server 都必须连接并完成工具发现，工具才会原子发布；部分启动按逆序关闭。session close、load 失败、进程退出与 profile replacement 使用同一条精确 disposer 路径。

客户端 MCP command、environment 值与 HTTP header 只存在于进程内，不写入会话数据库或诊断。工具 schema 与真实工具 attempt 仍遵循普通的持久工具规则。

## 用法

```sh
zuno acp [OPTIONS]
```

## 选项

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `--check` | 验证生产 ACP 适配器可用，然后退出 | |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `--sandbox-on-unavailable <ACTION>` | 选择受限 Shell 无法部署时的处理方式。可选值：`deny`、`run-unconfined` | `deny` |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

## 示例

确认这份构建中生产 ACP 适配器可用，然后退出。

```sh
zuno acp --check
```

在 stdin 与 stdout 上提供协议服务，也就是编辑器启动它的方式。

```sh
zuno acp
```

一边提供协议服务，一边把诊断信息镜像到 stderr，使 stdout 上的协议分帧保持完整。

```sh
zuno acp --print-logs --log-level DEBUG
```

## 参见

- [全局选项](/zh/cli/global-options)
- [zuno serve](/zh/cli/serve)
- [Zed ACP 集成](/zh/guide/editors)
- [日志](/zh/operate/logging)
