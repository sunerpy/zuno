# zuno serve

`zuno serve` 启动无头服务器，让外部客户端通过 HTTP 而不是终端来驱动 harness。当使用方是
编辑器、GUI、另一个服务，或者一个说服务器 API 而非 CLI 的脚本时，就用它。

服务器 owner 在进程内包装 `zuno_server::ServerBuilder`。它不会派生一个单独的 `zuno-server`
可执行文件，也不会重复实现监听器行为。

绑定到 `0.0.0.0` 或通过 mDNS 广告监听器，都会把它暴露到本机之外。服务器不会代你添加
认证，所以要把绑定地址和 CORS 来源限制到部署真正需要的范围。

## 用法

```sh
zuno serve [OPTIONS]
```

## 选项

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `--port <PORT>` | | `0` |
| `-v`, `--version` | 显示 Zuno 包版本 | |
| `--hostname <HOSTNAME>` | | `127.0.0.1` |
| `--mdns` | | |
| `--print-logs` | 除结构化本地日志存储之外，同时把日志打印到 stderr | |
| `--log-level <LOG_LEVEL>` | 设置最低日志级别。可选值：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR` | |
| `--mdns-domain <MDNS_DOMAIN>` | | `zuno.local` |
| `--cors <CORS>` | | |
| `--sandbox <SANDBOX>` | 为本次调用选择 Shell 约束。可选值：`read-only`、`workspace-write`、`danger-full-access` | |
| `-h`, `--help` | 打印帮助（用 `-h` 查看摘要） | |

## 示例

在回环接口上启动服务器，端口由操作系统分配。

```sh
zuno serve
```

固定端口，好让客户端配置指向一个稳定地址。

```sh
zuno serve --port 4096
```

在排查连接失败的客户端时，在 stderr 上观察服务器自身的日志流。

```sh
zuno serve --port 4096 --print-logs --log-level DEBUG
```

在受信网络上，用自定义域名通过 mDNS 广告监听器以便被发现。

```sh
zuno serve --port 4096 --mdns --mdns-domain zuno.local
```

## 参见

- [全局选项](/zh/cli/global-options)
- [zuno acp](/zh/cli/acp)
- [已排除的命令](/zh/cli/excluded)
- [配置参考](/zh/config/reference)
- [Zed ACP 集成](/zh/guide/editors)
- [日志](/zh/operate/logging)
