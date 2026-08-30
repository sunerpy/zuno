# zuno acp

`zuno acp` 在 stdin 与 stdout 上说 Agent Client Protocol。支持 ACP 的编辑器会把这个可执行
文件作为子进程启动，并在管道上交换带帧的消息，因此没有端口需要绑定，也没有 HTTP 面需要
加固。这是 Zed 以及其他 ACP 客户端的集成路径。

由于协议占用了 stdout，不要把那条流当作人类可读输出来读。只想确认适配器存在时用 `--check`，
用 `--print-logs` 把诊断信息路由到 stderr，那里不会破坏协议流。

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
